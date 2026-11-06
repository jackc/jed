/// Base inventories retain their owning catalog and environment. A join prices
/// every access pair; this helper never selects an input independently.
internal enum RelationAccess {
    /// Fixed steps preserve the established access preference, including the
    /// same-key clipped interval replacement that differs from cost tie order.
    static func legacy(_ candidates: [ScanCandidate]) -> ScanCandidate? {
        if let set = candidates.first(where: { $0.accessRank == 4 }), set.intervals?.replacesContiguous == true { return set }
        if let primary = candidates.first(where: { $0.accessRank == 0 }) { return primary }
        if let ordered = candidates.first(where: { $0.accessRank == 1 }) {
            if let set = candidates.first(where: { $0.accessRank == 5 && $0.indexName == ordered.indexName }),
               set.intervals?.replacesContiguous == true { return set }
            return ordered
        }
        for rank in [2, 3, 4, 5, 6] {
            if let candidate = candidates.first(where: { $0.accessRank == rank }) { return candidate }
        }
        return nil
    }

    static func indexedInventory(_ plan: RelationPlan, on: ResolvedExpression?, whereFilter: ResolvedExpression?,
                                 offset: Int, earlier: Set<Int>, context: ExpressionContext) throws -> [ScanCandidate] {
        func mapped(_ expression: ResolvedExpression?) -> ResolvedExpression? {
            QueryEstimate.and(QueryEstimate.conjuncts(expression).compactMap {
                local($0, offset: offset, width: plan.columns.count, earlier: earlier)
            })
        }
        let own = mapped(on), post = mapped(whereFilter)
        let predicate = QueryEstimate.and(QueryEstimate.conjuncts(own) + QueryEstimate.conjuncts(post))
        return try inventory(plan, predicate: predicate, allowOuter: true, includeScanFilter: false, context: context).filter { candidate in
            guard candidate.usesOuter else { return false }
            guard let required = candidate.orderedDefinition?.predicate else { return true }
            return PredicateImplication.inlAdmissible(on: own, whereFilter: post, predicate: required)
        }
    }

    static func inventory(_ plan: RelationPlan, predicate: ResolvedExpression? = nil,
                          allowOuter: Bool = false, includeScanFilter: Bool = true,
                          context: ExpressionContext) throws -> [ScanCandidate] {
        guard case .base(let store) = plan.body, let table = plan.baseTable,
              let snapshot = plan.sourceSnapshot, let settings = plan.sourceSettings else {
            throw JedError("XX000", "base relation has no access environment")
        }
        var terms = includeScanFilter ? QueryEstimate.conjuncts(plan.scanFilter) : []
        for term in QueryEstimate.conjuncts(predicate) where !terms.contains(where: { ExpressionIdentity.same($0, term) }) {
            terms.append(term)
        }
        let complete = QueryEstimate.and(terms)
        let base = BaseScanPlan(predicate: complete, keys: table.primaryKey, types: store.tree.types)
        return try AccessInventory.resolve(table: table, store: store, snapshot: snapshot,
            domain: plan.sourceDomain ?? DomainNames.main, predicate: complete, base: base,
            settings: settings, context: context, forJoin: true, allowOuter: allowOuter)
    }

    static func estimate(_ candidate: ScanCandidate, relation: RelationPlan, indexed: Bool,
                         parameters: [SQLValue]?, context: ExpressionContext) throws -> PlanEstimate {
        let source = try candidate.estimate(parameters: parameters, context: context)
        guard let filter = relation.scanFilter else { return source }
        var model = PlanCardinality(table: candidate.table, snapshot: candidate.snapshot,
                                    parameters: parameters, collatedKey: context.collatedKey, planningProbe: context.planningProbe)
        model.statisticsPopulation = candidate.table.rowCount
        let logical = try model.rows(filter, input: source.logicalRows)
        let rows = try indexed ? model.rows(filter, input: source.rows) : min(source.rows, logical)
        let tree = ExplainTree(options: SQLExplain(), parameters: parameters, context: context)
        return PlanEstimate(rows: rows,
            cost: saturatingAdd(source.cost, try tree.expressionCost([filter], rows: source.rows)), logicalRows: logical)
    }

    /// Query key expressions retain their local slots. Only bare columns from
    /// the earlier input become legal correlated probe operands.
    static func local(_ input: ResolvedExpression, offset: Int, width: Int,
                      earlier: Range<Int>) -> ResolvedExpression? {
        local(input, offset: offset, width: width, earlier: Set(earlier))
    }
    static func local(_ input: ResolvedExpression, offset: Int, width: Int,
                      earlier: Set<Int>) -> ResolvedExpression? {
        var result = input
        func child(_ value: ResolvedExpression) -> ResolvedExpression? {
            local(value, offset: offset, width: width, earlier: earlier)
        }
        func list(_ values: [ResolvedExpression]) -> [ResolvedExpression]? {
            var result: [ResolvedExpression] = []
            for value in values { guard let mapped = child(value) else { return nil }; result.append(mapped) }
            return result
        }
        switch input.node {
        case .column(let index):
            if index >= offset && index < offset + width { result.node = .column(index - offset) }
            else if earlier.contains(index) { result.node = .outerColumn(OuterColumnReference(depth: 1, index: index)) }
            else { return nil }
        case .constant, .parameter: break
        case .outerColumn:
            // An enclosing-query value is constant for an ordinary input scan.
            // INL mapping introduces a new sibling frame, so it keeps its
            // separate bare-sibling admission instead of reusing this depth.
            guard earlier.isEmpty else { return nil }
        case .binary(let op, let a, let b, let type):
            guard let a = child(a), let b = child(b) else { return nil }; result.node = .binary(op, a, b, type)
        case .convert(let value, let type):
            guard let value = child(value) else { return nil }; result.node = .convert(value, type)
        case .cast(let value, let type, let mode):
            guard let value = child(value) else { return nil }; result.node = .cast(value, type, mode)
        case .unary(let op, let value):
            guard let value = child(value) else { return nil }; result.node = .unary(op, value)
        case .isNull(let value, let negated):
            guard let value = child(value) else { return nil }; result.node = .isNull(value, negated)
        case .distinct(let a, let b, let type, let negated):
            guard let a = child(a), let b = child(b) else { return nil }; result.node = .distinct(a, b, type, negated)
        case .call(var call):
            guard let values = list(call.arguments) else { return nil }; call.arguments = values; result.node = .call(call)
        case .array(let values, let nested):
            guard let values = list(values) else { return nil }; result.node = .array(values, nested: nested)
        case .quantified(let op, let all, let a, let b, let type):
            guard let a = child(a), let b = child(b) else { return nil }; result.node = .quantified(op, all: all, a, b, type)
        case .rangeOperation(let op, let a, let b, let type):
            guard let a = child(a), let b = child(b) else { return nil }; result.node = .rangeOperation(op, a, b, type)
        case .field(let value, let ordinal):
            guard let value = child(value) else { return nil }; result.node = .field(value, ordinal)
        default: return nil
        }
        return result
    }

    static func correlated(_ candidate: ScanCandidate) -> Bool { candidate.usesOuter }
}
