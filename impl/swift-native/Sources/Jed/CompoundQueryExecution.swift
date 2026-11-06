/// Blocking set operations transfer their charged operand rows into the result.
/// Hash-table entries and sort decorations have separate, bounded lifetimes.
internal final class CompoundQueryExecution {
    private let plan: SelectPlan
    private let compound: CompoundQueryPlan
    private let context: ExpressionContext
    private var rows: [[SQLValue]?] = []
    private var reserved: Int64 = 0
    private var position = 0
    private var started = false
    init(plan: SelectPlan, compound: CompoundQueryPlan, context: ExpressionContext) {
        self.plan = plan; self.compound = compound; self.context = context
    }
    private func reserve(_ bytes: Int64) throws {
        try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
    }
    private func release(_ bytes: Int64) { context.meter.resources.query.release(bytes); reserved -= bytes }
    func next() throws -> [SQLValue]? {
        if !started { try prepare(); started = true }
        guard position < rows.count, let row = rows[position] else { return nil }
        if case .values(_, true) = compound.body { context.meter.charge(1); try context.meter.guardCost() }
        rows[position] = nil; position += 1; release(MemorySizes.row(row))
        return row
    }
    private func collect(_ plan: SelectPlan) throws -> [[SQLValue]] {
        let execution = SelectExecution(plan: plan, context: compound.context(from: context))
        defer { execution.close() }
        var result: [[SQLValue]] = []
        while let row = try execution.next() { try reserve(MemorySizes.row(row)); result.append(row) }
        return result
    }
    private func widen(_ rows: inout [[SQLValue]], source: SelectPlan) throws {
        let before = rows.reduce(Int64(0)) { saturatingAdd($0, MemorySizes.row($1)) }
        for row in rows.indices {
            for column in plan.projections.indices {
                if let target = plan.projections[column].type {
                    rows[row][column] = try context.convert(rows[row][column], from: source.projections[column].type?.type, to: target)
                }
            }
        }
        let after = rows.reduce(Int64(0)) { saturatingAdd($0, MemorySizes.row($1)) }
        if after > before { try reserve(after - before) } else { release(before - after) }
    }
    private func prepare() throws {
        let meter = context.meter
        var output: [[SQLValue]]
        switch compound.body {
        case .nested(let child): output = try collect(child)
        case .values(let inputs, _):
            output = []
            let current = compound.context(from: context)
            for expressions in inputs {
                try meter.checkpoint(); meter.charge(1)
                let row = try expressions.enumerated().map { index, expression in
                    let value = try current.evaluate(expression)
                    guard let target = plan.projections[index].type else { return value }
                    return try current.convert(value, from: expression.type?.type, to: target)
                }
                try reserve(MemorySizes.row(row)); output.append(row)
            }
        case .set(let operation, let all, let lhs, let rhs):
            var left = try collect(lhs), right = try collect(rhs)
            try widen(&left, source: lhs); try widen(&right, source: rhs)
            if operation == "union" && all { output = left + right }
            else {
                for row in left + right {
                    for value in row {
                        let units = MemorySizes.payload(value)
                        if units > 0 { meter.charge(units); try meter.guardCost() }
                    }
                }
                var membership: [[UInt8]: Int] = [:], kept: Set<[UInt8]> = []
                var tableBytes: Int64 = 0
                defer { release(tableBytes) }
                func entry(_ row: [SQLValue]) throws {
                    let bytes = row.reduce(GeneratedSpec.memoryEntry) { saturatingAdd($0, MemorySizes.value($1)) }
                    try reserve(bytes); tableBytes = saturatingAdd(tableBytes, bytes)
                }
                if operation != "union" {
                    for row in right {
                        let key = try ValueEqualityKey.encode(row)
                        if membership[key] == nil { try entry(row) }
                        membership[key, default: 0] += 1
                    }
                }
                output = []
                for row in operation == "union" ? left + right : left {
                    try meter.pollStep()
                    let key = try ValueEqualityKey.encode(row), count = membership[key] ?? 0
                    let keep: Bool
                    if operation == "union" { keep = !kept.contains(key) }
                    else if operation == "intersect" {
                        keep = count > 0 && (all || !kept.contains(key))
                        if all && count > 0 { membership[key] = count - 1 }
                    } else {
                        keep = count == 0 && (all || !kept.contains(key))
                        if all && count > 0 { membership[key] = count - 1 }
                    }
                    if keep {
                        if !all { try entry(row); kept.insert(key) }
                        output.append(row)
                    }
                }
                let before = (left + right).reduce(Int64(0)) { saturatingAdd($0, MemorySizes.row($1)) }
                let after = output.reduce(Int64(0)) { saturatingAdd($0, MemorySizes.row($1)) }
                release(before - after)
            }
        }
        try BufferedRowSort.sort(&output, order: plan.order, context: context, reserve: reserve, release: release)
        let start = Int(min(Int64(output.count), plan.offset))
        let end = start + Int(min(Int64(output.count - start), plan.limit ?? Int64.max))
        for row in output[..<start] { release(MemorySizes.row(row)) }
        for row in output[end...] { release(MemorySizes.row(row)) }
        rows = output[start..<end].map(Optional.some)
    }
    func close() { rows = []; context.meter.resources.query.release(reserved); reserved = 0 }
    deinit { close() }
}

internal enum BufferedRowSort {
    static func sort(_ output: inout [[SQLValue]], order: [ResolvedOrderTerm], context: ExpressionContext,
                     reserve: (Int64) throws -> Void, release: (Int64) -> Void) throws {
        guard !order.isEmpty else { return }
        var decorations: Int64 = 0
        defer { release(decorations) }
        var decorated: [(row: [SQLValue], keys: [SQLValue], ordinal: Int)] = []
        for (ordinal, row) in output.enumerated() {
            var keys: [SQLValue] = []
            for term in order {
                guard case .column(let index) = term.expression.node else { throw JedError("XX000", "compound sort key is not a column") }
                let value = row[index]
                if let name = term.type.collation, name.description != "C", !value.isNull, ExpressionCollation.collatable(term.type.type) {
                    let units: Int64
                    switch value {
                    case .text(let text): units = Int64(text.scalarCount)
                    case .array(let array): units = array.elements.reduce(0) { n, value in
                        if case .text(let text) = value { return saturatingAdd(n, Int64(text.scalarCount)) }; return n
                    }
                    default: units = 0
                    }
                    if units > 0 { context.meter.charge(units); try context.meter.guardCost() }
                    let key = try KeyEncoding.encode(value, type: term.type, collatedKey: context.collatedKey)
                    let bytes = saturatingAdd(GeneratedSpec.memoryValue, Int64(key.count))
                    try reserve(bytes); decorations = saturatingAdd(decorations, bytes); keys.append(.bytea(key))
                } else {
                    let units = MemorySizes.payload(value)
                    if units > 0 { context.meter.charge(units); try context.meter.guardCost() }
                    keys.append(value)
                }
            }
            decorated.append((row, keys, ordinal))
        }
        try decorated.sort { left, right in
            for index in order.indices {
                let term = order[index], a = left.keys[index], b = right.keys[index]
                if a.isNull != b.isNull { return a.isNull == term.nullsFirst }
                var type = term.type
                if let name = type.collation, name.description != "C", ExpressionCollation.collatable(type.type) { type = TypeDescriptor(.scalar(.bytea)) }
                let comparison = try ValueComparison.total(a, b, type: type)
                if comparison != 0 { return term.descending ? comparison > 0 : comparison < 0 }
            }
            return left.ordinal < right.ordinal
        }
        output = decorated.map(\.row)
    }
}
