/// The bounded DISTINCT lane retains stage order independently of disk
/// placement: source, WHERE, ORDER BY, projection/keying, then the row window.
internal final class BlockingDistinctExecution {
    private let plan: SelectPlan
    private let context: ExpressionContext
    private var output: RowSpoolCursor?
    private var remaining: Int64 = 0
    private var started = false
    init(plan: SelectPlan, context: ExpressionContext) { self.plan = plan; self.context = context }

    func next() throws -> [SQLValue]? {
        if !started {
            started = true
            try context.profile(plan.profileIdentity, .distinct,
                excluding: [.source, .folded, .filter, .aggregate, .window, .sort]) { try prepare() }
        }
        guard remaining > 0 else { close(); return nil }
        guard let row = try output?.next() else { close(); return nil }
        remaining -= 1
        if plan.productionCharge { context.meter.charge(1); try context.meter.guardCost() }
        return row
    }

    private func prepare() throws {
        let config = try BlockingRows.configuration(context)
        let source = try BlockingRows.gather(plan, context: context)
        var input = try BlockingRows.filter(source, plan: plan, context: context)
        if let aggregate = plan.aggregation {
            input = try context.profile(plan.profileIdentity, .aggregate, excluding: [.source, .filter, .sort]) {
                let groups = try BlockingAggregate.build(input, aggregate: aggregate, context: context)
                return try BlockingAggregate.having(groups, aggregate: aggregate, context: context)
            }
        }
        if plan.window != nil { input = try WindowSelectExecution.boundedStage(input, plan: plan, context: context) }
        func rowContext(_ row: [SQLValue]) -> ExpressionContext {
            BlockingOutputLayout.context(row, plan: plan, parent: context)
        }
        if !plan.order.isEmpty {
            input = try context.profile(plan.profileIdentity, .sort) {
                try BlockingRows.order(input, plan: plan, context: context, rowContext: rowContext,
                    columnSlot: { index in
                        return BlockingOutputLayout.slot(index, plan: self.plan)
                    })
            }
        }
        defer { input.close() }
        let seen = SpillStateMap(config: config)
        defer { seen.close() }
        let distinct = RowSpool(config: config)
        do {
            let cursor = try input.cursor()
            do {
                defer { cursor.close() }
                while let row = try cursor.next() {
                    try context.meter.pollStep()
                    let current = rowContext(row)
                    let projected = try plan.projections.map(current.evaluate)
                    for value in projected {
                        let bytes = MemorySizes.payload(value)
                        if bytes > 0 { context.meter.charge(bytes); try context.meter.guardCost() }
                    }
                    if try seen.insert(projected) { try distinct.append(projected) }
                }
            }
            input.close(); seen.close()
            let skipped = min(plan.offset, distinct.count)
            remaining = min(distinct.count - skipped, plan.limit ?? Int64.max)
            output = try distinct.takeOutput()
            for _ in 0..<skipped { _ = try output?.next() }
        } catch { distinct.close(); throw error }
    }
    func close() { output?.close(); output = nil; remaining = 0 }
    deinit { close() }
}
