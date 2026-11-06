/// Interval estimates use only resident metadata and already-readable sources.
/// Runtime canonicalization is deliberately separate: duplicate estimator leaves
/// retain their OR contributions even when execution merges their encoded keys.
internal struct IntervalEstimate {
    private struct Range {
        let expression: ResolvedExpression?
        let empty: Bool
        let allEqual: Bool
    }
    private struct Member {
        let plan: IntervalPlan.Member
        let parts: [Range]?
        let clip: Range
        let count: Int64
        var empty: Bool { clip.empty || count == 0 }
    }
    private let plan: IntervalPlan
    private let model: PlanCardinality
    private let members: [Member]
    private let fullUniquePoints: Int64?
    private let partial: [ResolvedExpression]

    init(plan: IntervalPlan, table: CatalogTable, snapshot: DomainSnapshot,
         parameters: [SQLValue]?, context: ExpressionContext) throws {
        self.plan = plan
        // Key uniqueness applies to a complete tuple, after its ordered member
        // program. An individual member must not inherit table-PK selectivity.
        var unkeyed = table; unkeyed.primaryKey = []
        let model = PlanCardinality(table: unkeyed, snapshot: snapshot, parameters: parameters,
                                    collatedKey: context.collatedKey, planningProbe: context.planningProbe)
        self.model = model
        var consumed: [Member] = [], product: Int64 = 1
        for member in plan.members {
            let clip = try Self.range(member.clip, member: member, model: model)
            var parts: [Range]?
            if let set = member.set {
                var expanded: [Range] = []
                for disjunct in set {
                    if let array = disjunct.array {
                        for source in Self.elements(array, member: member, model: model, context: context) {
                            let equality = IntervalPlan.Term(operation: "=", source: source,
                                expression: Self.comparison("=", source: source, member: member))
                            expanded.append(try Self.range([equality] + disjunct.terms, member: member, model: model))
                        }
                    } else { expanded.append(try Self.range(disjunct.terms, member: member, model: model)) }
                }
                parts = expanded
            }
            let count = parts.map { Int64($0.reduce(0) { $0 + ($1.empty ? 0 : 1) }) } ?? 1
            let item = Member(plan: member, parts: parts, clip: clip, count: count)
            if item.empty { consumed.append(item); product = 0; break }
            if !consumed.isEmpty, count > 1, QueryEstimate.multiply(product, count) > Int64(IntervalPlan.maximumProbes) { break }
            consumed.append(item); product = QueryEstimate.multiply(product, count)
        }
        self.members = consumed
        let unique = plan.index?.index.unique ?? true
        fullUniquePoints = unique && consumed.count == plan.keyCount && consumed.allSatisfy { $0.plan.point } ? product : nil
        if let predicate = plan.index?.predicate {
            let constrained = Set(plan.members.map(\.column))
            partial = QueryEstimate.conjuncts(predicate).filter { expression in
                var pending = [expression]
                while let item = pending.popLast() {
                    if case .column(let column) = item.node, constrained.contains(column) { return false }
                    pending += StoredExpressionDependencies.children(item)
                }
                return true
            }
        } else { partial = [] }
    }

    private static func comparison(_ operation: String, source: ResolvedExpression,
                                   member: IntervalPlan.Member) -> ResolvedExpression {
        ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)), node: .binary(operation,
            ResolvedExpression(type: member.type, node: .column(member.column), position: 0), source, member.type), position: 0)
    }
    private static func elements(_ array: IntervalPlan.ArraySource, member: IntervalPlan.Member,
                                 model: PlanCardinality, context: ExpressionContext) -> [ResolvedExpression] {
        func literal(_ value: SQLValue) -> ResolvedExpression {
            ResolvedExpression(type: member.type, node: .constant(IntervalPlan.pointValue(value, type: member.type) ?? .null), position: 0)
        }
        func point(_ source: ResolvedExpression) -> ResolvedExpression {
            model.known(source).map(literal) ?? source
        }
        switch array {
        case .elements(let elements): return elements.map(point)
        case .value(let source):
            if let value = model.known(source) {
                if value.isNull { return [] }
                if case .array(let array) = value { return array.elements.map(literal) }
            }
            // Unknown ARRAY operands contribute the shared default element
            // count. The retained source is never evaluated by this program.
            return Array(repeating: source, count: Int(PlanCardinality.constant("default_array_elements")))
        case .subquery(let query):
            if let rows = context.subqueries.values(query) { return rows.map { literal($0[0]) } }
            let unknown = ResolvedExpression(type: member.type, node: .subquery(query), position: 0)
            return Array(repeating: unknown, count: Int(PlanCardinality.constant("default_array_elements")))
        }
    }
    private static func range(_ terms: [IntervalPlan.Term], member: IntervalPlan.Member,
                              model: PlanCardinality) throws -> Range {
        let impossible = Range(expression: nil, empty: true, allEqual: false)
        var equality: IntervalPlan.Term?, lower: IntervalPlan.Term?, upper: IntervalPlan.Term?
        for (ordinal, term) in terms.enumerated() {
            if model.isNull(term.source) { return impossible }
            // Only an authored constant proves width impossibility without a
            // value read. Runtime parameter admission remains in bind().
            if term.operation == "=", case .constant(let value) = term.source.node,
               IntervalPlan.pointValue(value, type: member.type) == nil { return impossible }
            for prior in terms.prefix(ordinal) {
                guard PlanningProbe.literal(prior.source), PlanningProbe.literal(term.source),
                      let a = model.known(prior.source), let b = model.known(term.source) else { continue }
                let order: Int
                do { order = try ValueComparison.total(a, b, type: member.type,
                    collatedKey: model.collatedKey, equality: prior.operation == "=" && term.operation == "=") }
                catch let error as JedError where error.code == "0A000" { continue }
                func satisfies(_ order: Int, _ operation: String) -> Bool {
                    switch operation { case "=": order == 0; case "<": order < 0; case "<=": order <= 0
                    case ">": order > 0; case ">=": order >= 0; default: false }
                }
                if prior.operation == "=", !satisfies(order, term.operation) { return impossible }
                if term.operation == "=", !satisfies(-order, prior.operation) { return impossible }
                if [">", ">="].contains(prior.operation), ["<", "<="].contains(term.operation),
                   order > 0 || order == 0 && (prior.operation == ">" || term.operation == "<") { return impossible }
                if ["<", "<="].contains(prior.operation), [">", ">="].contains(term.operation),
                   order < 0 || order == 0 && (prior.operation == "<" || term.operation == ">") { return impossible }
            }
            if term.operation == "=" { if equality == nil { equality = term } }
            else if [">", ">="].contains(term.operation) { lower = term }
            else { upper = term }
        }
        let selected: [IntervalPlan.Term]
        if let equality { selected = [equality] }
        else { selected = [lower, upper].compactMap { $0 } }
        return Range(expression: QueryEstimate.and(selected.map { comparison($0.operation, source: $0.source, member: member) }),
                     empty: false, allEqual: !terms.isEmpty && terms.allSatisfy { $0.operation == "=" })
    }

    func rows(_ input: Int64) throws -> Int64 {
        guard input > 0, !members.contains(where: \.empty) else { return 0 }
        var result = input
        let uniqueLeading = members.count == 1 && plan.keyCount == 1 && (plan.index?.index.unique ?? true)
        for member in members {
            if let parts = member.parts {
                var sum: Int64 = 0
                for part in parts where !part.empty {
                    let selected = uniqueLeading && part.allEqual ? min(result, 1) : try model.rows(part.expression, input: result)
                    sum = min(result, saturatingAdd(sum, selected))
                }
                result = try model.rows(member.clip.expression, input: sum)
            } else { result = try model.rows(member.clip.expression, input: result) }
        }
        if members.count > 1, let fullUniquePoints { result = min(result, fullUniquePoints) }
        let uniqueAtom = uniqueLeading && members.first?.parts?.count == 1
            && members.first?.parts?.first?.allEqual == true && members.first?.clip.expression == nil
        if !uniqueAtom {
            let structural = PlanCardinality(table: nil, snapshot: nil, parameters: model.parameters, collatedKey: model.collatedKey, planningProbe: model.planningProbe)
            for predicate in partial { result = try structural.rows(predicate, input: result) }
        }
        return result
    }
    func estimate(store: TableStore, index: TableStore?, tableRows: Int64) throws -> PlanEstimate {
        let rows = try rows(tableRows), tree = index?.tree ?? store.tree
        let nodes = Int64(tree.nodeCount), height = Int64(tree.height)
        let pages = rows == 0 ? 0 : min(nodes, max(height, try self.rows(nodes)))
        let fetch = index == nil ? 0 : Int64(store.tree.height)
        // The frozen three-core estimator and shared EXPLAIN fixtures clamp
        // the set's combined selectivity once. Actual execution charges each
        // canonical interval. estimator.md's per-interval estimate prose is a
        // separately tracked baseline discrepancy; do not alter fixture costs.
        let work = CandidateEstimate(kind: index == nil ? .primaryIntervals : .orderedIntervals,
            scanRows: rows, outputRows: rows, accessPages: pages, tableHeight: fetch)
        return PlanEstimate(rows: rows, cost: work.cost,
            logicalRows: tableRows, prefix: .init(scanPages: pages, minimumScanPages: height,
                pointFetchPagesPerRow: fetch, pagePolicy: index == nil ? .clamped : .retained))
    }
}
