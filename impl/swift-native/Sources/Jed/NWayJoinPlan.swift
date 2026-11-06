/// The ordinary base-table inventory is searched independently of logical
/// column positions. Entire authored ON trees move to their first complete
/// dependency set; their internal evaluation order is never rewritten.
internal struct NWayJoinPlan {
    struct Input { let source: RelationPlan; let offset: Int }
    struct Edge { let owner: Int; let predicate: ResolvedExpression; let residual: ResolvedExpression?; let dependencies: Set<Int> }
    struct Key { let outer: Int; let inner: Int; let type: TypeDescriptor }
    enum Algorithm { case indexed(BaseScanPlan), hash([Key]), nested }
    struct Step { let input: Int; let predicates: [ResolvedExpression]; let algorithm: Algorithm }
    struct State {
        let order: [Int]
        let steps: [Step]
        let cost: Int64
        let rows: Int64
        let logicalRows: Int64
        var members: Set<Int> { Set(order) }
    }
    let inputs: [Input]
    let winner: State
    let width: Int
    let predicates: [ResolvedExpression]

    static func resolve(_ source: RelationPlan) -> Self? {
        var inputs: [Input] = [], authored: [(Int, ResolvedExpression, ResolvedExpression?)] = []
        func flatten(_ plan: RelationPlan) -> Bool {
            guard plan.lateralColumns.isEmpty else { return false }
            if case .base = plan.body, plan.baseTable != nil {
                inputs.append(Input(source: plan, offset: inputs.reduce(0) { $0 + $1.source.columns.count })); return true
            }
            guard case .join(let left, let right, let kind, let predicate) = plan.body,
                  kind == "inner" || kind == "cross", case .base = right.body,
                  flatten(left), flatten(right) else { return false }
            if let predicate { authored.append((inputs.count - 1, predicate, plan.effectiveOn)) }; return true
        }
        guard flatten(source), inputs.count >= 3 else { return nil }
        func owner(_ column: Int) -> Int? {
            inputs.indices.first { column >= inputs[$0].offset && column < inputs[$0].offset + inputs[$0].source.columns.count }
        }
        var edges: [Edge] = []
        for (index, predicate, residual) in authored {
            var dependencies: Set<Int> = [index]
            StoredExpressionDependencies.walk([predicate]) { if case .column(let column) = $0.node, let input = owner(column) { dependencies.insert(input) } }
            edges.append(Edge(owner: index, predicate: predicate, residual: residual, dependencies: dependencies))
        }
        func rowCount(_ index: Int) -> Int64 { QueryEstimate.rows(inputs[index].source.scanFilter, input: inputs[index].source.baseTable!.rowCount) }
        func scanCost(_ index: Int) -> Int64 {
            guard case .base(let store) = inputs[index].source.body else { return 0 }
            let count = inputs[index].source.baseTable!.rowCount
            return saturatingAdd(Int64(store.tree.nodeCount), QueryEstimate.multiply(count,
                saturatingAdd(1, QueryEstimate.expression(inputs[index].source.scanFilter))))
        }
        func less(_ a: State, _ b: State) -> Bool {
            if a.cost != b.cost { return a.cost < b.cost }
            if a.order != b.order { return a.order.lexicographicallyPrecedes(b.order) }
            func rank(_ algorithm: Algorithm) -> Int { switch algorithm { case .indexed: 0; case .hash: 1; case .nested: 2 } }
            return a.steps.map { rank($0.algorithm) }.lexicographicallyPrecedes(b.steps.map { rank($0.algorithm) })
        }
        func extend(_ state: State, _ index: Int) -> [State] {
            let members = state.members.union([index])
            let readyEdges = edges.filter { $0.dependencies.isSubset(of: members) && (state.order.count == 1 || !$0.dependencies.isSubset(of: state.members)) }
            let ready = readyEdges.map(\.predicate), evaluated = readyEdges.compactMap(\.residual)
            let input = inputs[index], count = rowCount(index), product = QueryEstimate.multiply(state.rows, count)
            let residual = evaluated.reduce(Int64(0)) { saturatingAdd($0, QueryEstimate.expression($1)) }
            let rows = evaluated.reduce(product) { QueryEstimate.rows($1, input: $0) }
            let logical = evaluated.reduce(QueryEstimate.multiply(state.logicalRows, count)) { QueryEstimate.rows($1, input: $0) }
            var alternatives: [(Algorithm, Int64, Int64, Int64)] = []
            if input.source.allowsIndexAccess, case .base(let store) = input.source.body,
               let bound = JoinPlan.indexed(QueryEstimate.and(ready), table: input.source.baseTable!, types: store.tree.types,
                   innerOffset: input.offset, innerWidth: input.source.columns.count) {
                let table = input.source.baseTable!
                var selected = table.rowCount, pages = Int64(store.tree.nodeCount)
                let unique = table.primaryKey.allSatisfy { bound.terms[$0]?.contains(where: { $0.operation == "=" }) ?? false }
                if unique { selected = min(1, selected); pages = selected == 0 ? 0 : Int64(store.tree.height) }
                else {
                    for column in table.primaryKey {
                        guard let terms = bound.terms[column], !terms.isEmpty else { break }
                        let equality = terms.contains { $0.operation == "=" }
                        selected = QueryEstimate.fraction(equality ? "equality" : "inequality", rows: selected)
                        pages = QueryEstimate.fraction(equality ? "equality" : "inequality", rows: pages)
                        if !equality { break }
                    }
                    pages = selected == 0 ? 0 : min(Int64(store.tree.nodeCount), max(pages, Int64(store.tree.height)))
                }
                let delivered = QueryEstimate.rows(input.source.scanFilter, input: selected)
                let inputCost = saturatingAdd(pages, QueryEstimate.multiply(selected,
                    saturatingAdd(1, QueryEstimate.expression(input.source.scanFilter))))
                let each = saturatingAdd(inputCost, QueryEstimate.multiply(delivered, residual))
                let pairs = QueryEstimate.multiply(state.rows, delivered)
                // The repeated access has already applied the join bounds. Its
                // physical pairs are also the logical population for this step.
                alternatives.append((.indexed(bound), QueryEstimate.multiply(state.rows, each), pairs, pairs))
            }
            if let keys = hashKeys(ready, input: input, prefix: state.members, owner: owner, inputs: inputs) {
                let width = keys.reduce(Int64(0)) { saturatingAdd($0, Int64(max(1, ValueCodec.fixedWidth($1.type.type) ?? 1))) }
                let framed = saturatingAdd(width, Int64(keys.count * 4))
                let hashes = QueryEstimate.multiply(saturatingAdd(state.rows, count), width)
                let checks = QueryEstimate.multiply(rows, saturatingAdd(framed, residual))
                alternatives.append((.hash(keys), saturatingAdd(scanCost(index), saturatingAdd(hashes, checks)), rows, logical))
            }
            alternatives.append((.nested, saturatingAdd(scanCost(index), QueryEstimate.multiply(product, residual)), rows, logical))
            return alternatives.map { algorithm, cost, rows, logicalRows in
                State(order: state.order + [index], steps: state.steps + [Step(input: index, predicates: evaluated, algorithm: algorithm)],
                      cost: saturatingAdd(state.cost, cost), rows: rows, logicalRows: logicalRows)
            }
        }
        let seeds = inputs.indices.map { State(order: [$0], steps: [], cost: scanCost($0), rows: rowCount($0), logicalRows: rowCount($0)) }
        let winner: State
        guard case .integer(let limit) = GeneratedSpec.estimator["join_dp_limit"] else { preconditionFailure("missing join DP limit") }
        if Int64(inputs.count) > limit {
            var current = seeds.min(by: less)!
            while current.order.count < inputs.count {
                current = inputs.indices.filter { !current.members.contains($0) }.flatMap { extend(current, $0) }.min(by: less)!
            }
            winner = current
        } else {
            var states = seeds
            for _ in 1..<inputs.count {
                var buckets: [Int: [State]] = [:]
                for state in states.sorted(by: less) {
                    for index in inputs.indices where !state.members.contains(index) {
                        for candidate in extend(state, index) {
                            let mask = candidate.order.reduce(0) { $0 | (1 << $1) }
                            var frontier = buckets[mask] ?? []
                            func dominates(_ a: State, _ b: State) -> Bool {
                                guard a.cost <= b.cost, a.rows <= b.rows, a.logicalRows <= b.logicalRows else { return false }
                                return a.cost < b.cost || a.rows < b.rows || a.logicalRows < b.logicalRows || !less(b, a)
                            }
                            if frontier.contains(where: { dominates($0, candidate) }) { continue }
                            frontier.removeAll { dominates(candidate, $0) }
                            frontier.append(candidate); buckets[mask] = frontier
                        }
                    }
                }
                states = buckets.keys.sorted().flatMap { buckets[$0]!.sorted(by: less) }
            }
            winner = states.min(by: less)!
        }
        return Self(inputs: inputs, winner: winner, width: source.columns.count, predicates: authored.map(\.1))
    }
    private static func hashKeys(_ predicates: [ResolvedExpression], input: Input, prefix: Set<Int>,
                                 owner: (Int) -> Int?, inputs: [Input]) -> [Key]? {
        var result: [Key] = []
        for predicate in predicates {
            for term in QueryEstimate.conjuncts(predicate) {
                guard case .binary(let operation, let a, let b, _) = term.node,
                      ["=", "<>", "<", "<=", ">", ">="].contains(operation),
                      case .column(let x) = a.node, case .column(let y) = b.node else { return nil }
                guard operation == "=" else { continue }
                let inner: Int, outer: Int
                if x >= input.offset && x < input.offset + input.source.columns.count { inner = x; outer = y }
                else if y >= input.offset && y < input.offset + input.source.columns.count { inner = y; outer = x }
                else { continue }
                guard let source = owner(outer), prefix.contains(source), a.type?.type == b.type?.type, let type = a.type,
                      JoinPlan.keyable(type.type) else { return nil }
                result.append(Key(outer: outer, inner: inner - input.offset, type: type))
            }
        }
        return result.isEmpty ? nil : result
    }
}
