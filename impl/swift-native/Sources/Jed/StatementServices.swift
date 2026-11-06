/// One instance belongs to one query-block execution. Copying its expression
/// context preserves the cached clock and UUID counter; executing a subquery or
/// set-operation arm creates a fresh instance. Random sources remain shared by
/// the immutable environment. Construction itself reads neither source.
internal final class StatementServices {
    let settings: SessionSettings
    let meter: CostMeter
    private var clock: Int64?
    private var uuidCounter: UInt64 = 0

    init(settings: SessionSettings, meter: CostMeter) {
        self.settings = settings; self.meter = meter
    }

    func now() throws -> Int64 {
        if let clock { return clock }
        let value = try settings.environment.clockSource.microseconds()
        clock = value
        return value
    }

    func context(parameters: [SQLValue] = [],
                 column: @escaping (Int) throws -> SQLValue = { _ in throw JedError("XX000", "missing expression row") }) -> ExpressionContext {
        var context = ExpressionContext(meter: meter, timezone: settings.timeZone, parameters: parameters, column: column)
        install(into: &context)
        return context
    }

    /// Retains existing specialization/regex services and delegates unknown state
    /// functions to the previous handler (the transaction owns sequence state).
    func install(into context: inout ExpressionContext) {
        let environment = settings.environment, fallback = context.functions.stateCall
        context.scalar.timezone = settings.timeZone
        context.scalar.statementClock = { try self.now() }
        context.functions.unicode = environment.unicode
        context.functions.timezones = environment.timezones
        context.functions.hostCall = { try environment.call($0, arguments: $1) }
        context.functions.stateCall = { name, values in
            switch name {
            case "clock_timestamp", "uuidv4", "gen_random_uuid", "uuidv7", "current_setting": return try self.call(name, values)
            default:
                guard let fallback else { throw JedError("55000", "function requires an execution-owned host service: \(name)") }
                return try fallback(name, values)
            }
        }
        context.collatedKey = { try environment.unicode.key($0, collation: $1) }
    }

    /// The expression node owns operator_eval and strict NULL propagation. These
    /// services add only the timezone work of uuidv7's calendar shift.
    func call(_ name: String, _ values: [SQLValue]) throws -> SQLValue {
        if values.contains(where: \.isNull) { return .null }
        switch name {
        case "clock_timestamp":
            guard values.isEmpty else { throw invalidArguments() }
            return .timestamptz(try settings.environment.clockSource.microseconds())
        case "uuidv4", "gen_random_uuid":
            guard values.isEmpty else { throw invalidArguments() }
            var bytes = try randomBytes(16)
            bytes[6] = bytes[6] & 0x0f | 0x40
            bytes[8] = bytes[8] & 0x3f | 0x80
            return .uuid(try SQLUUID(bytes: bytes))
        case "uuidv7":
            guard values.count <= 1 else { throw invalidArguments() }
            var instant = try now()
            if let value = values.first {
                guard case .interval(let shift) = value else { throw invalidArguments() }
                instant = try TemporalFunctions.shift(instant, by: shift, timezone: settings.timeZone, meter: meter)
            }
            let milliseconds = Temporal.floorDiv(instant, 1000)
            guard milliseconds >= 0, milliseconds < 281_474_976_710_656 else { throw JedError("22008", "UUID timestamp is out of range") }
            var bytes: [UInt8] = []
            for shift in stride(from: 40, through: 0, by: -8) { bytes.append(UInt8(truncatingIfNeeded: milliseconds >> shift)) }
            let counter = uuidCounter & 0x0fff
            uuidCounter &+= 1
            bytes.append(0x70 | UInt8(counter >> 8)); bytes.append(UInt8(truncatingIfNeeded: counter))
            bytes.append(contentsOf: try randomBytes(8))
            bytes[8] = bytes[8] & 0x3f | 0x80
            return .uuid(try SQLUUID(bytes: bytes))
        case "current_setting":
            guard values.count == 1 || values.count == 2, case .text(let name) = values[0] else { throw invalidArguments() }
            var missingOK = false
            if values.count == 2 {
                guard case .boolean(let value) = values[1] else { throw invalidArguments() }
                missingOK = value
            }
            return try settings.currentSetting(name, missingOK: missingOK)
        default: throw JedError("42883", "statement service does not exist: \(name)")
        }
    }

    private func randomBytes(_ count: Int) throws -> [UInt8] {
        let bytes = try settings.environment.randomSource.bytes(count: count)
        guard bytes.count == count else { throw JedError("22000", "random source returned an invalid byte count") }
        return bytes
    }
    private func invalidArguments() -> JedError { JedError("42804", "invalid statement service arguments") }
}
