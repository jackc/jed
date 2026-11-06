/// Ordered-set buffers use the same per-group lifetime as ordinary retained
/// aggregates. Direct arguments arrive once, evaluated in the finalized group.
internal final class AggregateOrderedSet {
    static let ordered: Set<String> = ["mode", "percentile_disc", "percentile_cont"]
    static let hypothetical: Set<String> = ["rank", "dense_rank", "percent_rank", "cume_dist"]
    let plan: AggregatePlan
    let context: ExpressionContext
    private var rows: [[SQLValue]] = []
    private var reserved: Int64 = 0
    private let measured: Bool

    init(plan: AggregatePlan, context: ExpressionContext, measured: Bool = true) {
        self.plan = plan; self.context = context; self.measured = measured
    }
    private func reserve(_ bytes: Int64) throws {
        guard measured else { return }
        try context.meter.reserveQuery(bytes); reserved = saturatingAdd(reserved, bytes)
    }
    func drainRetained(_ visit: ([SQLValue]) throws -> Void) rethrows {
        defer { rows = [] }
        for row in rows { try visit(row) }
    }
    static func chargeKey(_ value: SQLValue, type: TypeDescriptor, context: ExpressionContext) throws {
        let units: Int64
        if let name = type.collation, name.description != "C", ExpressionCollation.collatable(type.type) {
            switch value {
            case .text(let value): units = Int64(value.scalarCount)
            case .array(let value): units = value.elements.reduce(0) { count, item in
                if case .text(let text) = item { return saturatingAdd(count, Int64(text.scalarCount)) }; return count
            }
            default: units = 0
            }
        } else { units = MemorySizes.payload(value) }
        if units > 0 { context.meter.charge(units); try context.meter.guardCost() }
    }
    func fold(_ incoming: [SQLValue]) throws {
        if Self.hypothetical.contains(plan.name) {
            try reserve(MemorySizes.row(incoming)); rows.append(incoming); return
        }
        guard !incoming[0].isNull else { return }
        var value = incoming[0]
        if plan.name == "percentile_cont", plan.inputTypes[0].type != .scalar(.interval) {
            value = .f64(try AggregateStatistics.number(value).bitPattern)
        }
        try Self.chargeKey(value, type: plan.withinGroup[0].type, context: context)
        // The retained-input contract measures the authored argument, even
        // when percentile_cont stores its widened floating representation.
        try reserve(MemorySizes.value(incoming[0])); rows.append([value])
    }
    func finish(direct: [SQLValue]) throws -> SQLValue {
        if Self.hypothetical.contains(plan.name) { return try hypothetical(direct) }
        let order = plan.withinGroup[0]
        var type = order.type
        if plan.name == "percentile_cont", plan.inputTypes[0].type != .scalar(.interval) { type = TypeDescriptor(.scalar(.f64)) }
        // Collation keys are execution-owned decorations, released immediately
        // after sorting; equal keys retain the original input visitation order.
        var keys: [[UInt8]] = [], decorationBytes: Int64 = 0
        if let name = type.collation, name.description != "C", ExpressionCollation.collatable(type.type) {
            do {
                for row in rows {
                    let key = try KeyEncoding.encode(row[0], type: type, collatedKey: context.collatedKey)
                    let bytes = MemorySizes.value(.bytea(key))
                    try context.meter.reserveQuery(bytes); decorationBytes = saturatingAdd(decorationBytes, bytes)
                    keys.append(key)
                }
            } catch { context.meter.resources.query.release(decorationBytes); throw error }
        }
        var indices = Array(rows.indices)
        do {
            try indices.sort { a, b in
                let comparison: Int
                if keys.isEmpty { comparison = try ValueComparison.total(rows[a][0], rows[b][0], type: type) }
                else { comparison = keys[a] == keys[b] ? 0 : keys[a].lexicographicallyPrecedes(keys[b]) ? -1 : 1 }
                if comparison != 0 { return order.descending ? comparison > 0 : comparison < 0 }
                return a < b
            }
        } catch { context.meter.resources.query.release(decorationBytes); throw error }
        keys = []; context.meter.resources.query.release(decorationBytes)
        if plan.name == "mode" {
            guard let first = indices.first else { return .null }
            var best = first, bestCount = 1, start = 0
            for position in indices.indices.dropFirst() {
                let same = try ValueComparison.total(rows[indices[position]][0], rows[indices[start]][0], type: type, equality: true) == 0
                if same {
                    let count = position - start + 1
                    if count > bestCount { best = indices[start]; bestCount = count }
                } else { start = position }
            }
            return rows[best][0]
        }
        guard direct.count == 1 else { throw JedError("XX000", "percentile direct-argument width mismatch") }
        func fraction(_ value: SQLValue) throws -> Double? {
            if value.isNull { return nil }
            let number = try AggregateStatistics.number(value)
            guard !number.isNaN, number >= 0, number <= 1 else { throw JedError("22003", "percentile value is not between 0 and 1") }
            return number
        }
        func at(_ fraction: Double) throws -> SQLValue {
            if plan.name == "percentile_disc" {
                let position = max(0, Int((fraction * Double(indices.count)).rounded(.up)) - 1)
                return rows[indices[position]][0]
            }
            let position = fraction * Double(indices.count - 1)
            let lower = Int(position.rounded(.down)), upper = Int(position.rounded(.up))
            let low = rows[indices[lower]][0]
            if lower == upper { return low }
            let high = rows[indices[upper]][0], proportion = position - Double(lower)
            if case .interval(let low) = low, case .interval(let high) = high {
                let difference = try TemporalFunctions.add(high, low, subtract: true)
                return .interval(try TemporalFunctions.add(low, Self.scaleInterval(difference, by: proportion)))
            }
            let lo = try FloatFunctions.number(low), hi = try FloatFunctions.number(high)
            return .f64((lo + proportion * (hi - lo)).bitPattern)
        }
        if case .array(let array) = direct[0] {
            let fractions = try array.elements.map(fraction)
            guard !indices.isEmpty else { return .null }
            let values = try fractions.map { value in try value.map(at) ?? .null }
            return .array(try SQLArray(dimensions: array.dimensions, lowerBounds: array.lowerBounds, elements: values))
        }
        guard let fraction = try fraction(direct[0]), !indices.isEmpty else { return .null }
        return try at(fraction)
    }
    private func hypothetical(_ direct: [SQLValue]) throws -> SQLValue {
        guard direct.count == plan.withinGroup.count else { throw JedError("XX000", "hypothetical direct-argument width mismatch") }
        var before: Int64 = 0, through: Int64 = 0, distinct: Set<[UInt8]> = []
        for row in rows {
            var result = 0
            for index in row.indices {
                let order = plan.withinGroup[index], a = row[index], b = direct[index]
                if a.isNull != b.isNull { result = a.isNull == order.nullsFirst ? -1 : 1 }
                else {
                    result = try ValueComparison.total(a, b, type: order.type, collatedKey: context.collatedKey)
                    if order.descending { result = -result }
                }
                if result != 0 { break }
            }
            if result < 0 {
                before += 1; through += 1
                distinct.insert(try ValueEqualityKey.encode(row))
            } else if result == 0 { through += 1 }
        }
        switch plan.name {
        case "rank": return .integer(before + 1)
        case "dense_rank": return .integer(Int64(distinct.count) + 1)
        case "percent_rank": return .f64((rows.isEmpty ? 0 : Double(before) / Double(rows.count)).bitPattern)
        case "cume_dist": return .f64((Double(through + 1) / Double(rows.count + 1)).bitPattern)
        default: throw JedError("XX000", "invalid hypothetical aggregate")
        }
    }
    /// PG interval interpolation cascades fractional months through thirty-day
    /// months, then fractional days through seconds, rounding ties to even.
    static func scaleInterval(_ value: SQLInterval, by factor: Double) throws -> SQLInterval {
        func secondsRound(_ value: Double) -> Double { (value * 1_000_000).rounded(.toNearestOrEven) / 1_000_000 }
        func i32(_ value: Double) throws -> Int32 {
            guard value >= Double(Int32.min), value < -Double(Int32.min) else { throw Temporal.overflow("interval") }
            return Int32(value)
        }
        let months = try i32(Double(value.months) * factor)
        var days = Int128(try i32(Double(value.days) * factor))
        let monthDays = secondsRound((Double(value.months) * factor - Double(months)) * 30)
        var seconds = secondsRound((Double(value.days) * factor - Double(days) + monthDays - monthDays.rounded(.towardZero)) * 86_400)
        if abs(seconds) >= 86_400 {
            let whole = (seconds / 86_400).rounded(.towardZero)
            days += Int128(try i32(whole)); seconds -= whole * 86_400
        }
        days += Int128(try i32(monthDays))
        let micros = (Double(value.microseconds) * factor + seconds * 1_000_000).rounded(.toNearestOrEven)
        guard micros >= Double(Int64.min), micros < -Double(Int64.min) else { throw Temporal.overflow("interval") }
        return try TemporalFunctions.interval(Int128(months), days, Int128(micros))
    }
    func close() { rows = []; context.meter.resources.query.release(reserved); reserved = 0 }
    deinit { close() }
}
