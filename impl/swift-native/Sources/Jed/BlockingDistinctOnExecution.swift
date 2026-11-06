/// DISTINCT ON retains one previous key after a complete stable sort. It has
/// no hash-state owner; projected expressions run only for the selected window.
internal final class BlockingDistinctOnExecution {
    private let plan: SelectPlan
    private let context: ExpressionContext
    private let hostBatch: HostBatch?
    private let ahead = HostReadAhead<[SQLValue]>()
    private var output: RowSpoolCursor?
    private var remaining: Int64 = 0
    private var started = false
    init(plan: SelectPlan, context: ExpressionContext) {
        self.plan = plan; self.context = context
        hostBatch = HostBatch(expressions: plan.projections, context: context)
    }
    func next() throws -> [SQLValue]? {
        if !started { started = true; try prepare() }
        guard remaining > 0 else { close(); return nil }
        let row: [SQLValue], at: Int?
        if hostBatch != nil {
            let cap = HostReadAhead<[SQLValue]>.cap(meter: context.meter,
                unit: plan.productionCharge ? 1 : 0, remaining: remaining)
            guard let index = try ahead.next(cap: cap, pull: { try self.output?.pullForReplay() }) else { close(); return nil }
            at = index; row = ahead.rows[index]
            if index == 0 { hostBatch?.reset() }
        } else {
            guard let value = try output?.next() else { close(); return nil }
            row = value; at = nil
        }
        remaining -= 1
        if plan.productionCharge { context.meter.charge(1); try context.meter.guardCost() }
        var current = context; current.column = { row[$0] }; current.hostBatch = hostBatch
        current.hostBatchColumn = { row.indices.contains($0) ? row[$0] : nil }
        if let at {
            try hostBatch?.setRow(at, count: ahead.rows.count, context: current) { source, column in
                let values = self.ahead.rows[source]
                return values.indices.contains(column) ? values[column] : nil
            }
        }
        return try plan.projections.map(current.evaluate)
    }
    private func prepare() throws {
        let source = try BlockingRows.gather(plan, context: context)
        let input = try BlockingRows.filter(source, plan: plan, context: context)
        let sorted = try context.profile(plan.profileIdentity, .sort) {
            try BlockingRows.order(input, plan: plan, context: context)
        }
        let rows = try BlockingRows.distinctOn(sorted, plan: plan, context: context)
        do {
            let skip = min(plan.offset, rows.count)
            remaining = min(rows.count - skip, plan.limit ?? Int64.max)
            output = try rows.takeOutput()
            for _ in 0..<skip { _ = try output?.next() }
        } catch { rows.close(); throw error }
    }
    func close() { ahead.reset(); hostBatch?.reset(); output?.close(); output = nil; remaining = 0 }
    deinit { close() }
}
