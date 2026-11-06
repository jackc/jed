/// Locale-free proleptic Gregorian calendar and temporal value operations. The
/// epoch is 1970-01-01; Int128 intermediates let completed calendar steps be
/// checked without rejecting a wide intermediate product that later cancels.
internal enum Temporal {
    static let microsPerSecond: Int64 = 1_000_000
    static let microsPerDay: Int64 = 86_400_000_000
    static let postgresEpoch: Int128 = 946_684_800_000_000

    struct Civil: Sendable, Equatable {
        var year: Int64
        var month: Int64
        var day: Int64
        var hour: Int64 = 0
        var minute: Int64 = 0
        var second: Int64 = 0
        var microsecond: Int64 = 0
    }

    static func floorDiv(_ n: Int64, _ d: Int64) -> Int64 {
        n / d - (n < 0 && n % d != 0 ? 1 : 0)
    }
    static func floorMod(_ n: Int64, _ d: Int64) -> Int64 { let r = n % d; return r < 0 ? r + d : r }
    static func isInfinite(_ micros: Int64) -> Bool { micros == .min || micros == .max }
    static func isInfinite(_ days: Int32) -> Bool { days == .min || days == .max }
    static func overflow(_ type: String = "timestamp") -> JedError { JedError("22008", "\(type) out of range") }
    static func finiteTimestamp(_ value: Int128) throws -> Int64 {
        guard value > Int64.min, value < Int64.max else { throw overflow() }
        return Int64(value)
    }
    static func finiteDate(_ value: Int64) throws -> Int32 {
        guard value > Int32.min, value < Int32.max else { throw overflow("date") }
        return Int32(value)
    }
    static func daysFromCivil(year: Int64, month: Int64, day: Int64) -> Int64 {
        let adjusted = year - (month <= 2 ? 1 : 0)
        let era = floorDiv(adjusted, 400)
        let y = adjusted - era * 400
        let shiftedMonth = month + (month > 2 ? -3 : 9)
        let ordinal = (153 * shiftedMonth + 2) / 5 + day - 1
        return era * 146_097 + y * 365 + y / 4 - y / 100 + ordinal - 719_468
    }
    static func civilFromDays(_ days: Int64) -> Civil {
        let z = days + 719_468
        let era = floorDiv(z, 146_097)
        let ordinal = z - era * 146_097
        let y = (ordinal - ordinal / 1460 + ordinal / 36_524 - ordinal / 146_096) / 365
        let year = y + era * 400
        let doy = ordinal - (365 * y + y / 4 - y / 100)
        let shiftedMonth = (5 * doy + 2) / 153
        let month = shiftedMonth + (shiftedMonth < 10 ? 3 : -9)
        let day = doy - (153 * shiftedMonth + 2) / 5 + 1
        return Civil(year: year + (month <= 2 ? 1 : 0), month: month, day: day)
    }
    static func civilFromMicros(_ micros: Int64) -> Civil {
        var result = civilFromDays(floorDiv(micros, microsPerDay))
        let time = floorMod(micros, microsPerDay)
        result.hour = time / 3_600_000_000
        result.minute = time / 60_000_000 % 60
        result.second = time / microsPerSecond % 60
        result.microsecond = time % microsPerSecond
        return result
    }
    static func microsFromCivil(_ civil: Civil) throws -> Int64 { try finiteTimestamp(wideMicros(civil)) }
    static func wideMicros(_ c: Civil) -> Int128 {
        let days = Int128(daysFromCivil(year: c.year, month: c.month, day: c.day))
        return days * Int128(microsPerDay) + Int128(c.hour * 3600 + c.minute * 60 + c.second) * 1_000_000 + Int128(c.microsecond)
    }
    static func daysInMonth(year: Int64, month: Int64) -> Int64 {
        if month == 2 { return year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) ? 29 : 28 }
        return [4, 6, 9, 11].contains(month) ? 30 : 31
    }
    static func validateCivil(_ c: Civil, maximumYear: Int64 = 999_999) throws {
        guard c.year >= 1 - maximumYear, c.year <= maximumYear,
              (1...12).contains(c.month), c.day >= 1, c.day <= daysInMonth(year: c.year, month: c.month),
              (0...59).contains(c.minute), (0...59).contains(c.second), (0...1_000_000).contains(c.microsecond),
              (0...23).contains(c.hour) || (c.hour == 24 && c.minute == 0 && c.second == 0 && c.microsecond == 0) else {
            throw JedError("22008", "date/time field value out of range")
        }
    }

    /// Explicit context separates pure input from statement-clock date words.
    /// A caller passes no clock for bound parameters or container text input.
    static func parse(text: SQLText, type: ScalarType, timezone: SQLTimeZone = .utc,
                      statementMicros: Int64? = nil, typmod: TemporalTypmod? = nil,
                      meter: CostMeter? = nil, sessionDependency: (() -> Void)? = nil) throws -> SQLValue {
        if type == .interval {
            return .interval(try IntervalParser.parse(text.description, typmod: typmod))
        }
        let input = trimmed(text.utf8)
        let lower = String(decoding: input.map(asciiLower), as: UTF8.self)
        if type == .date {
            if lower == "epoch" { return .date(0) }
            if let shift = ["today": Int64(0), "now": 0, "tomorrow": 1, "yesterday": -1][lower], let clock = statementMicros {
                try chargeZone(meter)
                let day = floorDiv(try timezone.instantToLocal(clock), microsPerDay)
                return .date(try finiteDate(day + shift))
            }
        }
        if lower == "infinity" || lower == "+infinity" || lower == "-infinity" {
            let positive = lower != "-infinity"
            switch type {
            case .date: return .date(positive ? .max : .min)
            case .timestamp: return .timestamp(positive ? .max : .min)
            case .timestamptz: return .timestamptz(positive ? .max : .min)
            default: throw JedError("42804", "not a temporal type")
            }
        }
        var parser = TemporalInput(input, type: type.name)
        let parsed = try parser.datetime(maximumYear: type == .date ? 9_999_999 : 999_999)
        if type == .date {
            return .date(try finiteDate(daysFromCivil(year: parsed.civil.year, month: parsed.civil.month, day: parsed.civil.day)))
        }
        var micros: Int64
        if type == .timestamptz, let offset = parsed.offset {
            micros = try finiteTimestamp(wideMicros(parsed.civil) - Int128(offset) * 1_000_000)
        } else {
            micros = try microsFromCivil(parsed.civil)
            if type == .timestamptz {
                sessionDependency?()
                try chargeZone(meter)
                micros = try timezone.localToInstant(micros)
            }
        }
        if let precision = typmod?.precision { micros = try roundedTimestamp(micros, precision: precision) }
        switch type {
        case .timestamp: return .timestamp(micros)
        case .timestamptz: return .timestamptz(micros)
        default: throw JedError("42804", "not a temporal type")
        }
    }

    static func chargeZone(_ meter: CostMeter?) throws {
        meter?.charge(1)
        try meter?.guardCost()
    }
    static func asciiLower(_ byte: UInt8) -> UInt8 { (65...90).contains(byte) ? byte + 32 : byte }
    static func space(_ byte: UInt8) -> Bool { byte == 32 || (9...13).contains(byte) }
    static func trimmed(_ bytes: [UInt8]) -> [UInt8] {
        var a = 0, b = bytes.count
        while a < b && space(bytes[a]) { a += 1 }
        while b > a && space(bytes[b - 1]) { b -= 1 }
        return Array(bytes[a..<b])
    }
    static func pad(_ value: some BinaryInteger, _ width: Int) -> String {
        let digits = String(value)
        return String(repeating: "0", count: max(0, width - digits.utf8.count)) + digits
    }
    static func fraction(_ micros: UInt64) -> String {
        if micros == 0 { return "" }
        var digits = pad(micros, 6)
        while digits.last == "0" { digits.removeLast() }
        return "." + digits
    }
    static func dateText(_ c: Civil) -> String {
        pad(c.year <= 0 ? 1 - c.year : c.year, 4) + "-" + pad(c.month, 2) + "-" + pad(c.day, 2)
    }
    static func offsetText(_ seconds: Int32, iso: Bool = false) -> String {
        let value = Int64(seconds).magnitude
        var text = (seconds < 0 ? "-" : "+") + pad(value / 3600, 2)
        if iso || value % 3600 != 0 { text += ":" + pad(value / 60 % 60, 2) }
        if value % 60 != 0 { text += ":" + pad(value % 60, 2) }
        return text
    }
    static func timestampText(_ value: Int64, offset: Int32? = nil, iso: Bool = false) -> String {
        if value == .min { return "-infinity" }
        if value == .max { return "infinity" }
        let c = civilFromMicros(value)
        let time = pad(c.hour, 2) + ":" + pad(c.minute, 2) + ":" + pad(c.second, 2) + fraction(UInt64(c.microsecond))
        return dateText(c) + (iso ? "T" : " ") + time + (offset.map { offsetText($0, iso: iso) } ?? "") + (c.year <= 0 ? " BC" : "")
    }
    static func render(_ value: SQLValue, timezone: SQLTimeZone = .utc, iso: Bool = false,
                       meter: CostMeter? = nil) throws -> SQLText {
        let result: String
        switch value {
        case .date(let days):
            if days == .min { result = "-infinity" }
            else if days == .max { result = "infinity" }
            else { let c = civilFromDays(Int64(days)); result = dateText(c) + (c.year <= 0 ? " BC" : "") }
        case .timestamp(let micros): result = timestampText(micros, iso: iso)
        case .timestamptz(let micros):
            if isInfinite(micros) { result = timestampText(micros) }
            else {
                try chargeZone(meter)
                let offset = timezone.offset(at: micros).seconds
                let local = try finiteTimestamp(Int128(micros) + Int128(offset) * 1_000_000)
                result = timestampText(local, offset: offset, iso: iso)
            }
        case .interval(let interval): result = intervalText(interval)
        default: throw JedError("42804", "not a temporal value")
        }
        return SQLText(validatedUTF8: Array(result.utf8))
    }
    static func intervalText(_ value: SQLInterval) -> String {
        var parts: [String] = []
        var previousNegative = false
        for (number, singular, plural) in [(value.months / 12, "year", "years"), (value.months % 12, "mon", "mons"), (value.days, "day", "days")] where number != 0 {
            parts.append((number > 0 && previousNegative ? "+" : "") + String(number) + " " + (number == 1 ? singular : plural))
            previousNegative = number < 0
        }
        if value.microseconds != 0 || parts.isEmpty {
            let time = value.microseconds.magnitude
            let prefix = value.microseconds < 0 ? "-" : previousNegative ? "+" : ""
            parts.append(prefix + pad(time / 3_600_000_000, 2) + ":" + pad(time / 60_000_000 % 60, 2) + ":" + pad(time / 1_000_000 % 60, 2) + fraction(time % 1_000_000))
        }
        return parts.joined(separator: " ")
    }
    static func roundedTimestamp(_ value: Int64, precision: UInt8) throws -> Int64 {
        if isInfinite(value) || precision >= 6 { return value }
        let quantum = Int128(pow10(Int(6 - precision)))
        let shifted = Int128(value) - postgresEpoch
        return try finiteTimestamp(roundDivide(shifted, quantum) * quantum + postgresEpoch)
    }
    static func pow10(_ power: Int) -> Int64 { (0..<power).reduce(Int64(1)) { result, _ in result * 10 } }
    static func roundDivide(_ numerator: Int128, _ denominator: Int128, even: Bool = false) -> Int128 {
        precondition(denominator > 0)
        let quotient = numerator / denominator, remainder = numerator % denominator
        let magnitude = remainder.magnitude
        let half = UInt128(denominator) / 2
        let above = magnitude > half || (denominator % 2 == 0 && magnitude == half && (!even || quotient % 2 != 0))
        return above ? quotient + (numerator < 0 ? -1 : 1) : quotient
    }
}

/// A bounded ASCII field reader for the strict ISO timestamp/date grammar.
private struct TemporalInput {
    var bytes: [UInt8]
    let type: String
    var position = 0
    init(_ bytes: [UInt8], type: String) { self.bytes = bytes; self.type = type }
    func invalid() -> JedError { JedError("22007", "invalid input syntax for type \(type)") }
    mutating func take(_ byte: UInt8) -> Bool {
        guard position < bytes.count && bytes[position] == byte else { return false }
        position += 1; return true
    }
    mutating func number() throws -> Int64 {
        let start = position
        var value: Int64 = 0
        while position < bytes.count && (48...57).contains(bytes[position]) {
            value = min(10_000_000_000, value * 10 + Int64(bytes[position] - 48))
            position += 1
        }
        guard position > start else { throw invalid() }
        return value
    }
    mutating func datetime(maximumYear: Int64) throws -> (civil: Temporal.Civil, offset: Int32?) {
        var bc = false
        if bytes.count >= 3 && bytes[bytes.count - 3] == 32 {
            let suffix = bytes.suffix(2).map(Temporal.asciiLower)
            if suffix == [98, 99] || suffix == [97, 100] {
                bc = suffix == [98, 99]
                bytes = Temporal.trimmed(Array(bytes.dropLast(3)))
            }
        }
        let year = try number()
        guard take(45) else { throw invalid() }
        let month = try number()
        guard take(45) else { throw invalid() }
        let day = try number()
        var civil = Temporal.Civil(year: bc ? 1 - year : year, month: month, day: day)
        if take(32) || take(84) || take(116) {
            civil.hour = try number()
            guard take(58) else { throw invalid() }
            civil.minute = try number()
            if take(58) {
                civil.second = try number()
                if take(46) {
                    let start = position
                    var count = 0
                    while position < bytes.count && (48...57).contains(bytes[position]) {
                        let digit = Int64(bytes[position] - 48)
                        if count < 6 { civil.microsecond = civil.microsecond * 10 + digit }
                        if count == 6 && digit >= 5 { civil.microsecond += 1 }
                        count += 1; position += 1
                    }
                    guard position > start else { throw invalid() }
                    if count < 6 { civil.microsecond *= Temporal.pow10(6 - count) }
                }
            }
        }
        var offset: Int32?
        if take(90) || take(122) { offset = 0 }
        else if position < bytes.count && (bytes[position] == 43 || bytes[position] == 45) {
            let sign: Int64 = bytes[position] == 45 ? -1 : 1
            position += 1
            let hour = try number()
            var minute: Int64 = 0, second: Int64 = 0
            if take(58) { minute = try number(); if take(58) { second = try number() } }
            guard hour <= 15 && minute <= 59 && second <= 59 else { throw Temporal.overflow() }
            offset = Int32(sign * (hour * 3600 + minute * 60 + second))
        }
        guard position == bytes.count else { throw invalid() }
        guard year >= 1 && year <= maximumYear else { throw Temporal.overflow(type) }
        try Temporal.validateCivil(civil, maximumYear: maximumYear)
        return (civil, offset)
    }
}

internal enum DateTimeUnits {
    struct Token { let kind: String; let unit: String? }
    static func lookup(_ name: String, includeDate: Bool) -> Token? {
        let key = String(decoding: name.utf8.prefix(10).map(Temporal.asciiLower), as: UTF8.self)
        for table in includeDate ? ["delta", "date"] : ["delta"] {
            guard case .array(let rows) = GeneratedSpec.datetimeUnits[table] else { continue }
            for case .table(let row) in rows {
                guard case .text(let token) = row["token"], token == key,
                      case .text(let kind) = row["type"] else { continue }
                let value: String?
                if case .text(let text) = row["value"] { value = text } else { value = nil }
                return Token(kind: kind, unit: value)
            }
        }
        return nil
    }
    static func leastField(_ code: UInt8) -> String {
        guard case .array(let rows) = GeneratedSpec.datetimeUnits["interval_qualifier"] else { return "second" }
        for case .table(let row) in rows {
            if case .integer(let value) = row["code"], value == code, case .text(let least) = row["least"] { return least }
        }
        return "second"
    }
}

/// PostgreSQL interval input has positional fields and duplicate masks. It is
/// intentionally separate from the strict ISO calendar parser above.
internal enum IntervalParser {
    private enum Failure: Error { case syntax, overflow }
    private static let fractionScale: Int128 = 10_000_000_000_000_000_000_000_000
    private struct Number {
        let whole: Int128
        let fraction: Int128
        let nonzeroFraction: Bool
    }
    private struct Fields {
        var years: Int128 = 0, months: Int128 = 0, days: Int128 = 0, micros: Int128 = 0
        mutating func validate() throws {
            guard years >= Int32.min, years <= Int32.max, months >= Int32.min, months <= Int32.max,
                  days >= Int32.min, days <= Int32.max, micros >= Int64.min, micros <= Int64.max else { throw Failure.overflow }
        }
        mutating func add(_ number: Number, unit: String) throws {
            let whole = number.whole, fraction = number.fraction
            func rounded(_ scale: Int128, even: Bool = false) -> Int128 {
                Temporal.roundDivide(fraction * scale, IntervalParser.fractionScale, even: even)
            }
            switch unit {
            case "millennium", "century", "decade", "year":
                let factor: Int128 = unit == "millennium" ? 1000 : unit == "century" ? 100 : unit == "decade" ? 10 : 1
                years += whole * factor
                months += rounded(factor * 12, even: true)
            case "month":
                months += whole
                let fractionalDays = fraction * 30
                days += fractionalDays / IntervalParser.fractionScale
                micros += Temporal.roundDivide((fractionalDays % IntervalParser.fractionScale) * Int128(Temporal.microsPerDay), IntervalParser.fractionScale)
            case "week", "day":
                let factor: Int128 = unit == "week" ? 7 : 1
                days += whole * factor
                let fractionalDays = fraction * factor
                days += fractionalDays / IntervalParser.fractionScale
                micros += Temporal.roundDivide((fractionalDays % IntervalParser.fractionScale) * Int128(Temporal.microsPerDay), IntervalParser.fractionScale)
            case "hour", "minute", "second", "milliseconds", "microseconds":
                let factor: Int128 = unit == "hour" ? 3_600_000_000 : unit == "minute" ? 60_000_000 : unit == "second" ? 1_000_000 : unit == "milliseconds" ? 1000 : 1
                micros += whole * factor + rounded(factor)
            default: throw Failure.syntax
            }
            try validate()
        }
        func value() throws -> SQLInterval {
            let total = years * 12 + months
            guard total >= Int32.min && total <= Int32.max else { throw Failure.overflow }
            return SQLInterval(months: Int32(total), days: Int32(days), microseconds: Int64(micros))
        }
    }

    static func parse(_ text: String, typmod: TemporalTypmod? = nil) throws -> SQLInterval {
        let fields: Fields
        do {
            do { fields = try standard(text, least: DateTimeUnits.leastField(typmod?.fields ?? 0), minuteSecond: typmod?.fields == 13) }
            catch Failure.syntax { fields = try iso(text) }
            let value = try fields.value()
            return try typmod.map { try adjust(value, typmod: $0) } ?? value
        } catch Failure.syntax { throw JedError("22007", "invalid input syntax for type interval: \"\(text)\"") }
          catch Failure.overflow { throw JedError("22008", "interval field value out of range: \"\(text)\"") }
    }

    static func adjust(_ value: SQLInterval, typmod: TemporalTypmod) throws -> SQLInterval {
        var months = value.months, days = value.days, micros = Int128(value.microseconds)
        switch DateTimeUnits.leastField(typmod.fields) {
        case "year": months = months / 12 * 12; days = 0; micros = 0
        case "month": days = 0; micros = 0
        case "day": micros = 0
        case "hour": micros = micros / 3_600_000_000 * 3_600_000_000
        case "minute": micros = micros / 60_000_000 * 60_000_000
        default: break
        }
        if let precision = typmod.precision, precision < 6 {
            let quantum = Int128(Temporal.pow10(Int(6 - precision)))
            micros = Temporal.roundDivide(micros, quantum) * quantum
        }
        guard micros >= Int64.min && micros <= Int64.max else { throw Temporal.overflow("interval") }
        return SQLInterval(months: months, days: days, microseconds: Int64(micros))
    }

    private static func number(_ text: String) throws -> Number {
        let bytes = Array(text.utf8)
        var index = 0, sign: Int128 = 1
        if bytes.first == 43 || bytes.first == 45 { sign = bytes[0] == 45 ? -1 : 1; index += 1 }
        var whole: Int128 = 0, fraction: Int128 = 0
        var count = 0, fractionalDigits = 0
        while index < bytes.count && (48...57).contains(bytes[index]) {
            whole = whole * 10 + Int128(bytes[index] - 48); index += 1; count += 1
            guard whole <= Int128(Int64.max) + (sign < 0 ? 1 : 0) else { throw Failure.overflow }
        }
        var nonzero = false
        if index < bytes.count && bytes[index] == 46 {
            index += 1
            while index < bytes.count && (48...57).contains(bytes[index]) {
                let digit = Int128(bytes[index] - 48)
                if fractionalDigits < 25 { fraction = fraction * 10 + digit }
                if digit != 0 { nonzero = true }
                fractionalDigits += 1; count += 1; index += 1
            }
        }
        guard count > 0, index == bytes.count else { throw Failure.syntax }
        if fractionalDigits < 25 { for _ in fractionalDigits..<25 { fraction *= 10 } }
        return Number(whole: whole * sign, fraction: fraction * sign, nonzeroFraction: nonzero)
    }

    private static func fields(_ text: String) throws -> [String] {
        let bytes = Array(text.utf8)
        func digit(_ b: UInt8) -> Bool { (48...57).contains(b) }
        func alpha(_ b: UInt8) -> Bool { (65...90).contains(b) || (97...122).contains(b) }
        var index = 0, bufferBytes = 0
        var output: [String] = []
        while index < bytes.count {
            let byte = bytes[index]
            if Temporal.space(byte) { index += 1; continue }
            guard byte >= 32 && byte < 127 else { throw Failure.syntax }
            var field: [UInt8] = []
            if byte == 43 || byte == 45 {
                field.append(byte); index += 1
                while index < bytes.count && Temporal.space(bytes[index]) { index += 1 }
                guard index < bytes.count else { throw Failure.syntax }
                while index < bytes.count && (digit(bytes[index]) || [58, 46, 45].contains(bytes[index]) || alpha(bytes[index]) && field.count == 1) {
                    field.append(Temporal.asciiLower(bytes[index])); index += 1
                    if alpha(field.last!) { while index < bytes.count && alpha(bytes[index]) { field.append(Temporal.asciiLower(bytes[index])); index += 1 }; break }
                }
            } else if digit(byte) || byte == 46 && index + 1 < bytes.count && digit(bytes[index + 1]) {
                let start = index
                while index < bytes.count && (digit(bytes[index]) || bytes[index] == 46) { index += 1 }
                if index < bytes.count && bytes[index] == 58 {
                    while index < bytes.count && (digit(bytes[index]) || bytes[index] == 58 || bytes[index] == 46) { index += 1 }
                } else if index < bytes.count && (bytes[index] == 45 || bytes[index] == 47) {
                    while index < bytes.count && (digit(bytes[index]) || [45, 47, 46].contains(bytes[index])) { index += 1 }
                }
                field = Array(bytes[start..<index])
            } else if alpha(byte) {
                let start = index
                while index < bytes.count && alpha(bytes[index]) { index += 1 }
                let word = String(decoding: bytes[start..<index].map(Temporal.asciiLower), as: UTF8.self)
                let dateToken = DateTimeUnits.lookup(word, includeDate: true)
                if index < bytes.count && ([45, 47, 46].contains(bytes[index]) || (digit(bytes[index]) || bytes[index] == 43) && dateToken == nil) {
                    while index < bytes.count && !Temporal.space(bytes[index]) { index += 1 }
                } else if index < bytes.count && digit(bytes[index]) {
                    // Only date keywords may run into digits in ParseDateTime.
                    // Interval unit names are not date keywords.
                    if !["d", "h", "m", "s", "y", "mm"].contains(word) {
                        while index < bytes.count && !Temporal.space(bytes[index]) { index += 1 }
                    }
                }
                field = bytes[start..<index].map(Temporal.asciiLower)
            } else { index += 1; continue }
            guard !field.isEmpty else { throw Failure.syntax }
            bufferBytes += field.count + 1
            guard output.count < 25 && bufferBytes <= 256 else { throw Failure.syntax }
            output.append(String(decoding: field, as: UTF8.self))
        }
        guard !output.isEmpty else { throw Failure.syntax }
        return output
    }

    private static func standard(_ text: String, least: String, minuteSecond: Bool) throws -> Fields {
        let tokens = try fields(text)
        var result = Fields(), seen: Set<String> = []
        var unit = least, awaiting = false, ago = false
        for index in tokens.indices.reversed() {
            let token = tokens[index]
            if token == "ago" {
                guard index == tokens.count - 1 else { throw Failure.syntax }
                ago = true; awaiting = true; unit = "__ago"; continue
            }
            if token == "at" || token == "on" { unit = "second"; awaiting = false; continue }
            if let first = token.utf8.first, (97...122).contains(first) {
                guard !awaiting || unit == "__ago", token != "m",
                      let resolved = DateTimeUnits.lookup(token, includeDate: true), resolved.kind == "units", let name = resolved.unit else { throw Failure.syntax }
                unit = name; awaiting = true; continue
            }
            var claims: Set<String> = []
            if token.contains(":") {
                result.micros = try time(token, minuteSecond: minuteSecond)
                claims = ["hour", "minute", "second", "milliseconds", "microseconds"]
                unit = "day"
            } else if token.dropFirst(token.first == "+" || token.first == "-" ? 1 : 0).contains("-") {
                let negative = token.first == "-"
                let unsigned = token.dropFirst(token.first == "+" || negative ? 1 : 0)
                guard let split = unsigned.firstIndex(of: "-") else { throw Failure.syntax }
                let year = try number(String(unsigned[..<split]))
                let monthText = String(unsigned[unsigned.index(after: split)...])
                let month = monthText.isEmpty ? Number(whole: 0, fraction: 0, nonzeroFraction: false) : try number(monthText)
                guard !year.nonzeroFraction && !month.nonzeroFraction else { throw Failure.syntax }
                guard month.whole >= 0 && month.whole <= 11 else { throw Failure.overflow }
                result.months += (year.whole * 12 + month.whole) * (negative ? -1 : 1)
                // The SQL-standard field leaves the positional unit at month;
                // a bare number to its left therefore repeats its field mask.
                claims = ["month"]; unit = "month"
            } else {
                let value = try number(token)
                try result.add(value, unit: unit)
                claims = [unit]
                if unit == "second" && value.nonzeroFraction { claims.formUnion(["milliseconds", "microseconds"]) }
                if unit == "hour" { unit = "day" }
            }
            try result.validate()
            guard seen.isDisjoint(with: claims) else { throw Failure.syntax }
            seen.formUnion(claims); awaiting = false
        }
        guard !awaiting && !seen.isEmpty else { throw Failure.syntax }
        if ago { result.years = -result.years; result.months = -result.months; result.days = -result.days; result.micros = -result.micros; try result.validate() }
        return result
    }

    private static func time(_ text: String, minuteSecond: Bool) throws -> Int128 {
        var body = text[...], sign: Int128 = 1
        if body.first == "+" || body.first == "-" { sign = body.first == "-" ? -1 : 1; body = body.dropFirst() }
        let pieces = body.split(separator: ":", omittingEmptySubsequences: false)
        guard pieces.count == 2 || pieces.count == 3 else { throw Failure.syntax }
        let first = try number(String(pieces[0])), last = try number(String(pieces.last!))
        guard !first.nonzeroFraction, first.whole >= 0, last.whole >= 0 else { throw Failure.syntax }
        var hours: Int128 = 0, minutes: Int128 = 0, seconds: Int128 = 0, fraction: Int128 = 0
        if pieces.count == 3 {
            let middle = try number(String(pieces[1]))
            guard !middle.nonzeroFraction, middle.whole >= 0 else { throw Failure.syntax }
            hours = first.whole; minutes = middle.whole; seconds = last.whole; fraction = last.fraction
        } else if minuteSecond || pieces[1].contains(".") {
            minutes = first.whole; seconds = last.whole; fraction = last.fraction
        } else { hours = first.whole; minutes = last.whole }
        guard minutes <= 59, seconds <= 60 else { throw Failure.overflow }
        let result = sign * ((hours * 3600 + minutes * 60 + seconds) * 1_000_000 + Temporal.roundDivide(fraction * 1_000_000, fractionScale))
        guard result >= Int64.min, result <= Int64.max else { throw Failure.overflow }
        return result
    }

    private static func iso(_ text: String) throws -> Fields {
        var parser = ISOInput(bytes: Array(text.utf8))
        return try parser.parse()
    }

    private struct ISOInput {
        let bytes: [UInt8]
        var position = 0
        var peek: UInt8? { position < bytes.count ? bytes[position] : nil }
        mutating func parse() throws -> Fields {
            guard peek == 80 else { throw Failure.syntax }; position += 1
            guard peek != nil else { throw Failure.syntax }
            var result = Fields(), inTime = false, designatorSeen = false
            while peek != nil {
                if peek == 84 { guard !inTime else { throw Failure.syntax }; inTime = true; designatorSeen = false; position += 1; continue }
                let start = position
                let value = try readNumber()
                let token = String(decoding: bytes[start..<position], as: UTF8.self)
                let unit: String?
                switch (peek, inTime) {
                case (89, false): unit = "year"
                case (77, false): unit = "month"
                case (87, false): unit = "week"
                case (68, false): unit = "day"
                case (72, true): unit = "hour"
                case (77, true): unit = "minute"
                case (83, true): unit = "second"
                default: unit = nil
                }
                if let unit { position += 1; designatorSeen = true; try result.add(value, unit: unit); continue }
                guard !designatorSeen else { throw Failure.syntax }
                let integralWidth = token.prefix(while: { $0 >= "0" && $0 <= "9" }).count
                if !inTime && integralWidth == 8 {
                    try result.add(.init(whole: value.whole / 10000, fraction: 0, nonzeroFraction: false), unit: "year")
                    try result.add(.init(whole: value.whole / 100 % 100, fraction: 0, nonzeroFraction: false), unit: "month")
                    try result.add(.init(whole: value.whole % 100, fraction: value.fraction, nonzeroFraction: value.nonzeroFraction), unit: "day")
                } else if inTime && integralWidth == 6 {
                    try result.add(.init(whole: value.whole / 10000, fraction: 0, nonzeroFraction: false), unit: "hour")
                    try result.add(.init(whole: value.whole / 100 % 100, fraction: 0, nonzeroFraction: false), unit: "minute")
                    try result.add(.init(whole: value.whole % 100, fraction: 0, nonzeroFraction: false), unit: "second")
                    try result.add(.init(whole: 0, fraction: value.fraction, nonzeroFraction: value.nonzeroFraction), unit: "microseconds")
                } else {
                    try result.add(value, unit: inTime ? "hour" : "year")
                    let delimiter: UInt8 = inTime ? 58 : 45
                    if peek == delimiter {
                        position += 1; try result.add(readNumber(), unit: inTime ? "minute" : "month")
                        if peek == delimiter { position += 1; try result.add(readNumber(), unit: inTime ? "second" : "day") }
                    }
                }
                guard peek == nil || !inTime && peek == 84 else { throw Failure.syntax }
            }
            return result
        }

        mutating func readNumber() throws -> Number {
            let start = position
            if peek == 45 { position += 1 }
            if position + 2 <= bytes.count, bytes[position] == 48, position + 1 < bytes.count,
               bytes[position + 1] == 120 || bytes[position + 1] == 88 {
                return try readHex(start: start)
            }
            if position + 3 <= bytes.count {
                let word = bytes[position..<position + 3].map(Temporal.asciiLower)
                if word == [105, 110, 102] || word == [110, 97, 110] { throw Failure.overflow }
            }
            var digits = 0
            while let byte = peek, (48...57).contains(byte) { position += 1; digits += 1 }
            if peek == 46 { position += 1; while let byte = peek, (48...57).contains(byte) { position += 1; digits += 1 } }
            guard digits > 0 else { throw Failure.syntax }
            if peek == 101 || peek == 69 {
                position += 1
                if peek == 43 || peek == 45 { position += 1 }
                let exponentStart = position
                while let byte = peek, (48...57).contains(byte) { position += 1 }
                guard position > exponentStart else { throw Failure.syntax }
            }
            let text = String(decoding: bytes[start..<position], as: UTF8.self)
            let decimal: SQLDecimal
            do { decimal = try SQLDecimal.parse(text) } catch { throw Failure.syntax }
            let absolute = decimal.absolute()
            if !decimal.isZero {
                let weight = absolute.precision - absolute.scale - 1
                if weight < -308 || weight > 308 { throw Failure.syntax }
                if weight == -308 {
                    let smallestNormal = try SQLDecimal.parse("2.225073858507201383090232717332404064219e-308")
                    if absolute.compare(to: smallestNormal) < 0 { throw Failure.syntax }
                }
                if weight == 308 {
                    let largestFinite = try SQLDecimal.parse("1.797693134862315708145274237317043567981e308")
                    if absolute.compare(to: largestFinite) > 0 { throw Failure.syntax }
                }
                if absolute.compare(to: SQLDecimal(1_000_000_000_000_000)) > 0 { throw Failure.overflow }
            }
            return try number(decimal.rescaledUncapped(to: min(decimal.scale, 25), round: false).description)
        }

        mutating func readHex(start: Int) throws -> Number {
            let negative = bytes[start] == 45
            position += 2
            var coefficient = BigMagnitude(0), fractionalBits = 0, digitCount = 0, fraction = false
            func hex(_ byte: UInt8) -> UInt32? {
                if (48...57).contains(byte) { return UInt32(byte - 48) }
                if (65...70).contains(byte) { return UInt32(byte - 55) }
                if (97...102).contains(byte) { return UInt32(byte - 87) }
                return nil
            }
            while let byte = peek {
                if byte == 46 && !fraction { fraction = true; position += 1; continue }
                guard let digit = hex(byte) else { break }
                coefficient = coefficient.multiplying(16).adding(BigMagnitude(UInt64(digit)))
                if fraction { fractionalBits += 4 }
                digitCount += 1; position += 1
                guard digitCount <= 256 else { throw Failure.syntax }
            }
            guard digitCount > 0 else { throw Failure.syntax }
            var exponent = -fractionalBits
            if peek == 112 || peek == 80 {
                position += 1; var sign = 1
                if peek == 43 || peek == 45 { if peek == 45 { sign = -1 }; position += 1 }
                var value = 0, count = 0
                while let byte = peek, (48...57).contains(byte) { value = min(100_000, value * 10 + Int(byte - 48)); count += 1; position += 1 }
                guard count > 0 else { throw Failure.syntax }; exponent += sign * value
            }
            if coefficient.isZero { return Number(whole: 0, fraction: 0, nonzeroFraction: false) }
            var normalized = coefficient, lowest = exponent
            while normalized.limbs.first! % 2 == 0 { normalized = normalized.divided(by: 2).quotient; lowest += 1 }
            guard lowest >= -1074, exponent <= 1024 else { throw Failure.syntax }
            if exponent < -1022 {
                // Exact subnormal binary fractions are accepted; any value this
                // small is zero at the parser's fixed 25-decimal-digit guard.
                return Number(whole: 0, fraction: 0, nonzeroFraction: true)
            }
            let numerator: BigMagnitude, denominator: BigMagnitude
            if exponent >= 0 {
                var value = coefficient
                for _ in 0..<exponent { value = value.multiplying(2) }
                numerator = value; denominator = BigMagnitude(1)
            } else {
                var value = BigMagnitude(1)
                for _ in 0..<(-exponent) { value = value.multiplying(2) }
                numerator = coefficient; denominator = value
            }
            let decimal = try SQLDecimal(negative: negative, magnitude: numerator, scale: 0)
                .divided(SQLDecimal(negative: false, magnitude: denominator, scale: 0), scale: 25, round: false)
            guard decimal.absolute().compare(to: SQLDecimal(1_000_000_000_000_000)) <= 0 else { throw Failure.overflow }
            return try number(decimal.description)
        }
    }
}
