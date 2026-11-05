/// Runtime scalar conversion. Resolution admits the pair first; the same input
/// functions serve literals, parameters, container fields, and per-row casts.
internal enum ScalarCasts {
    struct Context {
        var timezone: SQLTimeZone = .utc
        var statementMicros: Int64?
        var meter: CostMeter?
    }

    static func input(_ text: SQLText, to descriptor: TypeDescriptor,
                      explicit: Bool = true, context: Context = Context()) throws -> SQLValue {
        guard case .scalar(let type) = descriptor.type else { throw JedError("42804", "expected a scalar input type") }
        let value: SQLValue
        switch type {
        case .text: value = .text(text)
        case .i16, .i32, .i64: value = .integer(try integer(text, type: type))
        case .decimal: value = .decimal(try SQLDecimal.parse(text.description))
        case .boolean: value = .boolean(try boolean(text))
        case .f32, .f64: value = try FloatText.parse(text, type: type)
        case .uuid: value = .uuid(try SQLUUID(bytes: uuid(text)))
        case .bytea: value = .bytea(try bytea(text))
        case .timestamp, .timestamptz, .date, .interval:
            value = try Temporal.parse(text: text, type: type, timezone: context.timezone,
                                       statementMicros: context.statementMicros, typmod: descriptor.temporal, meter: context.meter)
        case .json, .jsonb: value = try SQLJSON.parse(text: text, preserveText: type == .json)
        case .jsonpath: value = .jsonpath(try JSONPathProgram.compile(text, meter: context.meter).text)
        }
        return try applyTypmod(value, descriptor: descriptor, explicit: explicit)
    }

    static func cast(_ value: SQLValue, from source: ScalarType, to descriptor: TypeDescriptor,
                     mode: TypeRules.CastMode = .explicit, context: Context = Context()) throws -> SQLValue {
        guard case .scalar(let target) = descriptor.type,
              TypeRules.admitsCast(from: .scalar(source), to: descriptor.type, mode: mode) else {
            throw JedError(descriptor.type == .scalar(.jsonpath) ? "42846" : "42804", "cannot cast \(source.name) to \(descriptor.type.name)")
        }
        if case .null = value { return .null }
        let result: SQLValue
        if source == target { result = value }
        else if case .text(let text) = value {
            return try input(text, to: descriptor, explicit: mode == .explicit, context: context)
        } else if target == .text {
            result = .text(try render(value, context: context, growth: true))
        } else {
            switch (value, target) {
            case (.integer(let n), .i16), (.integer(let n), .i32), (.integer(let n), .i64):
                result = .integer(try checkedInteger(n, type: target))
            case (.integer(let n), .decimal): result = .decimal(SQLDecimal(n))
            case (.integer(let n), .boolean): result = .boolean(n != 0)
            case (.boolean(let b), .i32): result = .integer(b ? 1 : 0)
            case (.integer(let n), .f32), (.integer(let n), .f64): result = try FloatText.fromDecimal(SQLDecimal(n), type: target)
            case (.decimal(let n), .i16), (.decimal(let n), .i32), (.decimal(let n), .i64):
                result = .integer(try checkedInteger(n.integerValue(), type: target))
            case (.decimal(let n), .f32), (.decimal(let n), .f64): result = try FloatText.fromDecimal(n, type: target)
            case (.f32(let bits), .f64): result = .f64(ValueCodec.canonicalFloat64(Double(Float(bitPattern: bits)).bitPattern))
            case (.f64(let bits), .f32):
                let n = Double(bitPattern: bits), narrowed = Float(n)
                guard !n.isFinite || narrowed.isFinite else { throw JedError("22003", "f32 out of range") }
                guard n == 0 || narrowed != 0 else { throw JedError("22003", "f32 out of range: underflow") }
                result = .f32(ValueCodec.canonicalFloat32(narrowed.bitPattern))
            case (.f32(let bits), _): result = try decimalCast(BinaryFloatDecimal.exact(bits32: bits), to: target)
            case (.f64(let bits), _): result = try decimalCast(BinaryFloatDecimal.exact(bits64: bits), to: target)
            case (.uuid(let uuid), .bytea): result = .bytea(uuid.bytes)
            case (.bytea(let bytes), .uuid):
                guard bytes.count == 16 else { throw JedError("22P02", "invalid input syntax for type uuid") }
                result = .uuid(try SQLUUID(bytes: bytes))
            case (.json(let text), .jsonb): result = try SQLJSON.parse(text: text)
            case (.jsonb(let json), .json): result = .json(try SQLJSON.render(json, meter: context.meter, growth: true))
            case (.timestamp, .date), (.timestamp, .timestamptz), (.date, .timestamp), (.date, .timestamptz),
                 (.timestamptz, .date), (.timestamptz, .timestamp):
                result = try TemporalFunctions.cast(value, to: target, timezone: context.timezone, meter: context.meter)
            default: throw JedError("42804", "value does not match its cast source")
            }
        }
        return try applyTypmod(result, descriptor: descriptor, explicit: mode == .explicit)
    }

    private static func decimalCast(_ value: SQLDecimal, to target: ScalarType) throws -> SQLValue {
        if target == .decimal { return .decimal(value) }
        return .integer(try checkedInteger(value.integerValue(), type: target))
    }
    static func checkedInteger(_ value: Int64, type: ScalarType) throws -> Int64 {
        guard type.admits(value) else { throw JedError("22003", "\(type.name) out of range") }
        return value
    }

    static func applyTypmod(_ value: SQLValue, descriptor: TypeDescriptor, explicit: Bool) throws -> SQLValue {
        switch value {
        case .decimal(let decimal):
            if let mod = descriptor.decimal { return .decimal(try decimal.coerced(precision: mod.precision, scale: mod.scale)) }
        case .text(let text):
            if let limit = descriptor.textLimit {
                var offset = 0, count = 0
                while offset < text.utf8.count && count < limit {
                    offset += 1
                    while offset < text.utf8.count && text.utf8[offset] & 0xc0 == 0x80 { offset += 1 }
                    count += 1
                }
                if offset < text.utf8.count {
                    guard explicit || text.utf8[offset...].allSatisfy({ $0 == 32 }) else {
                        throw JedError("22001", "value too long for type character varying(\(limit))")
                    }
                    return .text(SQLText(validatedUTF8: Array(text.utf8[..<offset])))
                }
            }
        case .timestamp(let n):
            if let p = descriptor.temporal?.precision { return .timestamp(try Temporal.roundedTimestamp(n, precision: p)) }
        case .timestamptz(let n):
            if let p = descriptor.temporal?.precision { return .timestamptz(try Temporal.roundedTimestamp(n, precision: p)) }
        case .interval(let interval):
            if let mod = descriptor.temporal { return .interval(try IntervalParser.adjust(interval, typmod: mod)) }
        default: break
        }
        return value
    }

    static func render(_ value: SQLValue, context: Context = Context(), growth: Bool = false,
                       containerBoolean: Bool = false) throws -> SQLText {
        let text: String
        switch value {
        case .integer(let n): text = String(n)
        case .boolean(let b): text = containerBoolean ? (b ? "t" : "f") : (b ? "true" : "false")
        case .decimal(let n): text = n.description
        case .text(let s), .json(let s), .jsonpath(let s): return s
        case .jsonb(let json): return try SQLJSON.render(json, meter: context.meter, growth: growth)
        case .f32, .f64: return try FloatText.render(value)
        case .timestamp, .timestamptz, .date, .interval: return try Temporal.render(value, timezone: context.timezone, meter: context.meter)
        case .bytea(let bytes):
            let size = saturatingAdd(2, saturatingAdd(Int64(bytes.count), Int64(bytes.count)))
            if growth { context.meter?.charge(size); try context.meter?.guardCost() }
            let digits = Array("0123456789abcdef".utf8)
            var output: [UInt8] = [92, 120]
            output.reserveCapacity(Int(size))
            for byte in bytes { output += [digits[Int(byte >> 4)], digits[Int(byte & 15)]] }
            return SQLText(validatedUTF8: output)
        case .uuid(let uuid):
            let bytes = uuid.bytes
            guard bytes.count == 16 else { throw JedError.corruption("invalid uuid width") }
            let digits = Array("0123456789abcdef".utf8)
            var output: [UInt8] = []
            for (index, byte) in bytes.enumerated() {
                if index == 4 || index == 6 || index == 8 || index == 10 { output.append(45) }
                output += [digits[Int(byte >> 4)], digits[Int(byte & 15)]]
            }
            return SQLText(validatedUTF8: output)
        default: throw JedError("42804", "cannot render a non-scalar value as a scalar")
        }
        return SQLText(validatedUTF8: Array(text.utf8))
    }

    static func integer(_ text: SQLText, type: ScalarType) throws -> Int64 {
        let bytes = text.utf8
        var p = 0
        func malformed() -> JedError { JedError("22P02", "invalid input syntax for type \(type.name): \"\(text)\"") }
        func overflow() -> JedError { JedError("22003", "value \"\(text)\" is out of range for type \(type.name)") }
        while p < bytes.count && Temporal.space(bytes[p]) { p += 1 }
        var negative = false
        if p < bytes.count && (bytes[p] == 43 || bytes[p] == 45) { negative = bytes[p] == 45; p += 1 }
        var radix: UInt64 = 10
        if p + 1 < bytes.count && bytes[p] == 48 {
            switch bytes[p + 1] | 32 {
            case 120: radix = 16
            case 111: radix = 8
            case 98: radix = 2
            default: break
            }
            if radix != 10 { p += 2; if p < bytes.count && bytes[p] == 95 { p += 1 } }
        }
        let limit: UInt64 = type == .i16 ? 32_768 : type == .i32 ? 2_147_483_648 : 9_223_372_036_854_775_808
        var value: UInt64 = 0, seen = false
        while p < bytes.count, let digit = hex(bytes[p]), UInt64(digit) < radix {
            // PostgreSQL checks the pre-multiply magnitude while scanning; final
            // signed range checking comes after trailing-input validation.
            guard value <= limit / radix else { throw overflow() }
            value = value * radix + UInt64(digit); p += 1; seen = true
            if p < bytes.count && bytes[p] == 95 {
                p += 1
                guard p < bytes.count, let next = hex(bytes[p]), UInt64(next) < radix else { throw malformed() }
            }
        }
        guard seen else { throw malformed() }
        while p < bytes.count && Temporal.space(bytes[p]) { p += 1 }
        guard p == bytes.count else { throw malformed() }
        guard value <= limit, negative || value < limit else { throw overflow() }
        if value == 9_223_372_036_854_775_808 { return .min }
        return negative ? -Int64(value) : Int64(value)
    }
    static func boolean(_ text: SQLText) throws -> Bool {
        let input = Temporal.trimmed(text.utf8).map(Temporal.asciiLower)
        if input == [49] { return true }; if input == [48] { return false }
        var result: Bool?
        if !input.isEmpty {
            for (word, value) in [("true", true), ("false", false), ("yes", true), ("no", false), ("on", true), ("off", false)] {
                if Array(word.utf8).starts(with: input) {
                    if result != nil { throw JedError("22P02", "invalid input syntax for type boolean: \"\(text)\"") }
                    result = value
                }
            }
        }
        guard let result else { throw JedError("22P02", "invalid input syntax for type boolean: \"\(text)\"") }
        return result
    }
    static func hex(_ byte: UInt8) -> UInt8? {
        if (48...57).contains(byte) { return byte - 48 }
        let lower = byte | 32
        return (97...102).contains(lower) ? lower - 87 : nil
    }
    static func uuid(_ text: SQLText) throws -> [UInt8] {
        let bytes = text.utf8
        func invalid() -> JedError { JedError("22P02", "invalid input syntax for type uuid: \"\(text)\"") }
        var p = 0, output: [UInt8] = []
        let braced = bytes.first == 123
        if braced { p += 1 }
        for index in 0..<16 {
            guard p + 1 < bytes.count, let a = hex(bytes[p]), let b = hex(bytes[p + 1]) else { throw invalid() }
            output.append(a << 4 | b); p += 2
            if index < 15 && index % 2 == 1 && p < bytes.count && bytes[p] == 45 { p += 1 }
        }
        if braced { guard p < bytes.count && bytes[p] == 125 else { throw invalid() }; p += 1 }
        guard p == bytes.count else { throw invalid() }
        return output
    }
    static func bytea(_ text: SQLText) throws -> [UInt8] {
        let bytes = text.utf8
        if bytes.starts(with: [92, 120]) { return try decodeHex(Array(bytes.dropFirst(2)), code: "22P02") }
        return try decodeEscape(bytes, code: "22P02")
    }
    static func decodeHex(_ bytes: [UInt8], code: String = "22023") throws -> [UInt8] {
        var p = 0, output: [UInt8] = []
        func invalid(_ at: Int) -> JedError {
            var end = at + 1
            while end < bytes.count && bytes[end] & 0xc0 == 0x80 { end += 1 }
            return JedError(code, "invalid hexadecimal digit: \"\(String(decoding: bytes[at..<end], as: UTF8.self))\"")
        }
        while p < bytes.count {
            if bytes[p] == 32 || bytes[p] == 9 || bytes[p] == 10 || bytes[p] == 13 { p += 1; continue }
            guard let a = hex(bytes[p]) else { throw invalid(p) }; p += 1
            guard p < bytes.count else { throw JedError(code, "invalid hexadecimal data: odd number of digits") }
            guard let b = hex(bytes[p]) else { throw invalid(p) }; p += 1
            output.append(a << 4 | b)
        }
        return output
    }
    static func decodeEscape(_ bytes: [UInt8], code: String = "22023") throws -> [UInt8] {
        var p = 0, output: [UInt8] = []
        while p < bytes.count {
            if bytes[p] != 92 { output.append(bytes[p]); p += 1; continue }
            if p + 1 < bytes.count && bytes[p + 1] == 92 { output.append(92); p += 2; continue }
            guard p + 3 < bytes.count, (48...51).contains(bytes[p + 1]), (48...55).contains(bytes[p + 2]), (48...55).contains(bytes[p + 3]) else {
                throw JedError(code, "invalid input syntax for type bytea")
            }
            output.append((bytes[p + 1] - 48) * 64 + (bytes[p + 2] - 48) * 8 + (bytes[p + 3] - 48)); p += 4
        }
        return output
    }
}
