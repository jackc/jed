internal struct PlanEstimate {
    struct ScanPrefix {
        enum PagePolicy { case clamped, retained }
        let scanPages: Int64
        let minimumScanPages: Int64
        let pointFetchPagesPerRow: Int64
        let pagePolicy: PagePolicy
        init(scanPages: Int64, minimumScanPages: Int64, pointFetchPagesPerRow: Int64,
             pagePolicy: PagePolicy = .clamped) {
            self.scanPages = scanPages; self.minimumScanPages = minimumScanPages
            self.pointFetchPagesPerRow = pointFetchPagesPerRow; self.pagePolicy = pagePolicy
        }
        func pages(pulling rows: Int64) -> Int64 {
            switch pagePolicy {
            case .clamped: return rows == 0 ? 0 : min(scanPages, max(minimumScanPages, rows))
            case .retained: return scanPages
            }
        }
    }
    let rows: Int64
    let logicalRows: Int64
    let cost: Int64
    let prefix: ScanPrefix?
    init(rows: Int64, cost: Int64, logicalRows: Int64? = nil, prefix: ScanPrefix? = nil) {
        self.rows = rows; self.logicalRows = logicalRows ?? rows; self.cost = cost; self.prefix = prefix
    }
}

/// A supplied physical scan candidate follows the same complete query pipeline
/// estimate as EXPLAIN. Its override belongs only to this frame; child queries
/// retain their own source estimates. Estimation never opens a storage cursor.
internal enum SelectEstimate {
    /// Invert the same ceiling-rounded selectivity program used by the residual.
    /// Scaling by its full-input output loses the first qualifying ceiling step.
    static func requiredInput(model: PlanCardinality, predicate: ResolvedExpression?,
                              target: Int64, maximum: Int64) throws -> Int64 {
        guard target > 0, maximum > 0 else { return 0 }
        guard let predicate else { return min(target, maximum) }
        var model = model
        model.statisticsPopulation = model.table?.rowCount
        if try model.rows(predicate, input: maximum) < target { return maximum }
        var lower: Int64 = 0, upper = maximum
        while lower < upper {
            let middle = lower + (upper - lower) / 2
            if try model.rows(predicate, input: middle) >= target { upper = middle }
            else { lower = middle + 1 }
        }
        return lower
    }
    static func estimate(plan: SelectPlan, source: PlanEstimate? = nil,
                         parameters: [SQLValue]? = nil, context: ExpressionContext,
                         useUnregisteredCTEPlans: Bool = true) throws -> PlanEstimate {
        let tree = ExplainTree(options: SQLExplain(), parameters: parameters, context: context,
                               useUnregisteredCTEPlans: useUnregisteredCTEPlans)
        let root = try tree.query(plan, source: source)
        return PlanEstimate(rows: root.rows, cost: root.cost, logicalRows: root.logicalRows)
    }
}
