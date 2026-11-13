/// The packed single-key aggregate owns a bounded table, then transfers it in
/// creation order to the ordinary spill map and key spool (memory.md §6.6.1).
internal enum ColumnarGroupSpool {
    private struct Group {
        let key: SQLValue
        let states: [AggregateState]
    }
    static func build(plan: SelectPlan, aggregate: AggregateQueryPlan, context: ExpressionContext) throws -> RowSpool {
        let config = try BlockingRows.configuration(context), meter = context.meter
        let charge = SpillCharge(meter: meter)
        var groups: [Group] = [], lookup: [[UInt8]: Int] = [:]
        var resident: Int64 = 0
        var spilled: SpillStateMap?, keys: RowSpool?
        defer {
            charge.releaseAll(); spilled?.close(); keys?.close()
            for group in groups { for state in group.states { state.close() } }
        }
        func fresh() -> [AggregateState] {
            aggregate.calls.map { AggregateState(plan: $0.plan, context: context, bounded: true) }
        }
        func packed(_ states: [AggregateState], ordinal: Int64) -> [SQLValue] {
            [.integer(ordinal)] + states.map { .composite($0.packed()) }
        }
        func tupleSize(_ values: [SQLValue]) -> Int64 { values.reduce(0) { saturatingAdd($0, MemorySizes.value($1)) } }
        func fold(_ states: [AggregateState], current: ExpressionContext) throws {
            for (state, call) in zip(states, aggregate.calls) {
                meter.charge(1)
                try state.fold(call.arguments.map { try current.evaluate($0) })
            }
        }
        let input = try PackedLaneInput(plan: plan, context: context)
        defer { input.close() }
        var current = context; current.column = { try input.row.value($0) }
        let filter = PackedLaneFilter(predicate: plan.predicate, types: plan.store!.tree.types,
            touched: plan.touched, context: current)
        while try input.next() {
            guard try filter.keep(input.row, context: current) else { continue }
            try meter.checkpoint()
            let key = try input.row.value(aggregate.keyColumns[0])
            if let spilled, let keys {
                let stored = try spilled.get([key]), states = fresh()
                defer { for state in states { state.close() } }
                let ordinal: Int64
                if let stored {
                    guard case .integer(let value) = stored[0] else { throw JedError.corruption("invalid grouped ordinal") }
                    ordinal = value
                    for (index, state) in states.enumerated() {
                        guard case .composite(let fields) = stored[index + 1] else { throw JedError.corruption("invalid grouped state") }
                        try state.restore(fields)
                    }
                } else { ordinal = keys.count; try keys.append([key]) }
                try fold(states, current: current)
                try spilled.put([key], value: packed(states, ordinal: ordinal))
                continue
            }
            let encoded = try ValueEqualityKey.encode([key])
            let index: Int
            var rejected = false
            if let found = lookup[encoded] { index = found }
            else {
                index = groups.count
                let group = Group(key: key, states: fresh())
                let bytes = saturatingAdd(GeneratedSpec.memoryEntry,
                    saturatingAdd(MemorySizes.value(key), tupleSize(packed(group.states, ordinal: Int64(index)))))
                rejected = !charge.tryReserve(bytes); resident = saturatingAdd(resident, bytes)
                lookup[encoded] = index; groups.append(group)
            }
            let states = groups[index].states
            for (state, call) in zip(states, aggregate.calls) {
                let before = tupleSize(state.packed())
                meter.charge(1)
                try state.fold(call.arguments.map { try current.evaluate($0) })
                let delta = tupleSize(state.packed()) - before
                resident += delta
                if delta > 0 { if !charge.tryReserve(delta) { rejected = true } }
                else if delta < 0 { charge.release(-delta) }
            }
            if rejected || resident > config.workMemoryBytes {
                charge.releaseAll()
                let map = SpillStateMap(config: config), order = RowSpool(config: config)
                spilled = map; keys = order
                for (ordinal, group) in groups.enumerated() {
                    try map.put([group.key], value: packed(group.states, ordinal: Int64(ordinal)))
                    try order.append([group.key])
                    for state in group.states { state.close() }
                }
                groups = []; lookup = [:]
            }
        }
        let output = RowSpool(config: config)
        do {
            if let spilled, let keys {
                let cursor = try keys.cursor(); defer { cursor.close() }
                while let key = try cursor.next() {
                    guard let stored = try spilled.get(key) else { throw JedError.corruption("missing grouped state") }
                    let states = fresh(); defer { for state in states { state.close() } }
                    for (index, state) in states.enumerated() {
                        guard case .composite(let fields) = stored[index + 1] else { throw JedError.corruption("invalid grouped state") }
                        try state.restore(fields)
                    }
                    try output.append(key + states.map { try $0.finish() })
                }
            } else {
                var rows: [[SQLValue]] = []
                for group in groups { rows.append([group.key] + (try group.states.map { try $0.finish() })) }
                charge.releaseAll()
                for row in rows { try output.append(row) }
            }
            return output
        } catch { output.close(); throw error }
    }
}
