internal struct PlanEstimate {
    struct ScanPrefix {
        let scanPages: Int64
        let minimumScanPages: Int64
        let pointFetchPagesPerRow: Int64
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
    static func estimate(plan: SelectPlan, source: PlanEstimate? = nil,
                         parameters: [SQLValue]? = nil, context: ExpressionContext) throws -> PlanEstimate {
        let tree = ExplainTree(options: SQLExplain(), parameters: parameters, context: context)
        let root = try tree.query(plan, source: source)
        return PlanEstimate(rows: root.rows, cost: root.cost, logicalRows: root.logicalRows)
    }
}
