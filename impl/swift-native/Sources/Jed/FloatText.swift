/// The input seam performs one correctly rounded conversion at its destination
/// width. Output is handwritten exact-interval selection (float.md §9), with no
/// native floating-point formatter or locale-sensitive parsing.
internal enum FloatText {
    static func parse(_ text: SQLText, type: ScalarType) throws -> SQLValue {
        guard type == .f32 || type == .f64 else { throw JedError("42804", "float input requires a float type") }
        let bytes = text.utf8
        var first = 0, end = bytes.count
        func space(_ byte: UInt8) -> Bool { byte == 32 || (9...13).contains(byte) }
        while first < end && space(bytes[first]) { first += 1 }
        while end > first && space(bytes[end - 1]) { end -= 1 }
        let invalid = JedError("22P02", "invalid input syntax for type \(type == .f32 ? "real" : "double precision")")
        guard first < end else { throw invalid }
        let start = first
        let negative = bytes[first] == 45
        if bytes[first] == 43 || bytes[first] == 45 { first += 1 }
        guard first < end else { throw invalid }
        let word = bytes[first..<end].map { (65...90).contains($0) ? $0 + 32 : $0 }
        if word == Array("nan".utf8) { return type == .f32 ? .f32(0x7fc0_0000) : .f64(0x7ff8_0000_0000_0000) }
        if word == Array("inf".utf8) || word == Array("infinity".utf8) {
            return type == .f32 ? .f32(negative ? 0xff80_0000 : 0x7f80_0000) : .f64(negative ? 0xfff0_0000_0000_0000 : 0x7ff0_0000_0000_0000)
        }
        var digits = 0, nonzero = false
        func consumeDigits() {
            while first < end && (48...57).contains(bytes[first]) {
                digits += 1; nonzero = nonzero || bytes[first] != 48; first += 1
            }
        }
        consumeDigits()
        if first < end && bytes[first] == 46 { first += 1; consumeDigits() }
        guard digits > 0 else { throw invalid }
        if first < end && (bytes[first] == 101 || bytes[first] == 69) {
            first += 1
            if first < end && (bytes[first] == 43 || bytes[first] == 45) { first += 1 }
            let exponentStart = first
            while first < end && (48...57).contains(bytes[first]) { first += 1 }
            guard first > exponentStart else { throw invalid }
        }
        guard first == end else { throw invalid }
        // Arbitrarily large exponents on a spelled zero remain zero. Avoid
        // depending on the host parser's exponent accumulator for this case.
        if !nonzero { return type == .f32 ? .f32(negative ? 0x8000_0000 : 0) : .f64(negative ? 0x8000_0000_0000_0000 : 0) }
        let source = String(decoding: bytes[start..<end], as: UTF8.self)
        if type == .f32 {
            guard let value = Float(source) else { throw FloatFunctions.overflow() }
            guard value.isFinite else { throw FloatFunctions.overflow() }
            guard value != 0 else { throw FloatFunctions.underflow() }
            return .f32(value.bitPattern)
        }
        guard let value = Double(source) else { throw FloatFunctions.overflow() }
        guard value.isFinite else { throw FloatFunctions.overflow() }
        guard value != 0 else { throw FloatFunctions.underflow() }
        return .f64(value.bitPattern)
    }

    static func fromDecimal(_ value: SQLDecimal, type: ScalarType) throws -> SQLValue {
        try parse(SQLText(validatedUTF8: Array(value.description.utf8)), type: type)
    }

    static func render(_ value: SQLValue) throws -> SQLText {
        let negative: Bool, exact: SQLDecimal, previous: SQLDecimal, next: SQLDecimal?, digits: Int, fixedLimit: Int
        switch value {
        case .f64(let bits):
            let absolute = bits & 0x7fff_ffff_ffff_ffff
            negative = bits >> 63 != 0
            if absolute > 0x7ff0_0000_0000_0000 { return SQLText(validatedUTF8: Array("NaN".utf8)) }
            if absolute == 0x7ff0_0000_0000_0000 { return SQLText(validatedUTF8: Array((negative ? "-Infinity" : "Infinity").utf8)) }
            if absolute == 0 { return SQLText(validatedUTF8: Array((negative ? "-0" : "0").utf8)) }
            exact = try BinaryFloatDecimal.exact(bits64: absolute)
            previous = try BinaryFloatDecimal.exact(bits64: absolute - 1)
            next = absolute == 0x7fef_ffff_ffff_ffff ? nil : try BinaryFloatDecimal.exact(bits64: absolute + 1)
            digits = 17; fixedLimit = 15
        case .f32(let bits):
            let absolute = bits & 0x7fff_ffff
            negative = bits >> 31 != 0
            if absolute > 0x7f80_0000 { return SQLText(validatedUTF8: Array("NaN".utf8)) }
            if absolute == 0x7f80_0000 { return SQLText(validatedUTF8: Array((negative ? "-Infinity" : "Infinity").utf8)) }
            if absolute == 0 { return SQLText(validatedUTF8: Array((negative ? "-0" : "0").utf8)) }
            exact = try BinaryFloatDecimal.exact(bits32: absolute)
            previous = try BinaryFloatDecimal.exact(bits32: absolute - 1)
            next = absolute == 0x7f7f_ffff ? nil : try BinaryFloatDecimal.exact(bits32: absolute + 1)
            digits = 9; fixedLimit = 6
        default: throw JedError("42804", "float rendering requires a float value")
        }
        let lower = half(exact.addingUncapped(previous))
        let upper = next.map { half(exact.addingUncapped($0)) } ?? exact.addingUncapped(half(exact.addingUncapped(previous.negated())))
        let selected = try shortest(exact: exact, lower: lower, upper: upper, maximumDigits: digits)
        let output = (negative ? "-" : "") + layout(selected, fixedLimit: fixedLimit)
        return SQLText(validatedUTF8: Array(output.utf8))
    }

    private static func half(_ value: SQLDecimal) -> SQLDecimal {
        SQLDecimal(negative: value.negative, magnitude: value.magnitude.multiplying(5), scale: value.scale + 1)
    }

    private static func shortest(exact: SQLDecimal, lower: SQLDecimal, upper: SQLDecimal, maximumDigits: Int) throws -> SQLDecimal {
        let weight = exact.precision - exact.scale - 1
        for count in 1...maximumDigits {
            let scale = count - weight - 1
            let below = exact.rescaledUncapped(to: scale, round: false)
            let unit = SQLDecimal(negative: false, magnitude: BigMagnitude(1).shiftedDecimal(max(0, -scale)), scale: max(0, scale))
            let above = below.addingUncapped(unit)
            let lowValid = below.compare(to: lower) > 0 && below.compare(to: upper) < 0
            let highValid = above.compare(to: lower) > 0 && above.compare(to: upper) < 0
            if lowValid && highValid {
                let lowDistance = exact.addingUncapped(below.negated())
                let highDistance = above.addingUncapped(exact.negated())
                let comparison = lowDistance.compare(to: highDistance)
                if comparison < 0 { return below }
                if comparison > 0 { return above }
                let coefficient = scale < 0 ? below.magnitude.splitDecimal(-scale).quotient : below.magnitude
                return (coefficient.limbs.first ?? 0) % 2 == 0 ? below : above
            }
            if lowValid { return below }
            if highValid { return above }
        }
        throw JedError("XX000", "no decimal lies inside the float rounding interval")
    }

    private static func layout(_ value: SQLDecimal, fixedLimit: Int) -> String {
        var digits = Array(value.magnitude.description.utf8)
        let point = digits.count - value.scale, exponent = point - 1
        while digits.count > 1 && digits.last == 48 { digits.removeLast() }
        if exponent >= -4 && exponent < fixedLimit {
            if point <= 0 { return "0." + String(repeating: "0", count: -point) + String(decoding: digits, as: UTF8.self) }
            if point >= digits.count { return String(decoding: digits, as: UTF8.self) + String(repeating: "0", count: point - digits.count) }
            return String(decoding: digits[..<point], as: UTF8.self) + "." + String(decoding: digits[point...], as: UTF8.self)
        }
        let fraction = digits.count > 1 ? "." + String(decoding: digits.dropFirst(), as: UTF8.self) : ""
        let exponentDigits = String(abs(exponent))
        return String(decoding: digits.prefix(1), as: UTF8.self) + fraction + "e" + (exponent < 0 ? "-" : "+") + (exponentDigits.count < 2 ? "0" : "") + exponentDigits
    }
}
