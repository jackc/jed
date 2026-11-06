/// Plain bounded joins keep their final spool alive until the cursor closes.
/// Only an ordered final join step may stop before producing the complete join.
internal final class BlockingSelectExecution {
    private let plan: SelectPlan
    private let context: ExpressionContext
    private let hostBatch: HostBatch?
    private let ahead = HostReadAhead<[SQLValue]>()
    private var output: RowSpoolCursor?
    private var remaining: Int64 = 0
    private var initialized = false
    private var projected = false
    init(plan: SelectPlan, context: ExpressionContext) {
        self.plan = plan; self.context = context
        hostBatch = HostBatch(expressions: plan.projections, context: context)
    }
    func next() throws -> [SQLValue]? {
        if !initialized { initialized = true; try prepare() }
        guard remaining > 0 else { close(); return nil }
        let row: [SQLValue], at: Int?
        if hostBatch != nil && !projected {
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
        if projected { return row }
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
        projected = plan.orderedByScan && plan.limit != nil && !plan.order.isEmpty
        guard var rows = try BlockingJoinRows.gather(plan, context: context, projected: projected) else {
            throw JedError("XX000", "bounded join has no selected source")
        }
        do {
            if !projected {
                rows = try BlockingRows.filter(rows, plan: plan, context: context)
                if !plan.order.isEmpty {
                    rows = try context.profile(plan.profileIdentity, .sort) {
                        try BlockingRows.order(rows, plan: plan, context: context)
                    }
                }
            }
            let skipped = projected ? 0 : min(plan.offset, rows.count)
            remaining = min(rows.count - skipped, projected ? Int64.max : plan.limit ?? Int64.max)
            output = try rows.takeOutput()
            for _ in 0..<skipped { _ = try output?.next() }
        } catch { rows.close(); throw error }
    }
    func close() { ahead.reset(); hostBatch?.reset(); output?.close(); output = nil; remaining = 0 }
    deinit { close() }
}
