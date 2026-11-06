/// Group keys, finite state and retained values have separate bounded owners.
/// Rows fold globally in input order; only finalization visits groups in their
/// first-occurrence order, so spilling cannot reorder errors or decimal work.
internal enum BlockingAggregate {
    static func columnSlot(_ index: Int, aggregate: AggregateQueryPlan) -> Int? {
        if index < 0 { return aggregate.keys.count + aggregate.calls.count - index - 1 }
        if index >= aggregate.layoutWidth { return aggregate.keys.count + index - aggregate.layoutWidth }
        return aggregate.keyColumns.firstIndex(of: index)
    }
    static func rowContext(_ row: [SQLValue], aggregate: AggregateQueryPlan, context: ExpressionContext) -> ExpressionContext {
        var result = context
        result.column = { index in
            guard let slot = columnSlot(index, aggregate: aggregate), row.indices.contains(slot) else { throw JedError("XX000", "invalid aggregate output column") }
            return row[slot]
        }
        result.hostBatchColumn = { index in
            guard let slot = columnSlot(index, aggregate: aggregate), row.indices.contains(slot) else { return nil }
            return row[slot]
        }
        return result
    }
    private static func unmetered(_ context: ExpressionContext) -> ExpressionContext {
        var result = context
        let meter = CostMeter(resources: context.meter.resources, cancellation: context.meter.cancellation)
        result.meter = meter; result.scalar.meter = meter
        return result
    }
    private static func owner(group: Int64, ordinal: Int, count: Int) throws -> UInt64 {
        guard group >= 0 else { throw JedError("58030", "invalid aggregate group ordinal") }
        let product = UInt64(group).multipliedReportingOverflow(by: UInt64(count))
        let result = product.partialValue.addingReportingOverflow(UInt64(ordinal))
        guard !product.overflow, !result.overflow else { throw JedError("54000", "aggregate group count out of range") }
        return result.partialValue
    }
    static func build(_ input: RowSpool, aggregate: AggregateQueryPlan,
                      context: ExpressionContext) throws -> RowSpool {
        let config = try BlockingRows.configuration(context)
        var rows = input
        defer { rows.close() }
        if aggregate.keyColumns.contains(where: { $0 >= aggregate.sourceWidth }) {
            let decorated = RowSpool(config: config)
            do {
                let cursor = try rows.cursor(); defer { cursor.close() }
                while var row = try cursor.next() {
                    try context.meter.checkpoint()
                    var current = context; let source = row; current.column = { source[$0] }
                    for (slot, key) in zip(aggregate.keyColumns, aggregate.keys) where slot >= aggregate.sourceWidth {
                        row.append(try current.evaluate(key))
                    }
                    try decorated.append(row)
                }
            } catch { decorated.close(); throw error }
            rows.close(); rows = decorated
        }
        let output = RowSpool(config: config)
        do {
            for members in aggregate.sets {
                try foldSet(members, input: rows, output: output, aggregate: aggregate, context: context, config: config)
            }
            return output
        } catch { output.close(); throw error }
    }
    private static func foldSet(_ members: [Int], input: RowSpool, output: RowSpool,
                                aggregate: AggregateQueryPlan, context: ExpressionContext, config: SpillConfig) throws {
        let groups = SpillStateMap(config: config), keys = RowSpool(config: config)
        let seen = SpillStateMap(config: config), unique = SpillStateMap(config: config), collections = HashRowTable(config: config)
        defer { keys.close(); collections.close(); unique.close(); seen.close(); groups.close() }
        func fresh(_ ordinal: Int64) -> [SQLValue] {
            [.integer(ordinal)] + aggregate.calls.map { call in
                if !call.order.isEmpty { return .composite([]) }
                let state = AggregateState(plan: call.plan, context: context, bounded: true)
                defer { state.close() }; return .composite(state.packed())
            }
        }
        if members.isEmpty { try groups.put([], value: fresh(0)); try keys.append([]) }
        do {
            let cursor = try input.cursor(); defer { cursor.close() }
            while let row = try cursor.next() {
                try context.meter.checkpoint()
                var current = context; current.column = { row[$0] }
                let key = members.map { row[aggregate.keyColumns[$0]] }
                for value in key {
                    let bytes = MemorySizes.payload(value)
                    if bytes > 0 { context.meter.charge(bytes); try context.meter.guardCost() }
                }
                var packed: [SQLValue]
                if let stored = try groups.get(key) { packed = stored }
                else { packed = fresh(keys.count); try keys.append(key) }
                guard case .integer(let group) = packed[0] else { throw JedError("58030", "invalid aggregate group state") }
                for (ordinal, call) in aggregate.calls.enumerated() {
                    if let filter = call.filter, case .boolean(true) = try current.evaluate(filter) {} else if call.filter != nil { continue }
                    context.meter.charge(1)
                    let owner = try owner(group: group, ordinal: ordinal, count: aggregate.calls.count)
                    if AggregateOrderedSet.hypothetical.contains(call.plan.name), !call.plan.withinGroup.isEmpty {
                        var arguments: [SQLValue] = []
                        for (index, expression) in call.arguments.enumerated() {
                            let value = try current.evaluate(expression)
                            try AggregateOrderedSet.chargeKey(value, type: call.plan.withinGroup[index].type, context: current)
                            arguments.append(value)
                        }
                        try collections.append(hash: owner, row: arguments); continue
                    }
                    if call.arguments.count > 1 && call.plan.withinGroup.isEmpty { context.meter.charge(1); try context.meter.step() }
                    let arguments = try call.arguments.map(current.evaluate)
                    let ordered: [SQLValue]?
                    if call.order.isEmpty { ordered = nil }
                    else { ordered = try AggregateOrdering(order: call.order, context: context).tuple(arguments: arguments, current: current) }
                    if call.distinct {
                        let operand = arguments.count == 1 ? arguments[0] : .composite(arguments)
                        let keepsNull = ["array_agg", "json_agg", "jsonb_agg", "json_agg_strict", "jsonb_agg_strict"].contains(call.plan.name)
                        if operand.isNull && !keepsNull { continue }
                        let bytes = MemorySizes.payload(operand)
                        if bytes > 0 { context.meter.charge(bytes); try context.meter.guardCost() }
                        guard try seen.insert([.integer(group), .integer(Int64(ordinal)), operand]) else { continue }
                    }
                    if let ordered { try collections.append(hash: owner, row: ordered); continue }
                    guard case .composite(let fields) = packed[ordinal + 1] else { throw JedError("58030", "invalid accumulator tuple") }
                    let state = AggregateState(plan: call.plan, context: context, bounded: true)
                    defer { state.close() }
                    try state.restore(fields)
                    try BlockingAggregateFinalizer.fold(state, arguments: arguments, unique: unique,
                        prefix: [.integer(group), .integer(Int64(ordinal))], collections: collections, owner: owner)
                    packed[ordinal + 1] = .composite(state.packed())
                }
                try groups.put(key, value: packed)
            }
        }
        let cursor = try keys.cursor(); defer { cursor.close() }
        let membership = Set(members)
        while let key = try cursor.next() {
            guard let packed = try groups.get(key), case .integer(let group) = packed[0] else { throw JedError("58030", "missing aggregate group") }
            var row = Array(repeating: SQLValue.null, count: aggregate.keys.count)
            for (position, index) in members.enumerated() { row[index] = key[position] }
            for (ordinal, call) in aggregate.calls.enumerated() {
                guard case .composite(let fields) = packed[ordinal + 1] else { throw JedError("58030", "invalid accumulator tuple") }
                let state = AggregateState(plan: call.plan, context: context, bounded: true)
                defer { state.close() }
                if call.order.isEmpty { try state.restore(fields) }
                let current = unmetered(rowContext(row, aggregate: aggregate, context: context))
                let direct = try call.direct.map(current.evaluate)
                let value = try BlockingAggregateFinalizer.finish(state, call: call, direct: direct,
                    collections: collections, owner: owner(group: group, ordinal: ordinal, count: aggregate.calls.count), context: context)
                row.append(value)
            }
            row.append(contentsOf: aggregate.grouping.map { positions in
                .integer(positions.reduce(Int64(0)) { ($0 << 1) | (membership.contains($1) ? 0 : 1) })
            })
            try output.append(row)
        }
    }

    static func having(_ rows: RowSpool, aggregate: AggregateQueryPlan, context: ExpressionContext) throws -> RowSpool {
        guard let predicate = aggregate.having else { return rows }
        defer { rows.close() }
        let kept = RowSpool(config: try BlockingRows.configuration(context))
        do {
            let cursor = try rows.cursor(); defer { cursor.close() }
            while let row = try cursor.next() {
                try context.meter.pollStep()
                if try rowContext(row, aggregate: aggregate, context: context).matches(predicate) { try kept.append(row) }
            }
            return kept
        } catch { kept.close(); throw error }
    }
}

internal final class BlockingAggregateExecution {
    private let plan: SelectPlan
    private let aggregate: AggregateQueryPlan
    private let context: ExpressionContext
    private let hostBatch: HostBatch?
    private let ahead = HostReadAhead<[SQLValue]>()
    private var output: RowSpoolCursor?
    private var remaining: Int64 = 0
    private var started = false
    init(plan: SelectPlan, aggregate: AggregateQueryPlan, context: ExpressionContext) {
        self.plan = plan; self.aggregate = aggregate; self.context = context
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
        var current = BlockingAggregate.rowContext(row, aggregate: aggregate, context: context); current.hostBatch = hostBatch
        if let at {
            try hostBatch?.setRow(at, count: ahead.rows.count, context: current) { source, column in
                let values = self.ahead.rows[source]
                guard let slot = BlockingAggregate.columnSlot(column, aggregate: self.aggregate), values.indices.contains(slot) else { return nil }
                return values[slot]
            }
        }
        return try plan.projections.map(current.evaluate)
    }
    private func prepare() throws {
        var rows = try context.profile(plan.profileIdentity, .aggregate, excluding: [.source, .filter, .sort]) {
            let source = try BlockingRows.gather(plan, context: context)
            let input = try BlockingRows.filter(source, plan: plan, context: context)
            let groups = try BlockingAggregate.build(input, aggregate: aggregate, context: context)
            return try BlockingAggregate.having(groups, aggregate: aggregate, context: context)
        }
        var transferred = false
        defer { if !transferred { rows.close() } }
        if !plan.order.isEmpty {
            rows = try context.profile(plan.profileIdentity, .sort) {
                try BlockingRows.order(rows, plan: plan, context: context,
                    rowContext: { BlockingAggregate.rowContext($0, aggregate: self.aggregate, context: self.context) },
                    columnSlot: { BlockingAggregate.columnSlot($0, aggregate: self.aggregate) })
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
