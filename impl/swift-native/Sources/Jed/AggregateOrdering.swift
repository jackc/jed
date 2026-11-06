/// In-aggregate ORDER BY evaluates/decorates per passing input, then retains
/// admitted tuples until a stable finalize fold into the ordinary accumulator.
internal final class AggregateOrdering {
    private struct Entry { let row: [SQLValue]; let ordinal: Int }
    let order: [ResolvedOrderTerm]
    let context: ExpressionContext
    private var rows: [Entry] = []
    private var reserved: Int64 = 0
    init(order: [ResolvedOrderTerm], context: ExpressionContext) { self.order = order; self.context = context }

    func tuple(arguments: [SQLValue], current: ExpressionContext) throws -> [SQLValue] {
        var result: [SQLValue] = []
        for term in order {
            let value = try current.evaluate(term.expression)
            if let name = term.type.collation, name.description != "C", !value.isNull, ExpressionCollation.collatable(term.type.type) {
                let units: Int64
                switch value {
                case .text(let text): units = Int64(text.scalarCount)
                case .array(let array): units = array.elements.reduce(0) { n, value in
                    if case .text(let text) = value { return saturatingAdd(n, Int64(text.scalarCount)) }; return n
                }
                default: units = 0
                }
                if units > 0 { current.meter.charge(units); try current.meter.guardCost() }
                result.append(.bytea(try KeyEncoding.encode(value, type: term.type, collatedKey: current.collatedKey)))
            } else {
                let units = MemorySizes.payload(value)
                if units > 0 { current.meter.charge(units); try current.meter.guardCost() }
                result.append(value)
            }
        }
        result.append(arguments.count == 1 ? arguments[0] : .composite(arguments))
        return result
    }
    func append(_ row: [SQLValue]) throws {
        let bytes = MemorySizes.row(row)
        try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
        rows.append(Entry(row: row, ordinal: rows.count))
    }
    func fold(into state: AggregateState) throws {
        try rows.sort { a, b in
            for index in order.indices {
                let term = order[index], left = a.row[index], right = b.row[index]
                if left.isNull != right.isNull { return left.isNull == term.nullsFirst }
                var type = term.type
                if type.collation?.description != nil && type.collation?.description != "C" && ExpressionCollation.collatable(type.type) { type = TypeDescriptor(.scalar(.bytea)) }
                let result = try ValueComparison.total(left, right, type: type)
                if result != 0 { return term.descending ? result > 0 : result < 0 }
            }
            return a.ordinal < b.ordinal
        }
        for entry in rows {
            try context.meter.checkpoint()
            let operand = entry.row[order.count]
            if state.plan.inputTypes.count == 1 { try state.foldScalar(operand) }
            else if case .composite(let arguments) = operand { try state.fold(arguments) }
            else { throw JedError("XX000", "ordered aggregate tuple shape mismatch") }
        }
    }
    func close() { rows = []; context.meter.resources.query.release(reserved); reserved = 0 }
    deinit { close() }
}
