/// Bounded left-deep search retains access identities and authored ON ownership.
/// Physical permutations never change the original row's column positions.
internal struct NWayAccessPlan {
    struct Input {
        let source: RelationPlan
        let offset: Int
        var lateral: Bool { offset > 0 && !source.lateralColumns.isEmpty }
        var movable: Bool {
            if case .base = source.body { return !lateral && source.baseTable != nil }
            return false
        }
    }
    struct Edge {
        let owner: Int
        let kind: String
        let original: ResolvedExpression?
        let residual: ResolvedExpression?
        let dependencies: Set<Int>
    }
    struct Key { let outer: Int; let inner: Int; let type: TypeDescriptor }
    enum Algorithm {
        case indexed, hash([Key]), nested
        var rank: Int { switch self { case .indexed: 0; case .hash: 1; case .nested: 2 } }
    }
    struct Access { let candidate: ScanCandidate?; let indexed: Bool; let onIndex: Int }
    struct Step {
        let input: Int
        let onIndices: [Int]
        let algorithm: Algorithm
        let identity = QueryProfileIdentity()
    }
    struct State {
        let order: [Int]
        let access: [Access]
        let steps: [Step]
        let estimate: PlanEstimate
        let ordered: Bool
    }
    struct Estimate {
        let inputs: [PlanEstimate]
        let steps: [PlanEstimate]
        var total: PlanEstimate { steps.last ?? inputs[0] }
    }
    let source: RelationPlan
    let inputs: [Input]
    let edges: [Edge]
    let winner: State

    static func resolve(_ plan: SelectPlan, parameters: [SQLValue]?,
                        context: ExpressionContext) throws -> Self? {
        guard let source = plan.relation else { return nil }
        var inputs: [Input] = [], edges: [Edge] = [], width = 0
        func leaf(_ relation: RelationPlan) {
            inputs.append(Input(source: relation, offset: width)); width += relation.columns.count
        }
        func flatten(_ relation: RelationPlan) {
            guard case .join(let left, let right, let kind, let on) = relation.body else { leaf(relation); return }
            flatten(left); leaf(right)
            let owner = inputs.count - 1
            var dependencies: Set<Int> = [owner]
            StoredExpressionDependencies.walk(on.map { [$0] } ?? []) {
                if case .column(let column) = $0.node,
                   let owner = inputs.indices.first(where: { column >= inputs[$0].offset && column < inputs[$0].offset + inputs[$0].source.columns.count }) {
                    dependencies.insert(owner)
                }
            }
            edges.append(Edge(owner: owner, kind: kind, original: on, residual: relation.effectiveOn, dependencies: dependencies))
        }
        flatten(source)
        guard inputs.count >= 3 else { return nil }
        // Every non-base input and every outer edge is a fixed authored step.
        // The next INNER/CROSS base run may still form an independent island.
        var segments: [[Int]] = [], ordinal = 0
        func movableEdge(_ right: Int) -> Bool { right == 0 || ["inner", "cross"].contains(edges[right - 1].kind) }
        while ordinal < inputs.count {
            let first = ordinal
            if inputs[ordinal].movable && movableEdge(ordinal) {
                ordinal += 1
                while ordinal < inputs.count && inputs[ordinal].movable && movableEdge(ordinal) { ordinal += 1 }
            } else { ordinal += 1 }
            segments.append(Array(first..<ordinal))
        }
        // A fully fenced chain still owns fixed physical steps, even when no
        // adjacent pair is eligible for cost-based reordering.
        let model = Self.model(source, parameters: parameters, context: context)
        let estimateTree = ExplainTree(options: SQLExplain(), parameters: parameters, context: context,
                                       useUnregisteredCTEPlans: false)
        var ordinary: [[ScanCandidate]] = []
        for input in inputs {
            if case .base = input.source.body {
                let predicate = QueryEstimate.and(QueryEstimate.conjuncts(plan.completePredicate).compactMap {
                    RelationAccess.local($0, offset: input.offset, width: input.source.columns.count, earlier: 0..<0)
                })
                ordinary.append(try RelationAccess.inventory(input.source, predicate: predicate, context: context))
            } else { ordinary.append([]) }
        }
        // Preserve the pre-search fixed choices. A fixed step never acquires
        // a hash alternative merely because an adjacent island was reordered.
        var legacy: [Access] = []
        for ordinal in inputs.indices {
            let input = inputs[ordinal]
            var access = Access(candidate: RelationAccess.legacy(ordinary[ordinal]), indexed: false, onIndex: ordinal - 1)
            if ordinal > 0, input.movable, ["inner", "cross", "left"].contains(edges[ordinal - 1].kind) {
                let candidates = try RelationAccess.indexedInventory(input.source, on: edges[ordinal - 1].original,
                    whereFilter: plan.completePredicate, offset: input.offset, earlier: Set(0..<input.offset), context: context)
                if let first = candidates.first { access = Access(candidate: first, indexed: true, onIndex: ordinal - 1) }
            }
            legacy.append(access)
        }
        func inputEstimate(_ ordinal: Int, _ access: Access) throws -> PlanEstimate {
            try Self.inputEstimate(inputs[ordinal], access: access, parameters: parameters, context: context, tree: estimateTree)
        }
        func driverOrdered(_ ordinal: Int, _ access: ScanCandidate) -> Bool {
            guard plan.limit != nil, plan.aggregation == nil, plan.window == nil, !plan.distinct,
                  plan.distinctOn == nil, !plan.order.isEmpty, [0, 6].contains(access.accessRank),
                  let table = inputs[ordinal].source.baseTable,
                  plan.order.count <= table.primaryKey.count else { return false }
            for (index, key) in plan.order.enumerated() {
                guard !key.descending, case .column(let column) = key.expression.node,
                      column == inputs[ordinal].offset + table.primaryKey[index],
                      let descriptor = inputs[ordinal].source.columns.first(where: { $0.index == table.primaryKey[index] })?.type,
                      (key.type.collation?.description ?? "C") == (descriptor.collation?.description ?? "C") else { return false }
            }
            return true
        }
        func append(_ state: State, input ordinal: Int, access: Access,
                    onIndices: [Int], algorithm: Algorithm, fixed: Bool = false) throws -> State {
            let step = Step(input: ordinal, onIndices: onIndices, algorithm: algorithm)
            let inner = try inputEstimate(ordinal, access)
            let total = try Self.stepEstimate(outer: state.estimate, inner: inner, step: step,
                indexed: access.indexed, inputs: inputs, edges: edges, model: model,
                context: context, parameters: parameters).total
            return State(order: state.order + [ordinal], access: state.access + [access],
                steps: state.steps + [step], estimate: total, ordered: !fixed && state.ordered)
        }
        func expand(_ state: State, island: [Int]) throws -> [State] {
            let present = Set(state.order)
            var earlier: Set<Int> = []
            for index in state.order {
                earlier.formUnion(inputs[index].offset..<(inputs[index].offset + inputs[index].source.columns.count))
            }
            var result: [State] = []
            for ordinal in island where !present.contains(ordinal) {
                let after = present.union([ordinal]), input = inputs[ordinal]
                let ready = edges.indices.filter { edges[$0].original != nil && edges[$0].dependencies.isSubset(of: after) && !edges[$0].dependencies.isSubset(of: present) }
                var probes: [Access] = []
                // A distinct authored tree owns each INL option. WHERE remains
                // its own option rather than being merged with every ON tree.
                for onIndex in ready + [-1] {
                    let candidates = try RelationAccess.indexedInventory(input.source,
                        on: onIndex < 0 ? nil : edges[onIndex].original,
                        whereFilter: onIndex < 0 ? plan.completePredicate : nil,
                        offset: input.offset, earlier: earlier, context: context)
                    for candidate in candidates { probes.append(Access(candidate: candidate, indexed: true, onIndex: onIndex)) }
                }
                probes.sort {
                    let order = Self.accessOrder($0.candidate, $1.candidate)
                    return order == 0 ? $0.onIndex < $1.onIndex : order < 0
                }
                for access in probes {
                    result.append(try append(state, input: ordinal, access: access, onIndices: ready, algorithm: .indexed))
                }
                let keys = Self.hashKeys(ready, input: ordinal, prefix: present, inputs: inputs, edges: edges)
                for candidate in ordinary[ordinal] {
                    let access = Access(candidate: candidate, indexed: false, onIndex: -1)
                    if let keys { result.append(try append(state, input: ordinal, access: access, onIndices: ready, algorithm: .hash(keys))) }
                    result.append(try append(state, input: ordinal, access: access, onIndices: ready, algorithm: .nested))
                }
            }
            return result.sorted(by: Self.less)
        }
        func search(_ island: [Int], prefix: State?) throws -> State? {
            var seeds: [State] = []
            if let prefix { seeds = [prefix] }
            else {
                for index in island {
                    for candidate in ordinary[index] {
                        let access = Access(candidate: candidate, indexed: false, onIndex: -1)
                        seeds.append(State(order: [index], access: [access], steps: [],
                            estimate: try inputEstimate(index, access), ordered: driverOrdered(index, candidate)))
                    }
                }
            }
            guard !seeds.isEmpty else { return nil }
            if Int64(island.count) > PlanCardinality.constant("join_dp_limit") {
                func cheaper(_ a: State, _ b: State) -> Bool {
                    a.estimate.cost == b.estimate.cost ? Self.less(a, b) : a.estimate.cost < b.estimate.cost
                }
                var state = seeds.min(by: cheaper)!
                let target = island.count + (prefix?.order.count ?? 0)
                while state.order.count < target {
                    guard let next = try expand(state, island: island).min(by: cheaper) else { return nil }; state = next
                }
                return state
            }
            let end = 1 << island.count
            var frontiers = Array(repeating: [State](), count: end * 2)
            func bucket(_ state: State) -> Int {
                let mask = island.enumerated().reduce(0) { state.order.contains($1.element) ? $0 | (1 << $1.offset) : $0 }
                return mask * 2 + (state.ordered ? 1 : 0)
            }
            func insert(_ state: State) {
                let index = bucket(state)
                if frontiers[index].contains(where: { Self.dominates($0, state) }) { return }
                frontiers[index].removeAll { Self.dominates(state, $0) }
                frontiers[index].append(state); frontiers[index].sort(by: Self.less)
            }
            for state in seeds { insert(state) }
            for size in (prefix == nil ? 1 : 0)..<island.count {
                for mask in 0..<end where mask.nonzeroBitCount == size {
                    for ordered in 0...1 {
                        let states = frontiers[mask * 2 + ordered]
                        for state in states { for candidate in try expand(state, island: island) { insert(candidate) } }
                    }
                }
            }
            let completed = (frontiers[(end - 1) * 2] + frontiers[(end - 1) * 2 + 1]).sorted(by: Self.less)
            var selected: State?, best: Int64 = .max
            for state in completed {
                var cost = state.estimate.cost
                if state.order.count == inputs.count {
                    let physical = Self(source: source, inputs: inputs, edges: edges, winner: state)
                    var candidate = plan; candidate.chosenNWay = physical
                    candidate.orderedByScan = physical.providesOrder; candidate.descending = false
                    let estimate = try physical.estimate(plan: candidate, parameters: parameters, context: context, tree: estimateTree)
                    cost = try SelectEstimate.estimate(plan: candidate, source: estimate.total, parameters: parameters, context: context,
                        useUnregisteredCTEPlans: false).cost
                }
                if selected == nil || cost < best { selected = state; best = cost }
            }
            return selected
        }
        var state: State?
        for segment in segments {
            if segment.count >= 2 {
                guard let next = try search(segment, prefix: state) else { return nil }; state = next
            } else {
                let ordinal = segment[0], access = legacy[ordinal]
                if let prefix = state {
                    state = try append(prefix, input: ordinal, access: access, onIndices: [ordinal - 1],
                        algorithm: access.indexed ? .indexed : .nested, fixed: true)
                } else {
                    state = State(order: [ordinal], access: [access], steps: [],
                        estimate: try inputEstimate(ordinal, access), ordered: false)
                }
            }
        }
        guard let state else { return nil }
        return Self(source: source, inputs: inputs, edges: edges, winner: state)
    }

    /// Canonical fixed chains retain prefix-width rows. A physical search
    /// installs full-width row slots only when at least one island is movable.
    var hasCostedIsland: Bool {
        var length = 0
        for ordinal in inputs.indices {
            if inputs[ordinal].movable && (ordinal == 0 || ["inner", "cross"].contains(edges[ordinal - 1].kind)) {
                length += 1
                if length >= 2 { return true }
            } else { length = 0 }
        }
        return false
    }

    var providesOrder: Bool {
        guard winner.ordered, let access = winner.access.last else { return false }
        return !access.indexed || (access.candidate?.accessRank ?? 6) <= 3
    }
    private static func model(_ source: RelationPlan, parameters: [SQLValue]?, context: ExpressionContext) -> JoinCardinality {
        guard case .join(let left, let right, _, _) = source.body else { preconditionFailure("join source required") }
        return JoinCardinality(left: left, right: right, parameters: parameters, collatedKey: context.collatedKey, planningProbe: context.planningProbe)
    }
    func cardinality(parameters: [SQLValue]?, context: ExpressionContext) -> JoinCardinality {
        Self.model(source, parameters: parameters, context: context)
    }
    private static func inputEstimate(_ input: Input, access: Access, parameters: [SQLValue]?,
                                      context: ExpressionContext, tree: ExplainTree) throws -> PlanEstimate {
        if let candidate = access.candidate {
            return try RelationAccess.estimate(candidate, relation: input.source,
                indexed: access.indexed, parameters: parameters, context: context)
        }
        let node = try tree.source(input.source, touched: [], offset: 0)
        return PlanEstimate(rows: node.rows, cost: node.cost, logicalRows: node.logicalRows)
    }
    func estimate(plan: SelectPlan, parameters: [SQLValue]?, context: ExpressionContext,
                  tree: ExplainTree? = nil) throws -> Estimate {
        let model = cardinality(parameters: parameters, context: context)
        let tree = tree ?? ExplainTree(options: SQLExplain(), parameters: parameters, context: context,
                                      useUnregisteredCTEPlans: false)
        var children: [PlanEstimate] = [], steps: [PlanEstimate] = []
        for position in winner.order.indices {
            let ordinal = winner.order[position], access = winner.access[position]
            let input = try Self.inputEstimate(inputs[ordinal], access: access, parameters: parameters, context: context, tree: tree)
            if position == 0 { children.append(input); continue }
            let final = position == winner.order.count - 1 && providesOrder
            let value = try Self.stepEstimate(outer: steps.last ?? children[0], inner: input,
                step: winner.steps[position - 1], indexed: access.indexed, inputs: inputs, edges: edges,
                model: model, context: context, parameters: parameters,
                limit: final ? plan.limit : nil, offset: plan.offset, predicate: plan.predicate)
            children.append(value.inner); steps.append(value.total)
        }
        return Estimate(inputs: children, steps: steps)
    }
    private static func stepEstimate(outer: PlanEstimate, inner: PlanEstimate, step: Step, indexed: Bool,
        inputs: [Input], edges: [Edge], model: JoinCardinality, context: ExpressionContext,
        parameters: [SQLValue]?, limit: Int64? = nil, offset: Int64 = 0,
        predicate: ResolvedExpression? = nil) throws -> (inner: PlanEstimate, total: PlanEstimate) {
        let pairs = QueryEstimate.multiply(outer.rows, inner.rows)
        var rows = pairs
        // The frozen N-way schedule uses bound pairs as its logical population;
        // this differs from the separate two-relation estimator's rule.
        var logical = indexed ? pairs : QueryEstimate.multiply(outer.logicalRows, inner.logicalRows)
        if !indexed {
            for index in step.onIndices {
                rows = try model.rows(edges[index].residual, input: rows)
                logical = try model.rows(edges[index].residual, input: logical)
            }
        }
        let kind = stepKind(step, edges: edges)
        if kind == "left" || kind == "full" {
            rows = max(rows, outer.rows); logical = max(logical, outer.logicalRows)
        }
        if kind == "right" || kind == "full" {
            rows = max(rows, inner.rows); logical = max(logical, inner.logicalRows)
        }
        var calls = outer.rows
        if let limit {
            let target = saturatingAdd(limit, offset), filtered = try model.rows(predicate, input: rows)
            if target == 0 { calls = 0; rows = 0 }
            else if filtered > target, rows > 0 {
                calls = min(outer.rows, QueryEstimate.scale(target, numerator: outer.rows, denominator: filtered))
                rows = min(rows, QueryEstimate.scale(calls, numerator: rows, denominator: max(1, outer.rows)))
            }
        }
        let repeated = indexed || inputs[step.input].lateral
        let delivered = repeated ? PlanEstimate(rows: QueryEstimate.multiply(inner.rows, calls),
            cost: QueryEstimate.multiply(inner.cost, calls), logicalRows: QueryEstimate.multiply(inner.logicalRows, calls)) : inner
        var work = repeated ? delivered.rows : QueryEstimate.scale(calls, numerator: pairs, denominator: max(1, outer.rows))
        var cost = saturatingAdd(outer.cost, delivered.cost)
        if case .hash(let keys) = step.algorithm {
            var build: Int64 = 0, probe: Int64 = 0, framed: Int64 = 0
            for key in keys {
                let input = inputs[step.input]
                let right = width(key.inner, type: key.type, relation: input.source)
                let owner = inputs.first { key.outer >= $0.offset && key.outer < $0.offset + $0.source.columns.count }!
                let left = width(key.outer - owner.offset, type: key.type, relation: owner.source)
                build = saturatingAdd(build, right); probe = saturatingAdd(probe, left)
                framed = saturatingAdd(framed, saturatingAdd(4, min(left, right)))
            }
            cost = saturatingAdd(cost, saturatingAdd(QueryEstimate.multiply(inner.rows, build),
                saturatingAdd(QueryEstimate.multiply(calls, probe), QueryEstimate.multiply(rows, framed))))
            work = rows
        }
        let tree = ExplainTree(options: SQLExplain(), parameters: parameters, context: context)
        cost = saturatingAdd(cost, try tree.expressionCost(step.onIndices.compactMap { edges[$0].residual }, rows: work))
        return (delivered, PlanEstimate(rows: rows, cost: cost, logicalRows: logical))
    }
    static func stepKind(_ step: Step, edges: [Edge]) -> String {
        for index in step.onIndices where ["left", "right", "full"].contains(edges[index].kind) { return edges[index].kind }
        return step.onIndices.isEmpty ? "cross" : "inner"
    }
    private static func width(_ column: Int, type: TypeDescriptor, relation: RelationPlan) -> Int64 {
        if let width = ValueCodec.fixedWidth(type.type) { return Int64(width) }
        if let table = relation.baseTable, let facts = relation.sourceSnapshot?.catalog.statistics.first(where: {
            $0.table == table.name && $0.column == column
        }), facts.analyzed > facts.nulls {
            return max(1, QueryEstimate.scale(facts.widthSum, numerator: 1, denominator: facts.analyzed - facts.nulls))
        }
        return max(1, PlanCardinality.constant("default_variable_key_bytes"))
    }
    private static func hashKeys(_ indices: [Int], input ordinal: Int, prefix: Set<Int>,
                                 inputs: [Input], edges: [Edge]) -> [Key]? {
        let input = inputs[ordinal]
        func owner(_ column: Int) -> Int? {
            inputs.indices.first { column >= inputs[$0].offset && column < inputs[$0].offset + inputs[$0].source.columns.count }
        }
        func leaf(_ value: ResolvedExpression) -> Bool {
            switch value.node {
            case .column: return true
            case .constant(let value):
                if case .composite = value { return false }
                return true
            default: return false
            }
        }
        var result: [Key] = []
        for index in indices {
            guard let predicate = edges[index].original else { return nil }
            for term in QueryEstimate.conjuncts(predicate) {
                guard case .binary(let operation, let a, let b, _) = term.node,
                      ["=", "<>", "!="].contains(operation), leaf(a), leaf(b) else { return nil }
                guard operation == "=", case .column(let x) = a.node, case .column(let y) = b.node else { continue }
                let inner: Int, outer: Int
                if owner(x) == ordinal, let other = owner(y), prefix.contains(other) { inner = x; outer = y }
                else if owner(y) == ordinal, let other = owner(x), prefix.contains(other) { inner = y; outer = x }
                else { continue }
                guard a.type?.type == b.type?.type, let type = a.type, JoinPlan.keyable(type.type) else { continue }
                result.append(Key(outer: outer, inner: inner - input.offset, type: type))
            }
        }
        return result.isEmpty ? nil : result
    }
    private static func accessOrder(_ a: ScanCandidate?, _ b: ScanCandidate?) -> Int {
        let ar = a?.accessRank ?? 6, br = b?.accessRank ?? 6
        if ar != br { return ar < br ? -1 : 1 }
        let x = a?.indexName ?? SQLText(validatedUTF8: []), y = b?.indexName ?? SQLText(validatedUTF8: [])
        return x == y ? 0 : x < y ? -1 : 1
    }
    private static func less(_ a: State, _ b: State) -> Bool {
        if a.order != b.order { return a.order.lexicographicallyPrecedes(b.order) }
        for (x, y) in zip(a.access, b.access) {
            let order = accessOrder(x.candidate, y.candidate)
            if order != 0 { return order < 0 }
            if x.indexed, y.indexed, x.onIndex != y.onIndex { return x.onIndex < y.onIndex }
        }
        for (x, y) in zip(a.steps, b.steps) {
            if x.algorithm.rank != y.algorithm.rank { return x.algorithm.rank < y.algorithm.rank }
            if x.onIndices != y.onIndices { return x.onIndices.lexicographicallyPrecedes(y.onIndices) }
        }
        return false
    }
    private static func dominates(_ a: State, _ b: State) -> Bool {
        guard a.estimate.cost <= b.estimate.cost, a.estimate.rows <= b.estimate.rows,
              a.estimate.logicalRows <= b.estimate.logicalRows else { return false }
        return a.estimate.cost < b.estimate.cost || a.estimate.rows < b.estimate.rows
            || a.estimate.logicalRows < b.estimate.logicalRows || !less(b, a)
    }
}
