extension MergePreparation {
    var targetFilter: ResolvedExpression? {
        guard let view = relation.view else { return nil }
        return QueryEstimate.and(view.filters.map { view.scanExpression($0.expression, level: $0.level) })
    }
    var combinedCondition: ResolvedExpression {
        QueryEstimate.and((targetFilter.map { [$0] } ?? []) + [condition])!
    }
    var accessPredicate: ResolvedExpression {
        QueryEstimate.and((targetFilter.map { [$0] } ?? []) + [probe])!
    }
    func selectAccess(parameters: [SQLValue]?, context: ExpressionContext) throws -> ScanCandidate {
        try selectAccess(predicate: accessPredicate, parameters: parameters, context: context)
    }
    func selectUnmatchedAccess(parameters: [SQLValue]?, context: ExpressionContext) throws -> ScanCandidate {
        try selectAccess(predicate: targetFilter, parameters: parameters, context: context)
    }
    private func selectAccess(predicate: ResolvedExpression?, parameters: [SQLValue]?, context: ExpressionContext) throws -> ScanCandidate {
        let base = BaseScanPlan(predicate: predicate, keys: relation.table.primaryKey, types: relation.types)
        let candidates = try AccessInventory.resolve(table: relation.table, store: relation.store, snapshot: read,
            domain: settings.targetDomain, predicate: predicate, base: base, settings: settings, context: context)
        let estimator = ExplainTree(options: SQLExplain(), parameters: parameters, context: context)
        var winner: ScanCandidate?, minimum = Int64.max
        for candidate in candidates {
            let source = try candidate.estimate(parameters: parameters, context: context)
            let cost = saturatingAdd(source.cost, try estimator.expressionCost(predicate.map { [$0] } ?? [], rows: source.rows))
            if winner == nil || cost < minimum { winner = candidate; minimum = cost }
        }
        guard let winner else { throw JedError("XX000", "MERGE has no target access candidate") }
        return winner
    }
    struct HashKey { let left: Int; let right: Int; let type: TypeDescriptor }
    func hashKeys(access: ScanCandidate, parameters: [SQLValue]?, context: ExpressionContext) throws -> [HashKey]? {
        guard case .full = access.path else { return nil }
        var keys: [HashKey] = []
        func leaf(_ value: ResolvedExpression) -> Bool {
            switch value.node { case .column, .constant: true; default: false }
        }
        for term in QueryEstimate.conjuncts(accessPredicate) {
            guard case .binary(let operation, let left, let right, _) = term.node,
                  ["=", "<>", "!="].contains(operation) else { return nil }
            func permitted(_ value: ResolvedExpression) -> Bool {
                if case .outerColumn(let reference) = value.node { return reference.depth == 1 }
                return leaf(value)
            }
            guard permitted(left), permitted(right) else { return nil }
            guard operation == "=" else { continue }
            let target: Int, source: Int
            if case .column(let column) = left.node, case .outerColumn(let reference) = right.node {
                target = column; source = reference.index
            } else if case .column(let column) = right.node, case .outerColumn(let reference) = left.node {
                target = column; source = reference.index
            } else { continue }
            guard relation.types.indices.contains(target), self.source.projections.indices.contains(source),
                  relation.types[target].type == self.source.projections[source].type?.type,
                  JoinPlan.keyable(relation.types[target].type) else { continue }
            keys.append(HashKey(left: target, right: source, type: relation.types[target]))
        }
        guard !keys.isEmpty else { return nil }
        let source = try SelectEstimate.estimate(plan: self.source, parameters: parameters, context: context,
                                                 useUnregisteredCTEPlans: false)
        let scan = try access.estimate(parameters: parameters, context: context)
        let model = PlanCardinality(table: relation.table, snapshot: read, parameters: parameters, collatedKey: context.collatedKey, planningProbe: context.planningProbe)
        let matches = QueryEstimate.multiply(source.rows, try model.rows(accessPredicate, input: relation.table.rowCount))
        let estimator = ExplainTree(options: SQLExplain(), parameters: parameters, context: context)
        let nested = QueryEstimate.multiply(source.rows, saturatingAdd(scan.cost,
            try estimator.expressionCost([accessPredicate], rows: scan.rows)))
        let width = hashWidth(keys)
        let build = QueryEstimate.multiply(scan.rows, width)
        let probe = saturatingAdd(QueryEstimate.multiply(source.rows, width),
            QueryEstimate.multiply(matches, saturatingAdd(width, Int64(keys.count * 4))))
        let hashed = saturatingAdd(scan.cost, saturatingAdd(build, saturatingAdd(probe,
            try estimator.expressionCost([accessPredicate], rows: matches))))
        return hashed < nested ? keys : nil
    }
    func hashWidth(_ keys: [HashKey]) -> Int64 {
        var width: Int64 = 0
        for key in keys {
            let count: Int64
            if let fixed = ValueCodec.fixedWidth(key.type.type) { count = Int64(fixed) }
            else if let facts = read.catalog.statistics.first(where: { $0.table == relation.table.name && $0.column == key.left }),
                    facts.analyzed > facts.nulls {
                count = max(1, QueryEstimate.scale(facts.widthSum, numerator: 1, denominator: facts.analyzed - facts.nulls))
            } else { count = max(1, PlanCardinality.constant("default_variable_key_bytes")) }
            width = saturatingAdd(width, count)
        }
        return width
    }
}
