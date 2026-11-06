extension MutationExecutor.MutationPreparation {
    /// Target selection prices the source and complete residual predicate.
    /// UPDATE/DELETE do not add a result projection or row-production estimate.
    func selectAccess(parameters: [SQLValue]?, context: ExpressionContext) throws -> ScanCandidate {
        let candidates = try AccessInventory.resolve(table: relation.table, store: relation.store,
            snapshot: read, domain: settings.targetDomain, predicate: accessPredicate, base: scan,
            settings: settings, context: context)
        let estimator = ExplainTree(options: SQLExplain(), parameters: parameters, context: context)
        var winner: ScanCandidate?, minimum = Int64.max
        for candidate in candidates {
            let source = try candidate.estimate(parameters: parameters, context: context)
            let residual = try estimator.expressionCost(accessPredicate.map { [$0] } ?? [], rows: source.rows)
            let cost = saturatingAdd(source.cost, residual)
            if winner == nil || cost < minimum { winner = candidate; minimum = cost }
        }
        guard let winner else { throw JedError("XX000", "mutation has no physical scan candidate") }
        return winner
    }
}

/// FK actions retain their specialized gather. Ordinary DML uses the same
/// bounded physical cursor as SELECT and profiles its point reads as source work.
internal enum MutationScanCursor {
    case foreign(ForeignKeyActions.Gather)
    case access(AccessCursor)
    mutating func next() throws -> PackedRecord? {
        switch self {
        case .foreign(var gather):
            defer { self = .foreign(gather) }
            return try gather.next()
        case .access(let cursor): return try cursor.next()
        }
    }
    mutating func close() {
        switch self {
        case .foreign(var gather): gather.close(); self = .foreign(gather)
        case .access(let cursor): cursor.close()
        }
    }
}
