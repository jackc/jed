/// Exact finite decimal. Display scale is retained independently of value
/// equality, and every rounding operation uses half away from zero.
public struct SQLDecimal: Sendable, CustomStringConvertible, Equatable {
    public static let maximumPrecision = 1000
    public static let maximumIntegerDigits = 131_072
    public static let maximumScale = 16_383

    public let negative: Bool
    public let scale: Int
    public let limbs: [UInt32]

    internal var magnitude: BigMagnitude { BigMagnitude(limbs) }
    public var isZero: Bool { limbs.isEmpty }
    internal var precision: Int { isZero ? 0 : magnitude.decimalDigits }
    internal var base10000Weight: Int { isZero ? 0 : Self.floorDiv(precision - 1 - scale, 4) }
    /// Number of stored base-10^4 groups, independent of the internal limb base.
    public var canonicalDigitGroupCount: Int { isZero ? 0 : (magnitude.decimalDigits + 3) / 4 }

    public init(_ value: Int64) {
        self.init(negative: value < 0, magnitude: BigMagnitude(value.magnitude), scale: 0)
    }

    internal init(negative: Bool, magnitude: BigMagnitude, scale: Int) {
        precondition(scale >= 0)
        self.negative = negative && !magnitude.isZero
        self.scale = scale
        limbs = magnitude.limbs
    }

    internal func checked() throws -> Self {
        guard scale <= Self.maximumScale,
              isZero || magnitude.decimalDigits - scale <= Self.maximumIntegerDigits else {
            throw JedError("22003", "numeric value out of range")
        }
        return self
    }

    public static func parse(_ text: String) throws -> Self {
        let input = Array(text.utf8)
        func isSpace(_ byte: UInt8) -> Bool { byte == 32 || (9...13).contains(byte) }
        func digit(_ byte: UInt8) -> Int? {
            if (48...57).contains(byte) { return Int(byte - 48) }
            if (65...70).contains(byte) { return Int(byte - 55) }
            if (97...102).contains(byte) { return Int(byte - 87) }
            return nil
        }
        let invalid = JedError("22P02", "invalid input syntax for type numeric")
        var start = 0
        var end = input.count
        while start < end && isSpace(input[start]) { start += 1 }
        while end > start && isSpace(input[end - 1]) { end -= 1 }
        guard start < end else { throw invalid }
        var negative = false
        if input[start] == 43 || input[start] == 45 {
            negative = input[start] == 45
            start += 1
        }
        guard start < end else { throw invalid }

        if end - start >= 2 && input[start] == 48 {
            let radix: Int
            switch input[start + 1] {
            case 120, 88: radix = 16
            case 111, 79: radix = 8
            case 98, 66: radix = 2
            default: radix = 0
            }
            if radix != 0 {
                var position = start + 2
                if position < end && input[position] == 95 { position += 1 }
                var digits: [UInt32] = []
                var lastWasDigit = false
                while position < end {
                    let byte = input[position]
                    if byte == 95 {
                        guard lastWasDigit, position + 1 < end,
                              let next = digit(input[position + 1]), next < radix else { throw invalid }
                        lastWasDigit = false
                    } else {
                        guard let value = digit(byte), value < radix else { throw invalid }
                        digits.append(UInt32(value))
                        lastWasDigit = true
                    }
                    position += 1
                }
                guard lastWasDigit else { throw invalid }
                var value = BigMagnitude(0)
                for digit in digits {
                    value = value.multiplying(UInt32(radix)).adding(BigMagnitude(UInt64(digit)))
                    if value.decimalDigits > maximumIntegerDigits {
                        throw JedError("22003", "numeric value out of range")
                    }
                }
                return Self(negative: negative, magnitude: value, scale: 0)
            }
        }

        var position = start
        var digits: [UInt8] = []
        var fractionalDigits = 0
        var pointSeen = false
        var lastWasDigit = false
        while position < end {
            let byte = input[position]
            if (48...57).contains(byte) {
                digits.append(byte)
                if pointSeen { fractionalDigits += 1 }
                lastWasDigit = true
            } else if byte == 95 {
                guard lastWasDigit, position + 1 < end,
                      (48...57).contains(input[position + 1]) else { throw invalid }
                lastWasDigit = false
            } else if byte == 46 && !pointSeen {
                pointSeen = true
                lastWasDigit = false
            } else { break }
            position += 1
        }
        guard !digits.isEmpty else { throw invalid }
        var exponent = 0
        if position < end && (input[position] == 101 || input[position] == 69) {
            position += 1
            var exponentNegative = false
            if position < end && (input[position] == 43 || input[position] == 45) {
                exponentNegative = input[position] == 45
                position += 1
            }
            var exponentDigits = 0
            var lastExponentDigit = false
            let exponentLimit = maximumIntegerDigits + maximumScale + 2
            while position < end {
                let byte = input[position]
                if (48...57).contains(byte) {
                    exponent = min(exponentLimit, exponent * 10 + Int(byte - 48))
                    exponentDigits += 1
                    lastExponentDigit = true
                } else if byte == 95 {
                    guard lastExponentDigit, position + 1 < end,
                          (48...57).contains(input[position + 1]) else { throw invalid }
                    lastExponentDigit = false
                } else { throw invalid }
                position += 1
            }
            guard exponentDigits > 0, lastExponentDigit else { throw invalid }
            if exponentNegative { exponent = -exponent }
        }
        guard position == end else { throw invalid }
        let resultScale = fractionalDigits - exponent
        guard resultScale <= maximumScale else { throw JedError("22003", "numeric value out of range") }
        let firstNonzero = digits.firstIndex(where: { $0 != 48 }) ?? digits.endIndex
        let significant = digits[firstNonzero...]
        if !significant.isEmpty && significant.count - resultScale > maximumIntegerDigits {
            throw JedError("22003", "numeric value out of range")
        }
        var value = BigMagnitude(decimalDigits: significant)
        if resultScale < 0 { value = value.shiftedDecimal(-resultScale) }
        return Self(negative: negative, magnitude: value, scale: max(0, resultScale))
    }

    public var description: String {
        let digits = magnitude.description
        let prefix = negative ? "-" : ""
        if scale == 0 { return prefix + digits }
        if digits.utf8.count <= scale {
            return prefix + "0." + String(repeating: "0", count: scale - digits.utf8.count) + digits
        }
        let split = digits.index(digits.endIndex, offsetBy: -scale)
        return prefix + digits[..<split] + "." + digits[split...]
    }

    public static func == (left: Self, right: Self) -> Bool { left.compare(to: right) == 0 }

    public func compare(to other: Self) -> Int {
        if isZero && other.isZero { return 0 }
        if negative != other.negative { return negative ? -1 : 1 }
        let target = max(scale, other.scale)
        let result = magnitude.shiftedDecimal(target - scale)
            .compare(to: other.magnitude.shiftedDecimal(target - other.scale))
        return negative ? -result : result
    }

    public func negated() -> Self { Self(negative: !negative, magnitude: magnitude, scale: scale) }
    public func absolute() -> Self { Self(negative: false, magnitude: magnitude, scale: scale) }

    /// Aggregate transitions may exceed stored precision before the final result.
    internal func addingUncapped(_ other: Self) -> Self {
        let target = max(scale, other.scale)
        let a = magnitude.shiftedDecimal(target - scale)
        let b = other.magnitude.shiftedDecimal(target - other.scale)
        if negative == other.negative {
            return Self(negative: negative, magnitude: a.adding(b), scale: target)
        }
        if a.compare(to: b) >= 0 {
            return Self(negative: negative, magnitude: a.subtracting(b), scale: target)
        }
        return Self(negative: other.negative, magnitude: b.subtracting(a), scale: target)
    }

    public func adding(_ other: Self) throws -> Self { try addingUncapped(other).checked() }
    public func subtracting(_ other: Self) throws -> Self { try addingUncapped(other.negated()).checked() }

    public func multiplying(_ other: Self) throws -> Self {
        let result = Self(negative: negative != other.negative,
                          magnitude: magnitude.multiplying(other.magnitude), scale: scale + other.scale)
        if result.scale > Self.maximumScale { return try result.rounded(to: Self.maximumScale) }
        return try result.checked()
    }

    internal var leadingBase10000: (weight: Int, digit: Int) {
        if isZero { return (0, 0) }
        let exponent = magnitude.decimalDigits - 1 - scale
        let weight = Self.floorDiv(exponent, 4)
        let shift = 4 * weight + scale
        let head = shift >= 0 ? magnitude.splitDecimal(shift).quotient : magnitude.shiftedDecimal(-shift)
        return (weight, Int(head.limbs.first ?? 0))
    }

    internal func divisionScale(_ other: Self) -> Int {
        let a = leadingBase10000
        let b = other.leadingBase10000
        let weight = a.weight - b.weight - (a.digit <= b.digit ? 1 : 0)
        return min(Self.maximumPrecision, max(0, scale, other.scale, 16 - 4 * weight))
    }

    public func dividing(_ other: Self) throws -> Self {
        try divided(other, scale: divisionScale(other), round: true).checked()
    }

    /// Exact-scale division is also the arithmetic primitive for guarded kernels.
    internal func divided(_ other: Self, scale target: Int, round: Bool) throws -> Self {
        guard !other.isZero else { throw JedError("22012", "division by zero") }
        precondition(target >= 0)
        let shift = target + other.scale - scale
        let numerator = shift >= 0 ? magnitude.shiftedDecimal(shift) : magnitude
        let denominator = shift < 0 ? other.magnitude.shiftedDecimal(-shift) : other.magnitude
        let division = numerator.divided(by: denominator)
        var quotient = division.quotient
        if round && division.remainder.multiplying(2).compare(to: denominator) >= 0 {
            quotient = quotient.adding(BigMagnitude(1))
        }
        return Self(negative: negative != other.negative, magnitude: quotient, scale: target)
    }

    public func modulo(_ other: Self) throws -> Self {
        guard !other.isZero else { throw JedError("22012", "division by zero") }
        let target = max(scale, other.scale)
        let a = magnitude.shiftedDecimal(target - scale)
        let b = other.magnitude.shiftedDecimal(target - other.scale)
        return try Self(negative: negative, magnitude: a.divided(by: b).remainder, scale: target).checked()
    }

    public func rounded(to requestedScale: Int) throws -> Self {
        try rescaled(to: requestedScale, round: true).checked()
    }

    public func truncated(to requestedScale: Int = 0) throws -> Self {
        try rescaled(to: requestedScale, round: false).checked()
    }

    internal func rescaled(to requestedScale: Int, round: Bool) -> Self {
        let target = max(-Self.maximumIntegerDigits - 1, min(Self.maximumScale, requestedScale))
        return rescaledUncapped(to: target, round: round)
    }

    internal func rescaledUncapped(to target: Int, round: Bool = true) -> Self {
        if target >= scale {
            return Self(negative: negative, magnitude: magnitude.shiftedDecimal(target - scale), scale: target)
        }
        let cut = scale - target
        let split = magnitude.splitDecimal(cut)
        var result = split.quotient
        if round && !split.remainder.isZero {
            // Compare only when enough digits can reach half: this avoids a
            // giant power of ten for extreme negative rounding scales.
            let twice = split.remainder.multiplying(2)
            if twice.decimalDigits > cut {
                result = result.adding(BigMagnitude(1))
            }
        }
        if target < 0 { result = result.shiftedDecimal(-target) }
        return Self(negative: negative, magnitude: result, scale: max(0, target))
    }

    internal func multiplyingUncapped(_ other: Self) -> Self {
        Self(negative: negative != other.negative, magnitude: magnitude.multiplying(other.magnitude),
             scale: scale + other.scale)
    }

    internal func multiplying(_ other: Self, scale: Int) -> Self {
        multiplyingUncapped(other).rescaledUncapped(to: scale)
    }

    public func coerced(precision: Int, scale target: Int) throws -> Self {
        guard (1...Self.maximumPrecision).contains(precision), target >= 0, target <= precision else {
            throw JedError("22023", "invalid numeric precision or scale")
        }
        let result = try rounded(to: target)
        if !result.isZero && result.magnitude.decimalDigits - target > precision - target {
            throw JedError("22003", "numeric field overflow")
        }
        return result
    }

    public func integerValue() throws -> Int64 {
        let rounded = try rounded(to: 0)
        guard let result = Int64(rounded.description) else { throw JedError("22003", "integer out of range") }
        return result
    }

    /// Canonical numeric-value key, independent of display scale. NULL framing
    /// and per-index descending options are composed by the caller.
    internal func keyBytes() -> [UInt8] {
        if isZero { return [4] }
        var digits = Array(magnitude.description.utf8)
        let decimalPoint = digits.count - scale
        while digits.last == 48 { digits.removeLast() }
        if decimalPoint % 2 != 0 { digits.insert(48, at: 0) }
        if digits.count % 2 != 0 { digits.append(48) }
        let exponent = Int32(Self.floorDiv(decimalPoint + 1, 2))
        var writer = ByteWriter()
        writer.u32(UInt32(bitPattern: exponent) ^ 0x8000_0000)
        for i in stride(from: 0, to: digits.count, by: 2) {
            writer.u8((digits[i] - 48) * 10 + digits[i + 1] - 48 + 1)
        }
        writer.u8(0)
        return [negative ? 3 : 5] + (negative ? writer.bytes.map { ~$0 } : writer.bytes)
    }

    /// The inline plain value body. Presence/external/compressed tags live at
    /// the enclosing record layer, not in this decimal-specific codec.
    internal func encodedBody() throws -> [UInt8] {
        _ = try checked()
        let digits = Array(magnitude.description.utf8)
        var groups: [UInt16] = []
        if !isZero {
            var position = 0
            var length = (digits.count - 1) % 4 + 1
            while position < digits.count {
                var group: UInt16 = 0
                for byte in digits[position..<(position + length)] { group = group * 10 + UInt16(byte - 48) }
                groups.append(group)
                position += length
                length = 4
            }
        }
        var writer = ByteWriter()
        writer.u8(negative ? 1 : 0)
        writer.u16(UInt16(scale))
        writer.u16(UInt16(groups.count))
        for group in groups { writer.u16(group) }
        return writer.bytes
    }

    internal static func decodeBody(_ bytes: [UInt8]) throws -> Self {
        var reader = ByteReader(bytes)
        let result = try decodeBody(from: &reader)
        try reader.requireEnd()
        return result
    }

    internal static func decodeBody(from reader: inout ByteReader) throws -> Self {
        let flags = try reader.u8()
        let scale = Int(try reader.u16())
        let count = Int(try reader.u16())
        guard flags <= 1, scale <= maximumScale,
              count <= (maximumIntegerDigits + scale + 3) / 4,
              count > 0 || flags == 0 else { throw JedError.corruption("invalid decimal header") }
        var digits: [UInt8] = []
        digits.reserveCapacity(count * 4)
        for i in 0..<count {
            let group = try reader.u16()
            guard group < 10_000, i != 0 || group != 0 else { throw JedError.corruption("invalid decimal digit group") }
            let text = Array(String(group).utf8)
            if i != 0 { digits.append(contentsOf: repeatElement(UInt8(48), count: 4 - text.count)) }
            digits.append(contentsOf: text)
        }
        let result = Self(negative: flags == 1, magnitude: BigMagnitude(decimalDigits: digits[...]), scale: scale)
        do { return try result.checked() }
        catch { throw JedError.corruption("decimal exceeds the numeric format limits") }
    }

    internal static func floorDiv(_ numerator: Int, _ denominator: Int) -> Int {
        let quotient = numerator / denominator
        return numerator < 0 && numerator % denominator != 0 ? quotient - 1 : quotient
    }
}
