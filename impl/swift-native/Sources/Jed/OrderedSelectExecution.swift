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
        var bytes: Int64 = 0
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
        try context.profile(plan.profileIdentity, .sort, excluding: [.source, .filter]) { try prepareBody() }
    }
    private func prepareBody() throws {
        let meter = context.meter
        if plan.distinctOn == nil, plan.store != nil, plan.relation == nil, let limit = plan.limit,
           !plan.order.contains(where: { ($0.type.collation?.description ?? "C") != "C" }) {
            let (count, overflow) = plan.offset.addingReportingOverflow(limit)
            if !overflow { try prepareTopK(limit == 0 ? 0 : count); return }
        }
        let input = SelectInput(plan: plan, context: context); defer { input.close() }
        while let row = try input.next() {
            var current = context; current.column = { row[$0] }
            if let predicate = plan.predicate, try !context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { continue }
            try reserve(MemorySizes.row(row))
            entries.append(Entry(row: row, keys: [], ordinal: entries.count, bytes: MemorySizes.row(row)))
        }
        input.close()
        // Expression keys are evaluated for the complete pre-sort input before
        // any key decoration, even for a LIMIT that will emit no rows.
        for index in entries.indices {
            let row = entries[index].row
            var current = context; current.column = { row[$0] }
            entries[index].keys = Array(repeating: .null, count: plan.order.count)
            for ordinal in plan.keyEvaluationOrder {
                let term = plan.order[ordinal]
                let key = try current.evaluate(term.expression)
                if case .column = term.expression.node {} else {
                    let bytes = MemorySizes.value(key); try reserve(bytes); entries[index].bytes = saturatingAdd(entries[index].bytes, bytes)
                }
                entries[index].keys[ordinal] = key
            }
        }
        for index in entries.indices {
            for ordinal in plan.keyEvaluationOrder {
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
        if let distinct = plan.distinctOn {
            var kept: [Entry] = []
            for entry in entries {
                if let previous = kept.last, try distinct.same(previous.keys, entry.keys, order: plan.order) {
                    let bytes = entryBytes(entry); context.meter.resources.query.release(bytes); reserved -= bytes
                } else { kept.append(entry) }
            }
            entries = kept
        }
        for index in entries.indices { entries[index].keys = [] }
        context.meter.resources.query.release(decorations); reserved -= decorations; decorations = 0
        position = Int(min(Int64(entries.count), plan.offset))
    }
    private func before(_ left: Entry, _ right: Entry) throws -> Bool {
        for index in plan.order.indices {
            let term = plan.order[index], a = left.keys[index], b = right.keys[index]
            if a.isNull != b.isNull { return a.isNull == term.nullsFirst }
            let order = try ValueComparison.total(a, b, type: term.type)
            if order != 0 { return term.descending ? order > 0 : order < 0 }
        }
        return left.ordinal < right.ordinal
    }
    private func entryBytes(_ entry: Entry) -> Int64 {
        if entry.bytes > 0 { return entry.bytes }
        return zip(plan.order, entry.keys).reduce(MemorySizes.row(entry.row)) { total, pair in
            if case .column = pair.0.expression.node { return total }
            return saturatingAdd(total, MemorySizes.value(pair.1))
        }
    }
    private func prepareTopK(_ count: Int64) throws {
        let input = SelectInput(plan: plan, context: context); defer { input.close() }
        var ordinal = 0
        while let row = try input.next() {
            var current = context; current.column = { row[$0] }
            if let predicate = plan.predicate, try !context.profile(plan.profileIdentity, .filter, { try current.matches(predicate) }) { continue }
            let keys = try plan.order.map { try current.evaluate($0.expression) }
            for key in keys {
                let units = MemorySizes.payload(key)
                if units > 0 { context.meter.charge(units); try context.meter.guardCost() }
            }
            let entry = Entry(row: row, keys: keys, ordinal: ordinal); ordinal += 1
            guard count > 0 else { continue }
            if Int64(entries.count) < count { try reserve(entryBytes(entry)); entries.append(entry); continue }
            var worst = 0
            for index in entries.indices.dropFirst() where try before(entries[worst], entries[index]) { worst = index }
            guard try before(entry, entries[worst]) else { continue }
            // Admission precedes eviction; both rows are momentarily live.
            try reserve(entryBytes(entry))
            let bytes = entryBytes(entries[worst]); context.meter.resources.query.release(bytes); reserved -= bytes
            entries[worst] = entry
        }
        try entries.sort(by: before)
        for index in entries.indices { entries[index].keys = [] }
        position = Int(min(Int64(entries.count), plan.offset))
    }
    func close() { entries = []; context.meter.resources.query.release(reserved); reserved = 0; decorations = 0 }
    deinit { close() }
}
