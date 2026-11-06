/// Stage boundaries for the file-host bounded operator lane. Scratch placement
/// changes residency only; scan, filter and expression work remain in SQL order.
internal enum BlockingRows {
    static func configuration(_ context: ExpressionContext) throws -> SpillConfig {
        guard let config = context.spill, config.canSpill else {
            throw JedError("XX000", "bounded operator has no scratch policy")
        }
        return config
    }

    /// Return touched source rows before the complete WHERE. Bounded join
    /// drivers install their producer here; other materialized producers keep
    /// their prescribed admission phase before transfer into spool residency.
    static func gather(_ plan: SelectPlan, context: ExpressionContext) throws -> RowSpool {
        if let rows = try BlockingJoinRows.gather(plan, context: context) { return rows }
        var raw = plan
        raw.predicate = nil
        let eager: Bool
        if let candidate = plan.chosenAccess {
            switch candidate.path {
            case .primary, .full: eager = false
            default: eager = true
            }
        } else { eager = !plan.scan.pointSets.isEmpty }
        raw.materializeSource = eager
        let input = SelectInput(plan: raw, context: context)
        defer { input.close() }
        return try input.drainToSpool(config: configuration(context))
    }

    /// Even an absent predicate copies through this stage. The old spool stays
    /// charged while the new one is populated, then its complete charge ends.
    static func filter(_ rows: RowSpool, plan: SelectPlan, context: ExpressionContext) throws -> RowSpool {
        defer { rows.close() }
        let output = RowSpool(config: try configuration(context))
        do {
            let cursor = try rows.cursor(); defer { cursor.close() }
            while let row = try cursor.next() {
                try context.meter.pollStep()
                var current = context; current.column = { row[$0] }
                if let predicate = plan.predicate,
                   try !context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { continue }
                try output.append(row)
            }
            return output
        } catch { output.close(); throw error }
    }

    /// All expression keys precede all collation decoration. The sorted rows
    /// retain private key slots through output, just as the bounded row lane.
    static func order(_ rows: RowSpool, plan: SelectPlan, context: ExpressionContext,
                      rowContext: (([SQLValue]) -> ExpressionContext)? = nil,
                      columnSlot: ((Int) -> Int?)? = nil) throws -> RowSpool {
        guard !plan.order.isEmpty else { return rows }
        let config = try configuration(context)
        let decorated = RowSpool(config: config)
        defer { rows.close(); decorated.close() }
        func slot(_ index: Int) -> Int? {
            if let columnSlot { return columnSlot(index) }
            return index >= 0 ? index : nil
        }
        let noncolumns = plan.keyEvaluationOrder.filter {
            if !plan.materializesOrderKey(plan.order[$0]),
               case .column(let index) = plan.order[$0].expression.node, slot(index) != nil { return false }
            return true
        }
        func current(_ row: [SQLValue]) -> ExpressionContext {
            if let rowContext { return rowContext(row) }
            var result = context; result.column = { row[$0] }; return result
        }
        do {
            let cursor = try rows.cursor(); defer { cursor.close() }
            while let row = try cursor.next() {
                try context.meter.pollStep()
                let local = current(row)
                var value = row
                for ordinal in noncolumns { value.append(try local.evaluate(plan.order[ordinal].expression)) }
                try decorated.append(value)
            }
        } catch { throw error }
        rows.close()
        var keySlots: [Int] = []
        let sorter = ExternalRowSorter(workMemoryBytes: config.workMemoryBytes, host: config.host,
            meter: context.meter) { left, right in
                for ordinal in plan.order.indices {
                    let term = plan.order[ordinal], a = left[keySlots[ordinal]], b = right[keySlots[ordinal]]
                    if a.isNull != b.isNull { return a.isNull == term.nullsFirst ? -1 : 1 }
                    var type = term.type
                    if ExpressionCollation.collatable(type.type), type.collation?.description != nil,
                       type.collation?.description != "C" { type = TypeDescriptor(.scalar(.bytea)) }
                    let comparison = try ValueComparison.total(a, b, type: type)
                    if comparison != 0 { return term.descending ? -comparison : comparison }
                }
                return 0
            }
        defer { sorter.close() }
        do {
            let cursor = try decorated.cursor(); defer { cursor.close() }
            while let row = try cursor.next() {
                try context.meter.pollStep()
                var value = row
                if keySlots.isEmpty { keySlots = Array(row.count..<(row.count + plan.order.count)) }
                var keys = Array(repeating: SQLValue.null, count: plan.order.count)
                for ordinal in plan.keyEvaluationOrder {
                    let term = plan.order[ordinal]
                    let key: SQLValue
                    if let extra = noncolumns.firstIndex(of: ordinal) { key = row[row.count - noncolumns.count + extra] }
                    else if case .column(let index) = term.expression.node, let physical = slot(index) { key = row[physical] }
                    else { throw JedError("XX000", "sort key has no materialized slot") }
                    try AggregateOrderedSet.chargeKey(key, type: term.type, context: context)
                    if !key.isNull, ExpressionCollation.collatable(term.type.type),
                       let collation = term.type.collation, collation.description != "C" {
                        keys[ordinal] = .bytea(try KeyEncoding.encode(key, type: term.type, collatedKey: context.collatedKey))
                    } else { keys[ordinal] = key }
                }
                value.append(contentsOf: keys)
                try sorter.append(value)
            }
        } catch { throw error }
        decorated.close()
        try sorter.finish()
        let output = RowSpool(config: config)
        do {
            while let row = try sorter.next() { try output.append(row) }
            return output
        } catch { output.close(); throw error }
    }
    static func distinctOn(_ rows: RowSpool, plan: SelectPlan, context: ExpressionContext) throws -> RowSpool {
        guard let distinct = plan.distinctOn else { return rows }
        defer { rows.close() }
        let output = RowSpool(config: try configuration(context))
        do {
            let cursor = try rows.cursor(); defer { cursor.close() }
            var previous: [SQLValue]?
            while let row = try cursor.next() {
                try context.meter.pollStep()
                let keys = Array(row.suffix(plan.order.count))
                if let previous, try distinct.same(previous, keys, order: plan.order) { continue }
                previous = keys; try output.append(row)
            }
            return output
        } catch { output.close(); throw error }
    }

}
