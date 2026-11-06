/// Window state remains an eager Q1 owner. Its input spool overlaps the entire
/// window stage; the stage releases row ownership before filling its output.
internal enum BlockingOutputLayout {
    static func slot(_ index: Int, plan: SelectPlan) -> Int? {
        let physical = plan.window?.slot(index) ?? index
        if let aggregate = plan.aggregation { return BlockingAggregate.columnSlot(physical, aggregate: aggregate) }
        return physical >= 0 ? physical : nil
    }
    static func context(_ row: [SQLValue], plan: SelectPlan, parent: ExpressionContext) -> ExpressionContext {
        var result = parent
        result.column = { index in
            guard let at = slot(index, plan: plan), row.indices.contains(at) else { throw JedError("XX000", "invalid bounded output column") }
            return row[at]
        }
        result.hostBatchColumn = { index in
            guard let at = slot(index, plan: plan), row.indices.contains(at) else { return nil }
            return row[at]
        }
        return result
    }
    static func expanded(_ row: [SQLValue], aggregate: AggregateQueryPlan?) -> [SQLValue] {
        guard let aggregate else { return row }
        var expanded = Array(repeating: SQLValue.null, count: aggregate.layoutWidth)
        for (position, slot) in aggregate.keyColumns.enumerated() { expanded[slot] = row[position] }
        expanded.append(contentsOf: row.dropFirst(aggregate.keys.count)); return expanded
    }
    static func compact(_ row: [SQLValue], aggregate: AggregateQueryPlan?) -> [SQLValue] {
        guard let aggregate else { return row }
        return aggregate.keyColumns.map { row[$0] } + row.dropFirst(aggregate.layoutWidth)
    }
}

internal final class BlockingWindowExecution {
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
        var current = BlockingOutputLayout.context(row, plan: plan, parent: context); current.hostBatch = hostBatch
        if let at {
            try hostBatch?.setRow(at, count: ahead.rows.count, context: current) { source, column in
                let values = self.ahead.rows[source]
                guard let slot = BlockingOutputLayout.slot(column, plan: self.plan), values.indices.contains(slot) else { return nil }
                return values[slot]
            }
        }
        return try plan.projections.map(current.evaluate)
    }
    private func prepare() throws {
        let source = try BlockingRows.gather(plan, context: context)
        var rows = try BlockingRows.filter(source, plan: plan, context: context)
        var transferred = false
        defer { if !transferred { rows.close() } }
        if let aggregate = plan.aggregation {
            rows = try context.profile(plan.profileIdentity, .aggregate, excluding: [.source, .filter, .sort]) {
                let groups = try BlockingAggregate.build(rows, aggregate: aggregate, context: context)
                return try BlockingAggregate.having(groups, aggregate: aggregate, context: context)
            }
        }
        rows = try WindowSelectExecution.boundedStage(rows, plan: plan, context: context)
        if !plan.order.isEmpty {
            rows = try context.profile(plan.profileIdentity, .sort) {
                try BlockingRows.order(rows, plan: plan, context: context,
                    rowContext: { BlockingOutputLayout.context($0, plan: self.plan, parent: self.context) },
                    columnSlot: { BlockingOutputLayout.slot($0, plan: self.plan) })
            }
        }
        rows = try BlockingRows.distinctOn(rows, plan: plan, context: context)
        let skip = min(plan.offset, rows.count)
        remaining = min(rows.count - skip, plan.limit ?? Int64.max)
        output = try rows.takeOutput(); transferred = true
        for _ in 0..<skip { _ = try output?.next() }
    }
    func close() { ahead.reset(); hostBatch?.reset(); output?.close(); output = nil; remaining = 0 }
    deinit { close() }
}
