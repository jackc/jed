internal struct ResolvedOrderTerm {
    let expression: ResolvedExpression
    let type: TypeDescriptor
    let descending: Bool
    let nullsFirst: Bool
}

/// Blocking order keeps only the touched row layout. All ORDER BY expressions
/// precede decoration, and projection begins only after the final row window.
internal final class OrderedSelectExecution {
    private struct Entry {
        let row: [SQLValue]
        var keys: [SQLValue]
        let ordinal: Int
    }
    private let plan: SelectPlan
    private let context: ExpressionContext
    private var entries: [Entry] = []
    private var reserved: Int64 = 0
    private var decorations: Int64 = 0
    private var started = false
    private var position = 0
    private var emitted: Int64 = 0
    init(plan: SelectPlan, context: ExpressionContext) { self.plan = plan; self.context = context }

    func next() throws -> [SQLValue]? {
        if !started { try prepare(); started = true }
        guard position < entries.count, plan.limit == nil || emitted < plan.limit! else { return nil }
        let row = entries[position].row; position += 1; emitted += 1
        if plan.productionCharge { context.meter.charge(1); try context.meter.guardCost() }
        var current = context; current.column = { row[$0] }
        return try plan.projections.map { try current.evaluate($0) }
    }
    private func reserve(_ bytes: Int64) throws {
        try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
    }
    private func prepare() throws {
        let meter = context.meter
        let input = SelectInput(plan: plan, context: context); defer { input.close() }
        while let row = try input.next() {
            var current = context; current.column = { row[$0] }
            if let predicate = plan.predicate, try !current.matches(predicate) { continue }
            try reserve(MemorySizes.row(row))
            entries.append(Entry(row: row, keys: [], ordinal: entries.count))
        }
        input.close()
        // Expression keys are evaluated for the complete pre-sort input before
        // any key decoration, even for a LIMIT that will emit no rows.
        for index in entries.indices {
            let row = entries[index].row
            var current = context; current.column = { row[$0] }
            for term in plan.order {
                let key = try current.evaluate(term.expression)
                if case .column = term.expression.node {} else { try reserve(MemorySizes.value(key)) }
                entries[index].keys.append(key)
            }
        }
        for index in entries.indices {
            for ordinal in plan.order.indices {
                let value = entries[index].keys[ordinal], type = plan.order[ordinal].type
                if let name = type.collation, name.description != "C", !value.isNull,
                   ExpressionCollation.collatable(type.type) {
                    let units: Int64
                    switch value {
                    case .text(let text): units = Int64(text.scalarCount)
                    case .array(let array): units = array.elements.reduce(0) { total, element in
                        if case .text(let text) = element { return saturatingAdd(total, Int64(text.scalarCount)) }; return total
                    }
                    default: units = 0
                    }
                    if units > 0 { meter.charge(units); try meter.guardCost() }
                    let key = try KeyEncoding.encode(value, type: type, collatedKey: context.collatedKey)
                    let bytes = saturatingAdd(GeneratedSpec.memoryValue, Int64(key.count))
                    try reserve(bytes); decorations = saturatingAdd(decorations, bytes)
                    entries[index].keys[ordinal] = .bytea(key)
                } else {
                    let units = MemorySizes.payload(value)
                    if units > 0 { meter.charge(units); try meter.guardCost() }
                }
            }
        }
        try entries.sort { left, right in
            for ordinal in plan.order.indices {
                let a = left.keys[ordinal], b = right.keys[ordinal], term = plan.order[ordinal]
                if a.isNull != b.isNull { return a.isNull == term.nullsFirst }
                var type = term.type
                if type.collation?.description != nil && type.collation?.description != "C" && ExpressionCollation.collatable(type.type) {
                    type = TypeDescriptor(.scalar(.bytea))
                }
                let order = try ValueComparison.total(a, b, type: type)
                if order != 0 { return term.descending ? order > 0 : order < 0 }
            }
            return left.ordinal < right.ordinal
        }
        for index in entries.indices { entries[index].keys = [] }
        context.meter.resources.query.release(decorations); reserved -= decorations; decorations = 0
        position = Int(min(Int64(entries.count), plan.offset))
    }
    func close() { entries = []; context.meter.resources.query.release(reserved); reserved = 0; decorations = 0 }
    deinit { close() }
}
