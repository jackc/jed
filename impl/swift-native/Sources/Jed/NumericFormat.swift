/// Exact binary expansion shared by formatting and explicit float-to-decimal
/// casts. The host float is only decoded into its IEEE sign/exponent/mantissa.
internal enum BinaryFloatDecimal {
    static func exact(bits64 bits: UInt64) throws -> SQLDecimal {
        let exponent = Int((bits >> 52) & 2047), fraction = bits & 0x000f_ffff_ffff_ffff
        guard exponent != 2047 else { throw JedError("22003", "cannot convert infinity or NaN to numeric") }
        return expand(mantissa: exponent == 0 ? fraction : fraction | (1 << 52), exponent: exponent == 0 ? -1074 : exponent - 1075, negative: bits >> 63 != 0)
    }
    static func exact(bits32 bits: UInt32) throws -> SQLDecimal {
        let exponent = Int((bits >> 23) & 255), fraction = bits & 0x007f_ffff
        guard exponent != 255 else { throw JedError("22003", "cannot convert infinity or NaN to numeric") }
        return expand(mantissa: UInt64(exponent == 0 ? fraction : fraction | (1 << 23)), exponent: exponent == 0 ? -149 : exponent - 150, negative: bits >> 31 != 0)
    }
    private static func expand(mantissa: UInt64, exponent: Int, negative: Bool) -> SQLDecimal {
        if mantissa == 0 { return SQLDecimal(0) }
        var magnitude = BigMagnitude(mantissa), scale = 0
        if exponent >= 0 { for _ in 0..<exponent { magnitude = magnitude.multiplying(2) } }
        else {
            scale = -exponent
            for _ in 0..<scale { magnitude = magnitude.multiplying(5) }
            while scale > 0 {
                let division = magnitude.divided(by: UInt32(10))
                if division.remainder != 0 { break }
                magnitude = division.quotient; scale -= 1
            }
        }
        return SQLDecimal(negative: negative, magnitude: magnitude, scale: scale)
    }
    static func roundedEven(_ value: SQLDecimal, scale: Int) -> SQLDecimal {
        if scale >= value.scale { return value.rescaledUncapped(to: scale) }
        let discarded = value.scale - scale
        let parts = value.magnitude.splitDecimal(discarded)
        let comparison = parts.remainder.multiplying(2).compare(to: BigMagnitude(1).shiftedDecimal(discarded))
        let odd = (parts.quotient.limbs.first ?? 0) % 2 != 0
        var coefficient = comparison > 0 || comparison == 0 && odd ? parts.quotient.adding(BigMagnitude(1)) : parts.quotient
        if scale < 0 { coefficient = coefficient.shiftedDecimal(-scale) }
        return SQLDecimal(negative: value.negative, magnitude: coefficient, scale: max(0, scale))
    }
}

internal enum NumericFormat {
    private struct Descriptor {
        var before = 0, after = 0, multiplier = 0
        var decimal = false, fill = false, scale = false, scientific = false
        var roman = false, lowerRoman = false
        var sign = false, signAfter = false, signAt = 0
        var minus = false, plus = false, signed = false, brackets = false
        var firstZero: Int?, lastZero = 0
        var nodes: [FormatTemplate.Node]
        init(_ template: SQLText) throws {
            nodes = FormatTemplate.parse(template, datetime: false)
            var digitSeen = false
            for node in nodes {
                guard let id = node.keyword?.id else { continue }
                if scientific { throw JedError("42601", "EEEE must be the last pattern used") }
                switch id {
                case "9", "0":
                    if brackets { throw JedError("42601", "PR must be ahead of digits") }
                    digitSeen = true
                    if scale { multiplier += 1 }
                    else if decimal { after += 1 }
                    else { before += 1 }
                    if id == "0" {
                        let position = before + after + multiplier
                        if firstZero == nil { firstZero = position }
                        lastZero = position
                    }
                case "DEC", "D":
                    if decimal { throw JedError("42601", "multiple decimal points") }
                    if scale { throw JedError("42601", "cannot use V and decimal point together") }
                    decimal = true
                case "V":
                    if decimal { throw JedError("42601", "cannot use V and decimal point together") }
                    scale = true
                case "FM": fill = true
                case "S":
                    if sign { throw JedError("42601", "cannot use S twice") }
                    if minus || plus || signed || brackets { throw JedError("42601", "cannot use S and MI, PL, SG or PR together") }
                    sign = true; signAt = before; signAfter = decimal
                case "MI", "PL", "SG":
                    if sign { throw JedError("42601", "cannot use S and MI, PL, SG or PR together") }
                    if id == "MI" { minus = true }; if id == "PL" { plus = true }; if id == "SG" { signed = true }
                case "PR":
                    if sign { throw JedError("42601", "cannot use S and MI, PL, SG or PR together") }
                    brackets = true
                    if !digitSeen { throw JedError("42601", "PR must be ahead of digits") }
                case "RN", "rn":
                    if roman { throw JedError("42601", "cannot use RN twice") }
                    roman = true; lowerRoman = id == "rn"
                case "E":
                    if fill || scale || roman || sign || minus || plus || signed || brackets || firstZero != nil { throw JedError("42601", "EEEE is incompatible with other formats") }
                    scientific = true
                default: break
                }
            }
            if roman {
                for node in nodes {
                    if let id = node.keyword?.id, !["RN", "rn", "FM", "9"].contains(id) { throw JedError("42601", "cannot use RN with other formats") }
                }
            }
            if sign && !signAfter && signAt == before { signAfter = true }
        }
    }
    private static func roman(_ number: Int64, lower: Bool = false) -> String {
        guard number > 0 && number < 4000 else { return String(repeating: "#", count: 15) }
        let n = Int(number)
        var output = String(repeating: "M", count: n / 1000)
        if n / 100 % 10 > 0 { output += FormatTemplate.words("rn_hundreds")[n / 100 % 10 - 1] }
        if n / 10 % 10 > 0 { output += FormatTemplate.words("rn_tens")[n / 10 % 10 - 1] }
        if n % 10 > 0 { output += FormatTemplate.words("rn_units")[n % 10 - 1] }
        return lower ? output.lowercased() : output
    }
    private static func shifted(_ value: SQLDecimal, by digits: Int) -> SQLDecimal {
        if digits >= value.scale { return SQLDecimal(negative: value.negative, magnitude: value.magnitude.shiftedDecimal(digits - value.scale), scale: 0) }
        return SQLDecimal(negative: value.negative, magnitude: value.magnitude, scale: value.scale - digits)
    }
    private static func absolute(_ value: SQLDecimal) -> SQLDecimal { SQLDecimal(negative: false, magnitude: value.magnitude, scale: value.scale) }
    private static func scientific(_ value: SQLDecimal, precision: Int, negative: Bool, even: Bool) throws -> String {
        var exponent = value.isZero ? 0 : value.precision - value.scale - 1
        let normalized = shifted(absolute(value), by: -exponent)
        var digits = even ? BinaryFloatDecimal.roundedEven(normalized, scale: min(350, precision)) : try normalized.rounded(to: precision)
        if even && digits.compare(to: SQLDecimal(10)) >= 0 {
            exponent += 1; digits = shifted(digits, by: -1).rescaledUncapped(to: min(350, precision))
        }
        var text = digits.description
        if even && precision > 350 { text += String(repeating: "0", count: precision - 350) }
        return (negative ? "-" : " ") + text + "e" + (exponent < 0 ? "-" : "+") + FormatTemplate.padded(Int64(abs(exponent)), width: 2)
    }
    private struct Prepared {
        let digits: [UInt8]
        let negative: Bool
        let integerDigits: Int
        let overflow: Bool
        let roman: String?
    }
    private static func prepare(_ value: SQLValue, type: ScalarType, descriptor: Descriptor) throws -> Prepared {
        var decimal: SQLDecimal, negative: Bool, floatBits: UInt64? = nil, float32 = false, special: String? = nil
        switch value {
        case .integer(let n): decimal = SQLDecimal(n); negative = n < 0
        case .decimal(let n): decimal = n; negative = n.negative
        case .f32(let bits):
            float32 = true; let f = Float(bitPattern: bits)
            floatBits = Double(f).bitPattern; negative = bits >> 31 != 0
            if f.isNaN { special = "NaN"; negative = false; decimal = SQLDecimal(0) }
            else if f.isInfinite { special = "Infinity"; decimal = SQLDecimal(0) }
            else { decimal = try BinaryFloatDecimal.exact(bits32: bits) }
        case .f64(let bits):
            floatBits = bits; let f = Double(bitPattern: bits); negative = bits >> 63 != 0
            if f.isNaN { special = "NaN"; negative = false; decimal = SQLDecimal(0) }
            else if f.isInfinite { special = "Infinity"; decimal = SQLDecimal(0) }
            else { decimal = try BinaryFloatDecimal.exact(bits64: bits) }
        default: throw JedError("42883", "function does not exist")
        }
        if descriptor.multiplier > 0 {
            if let bits = floatBits {
                var factor = Double("1e\(descriptor.multiplier)") ?? .infinity
                if descriptor.multiplier == 23 || descriptor.multiplier == 210 { factor = factor.nextUp }
                let product = float32 ? Double(Float(Double(bitPattern: bits)) * Float(factor)) : Double(bitPattern: bits) * factor
                floatBits = product.bitPattern; negative = product.sign == .minus
                if product.isNaN { special = "NaN"; negative = false }
                else if product.isInfinite { special = "Infinity" }
                else { decimal = try BinaryFloatDecimal.exact(bits64: product.bitPattern) }
            } else {
                if case .integer = value {
                    let maxDigits = type == .i64 ? 18 : 9
                    guard descriptor.multiplier <= maxDigits else { throw JedError("22003", "integer out of range") }
                }
                decimal = try shifted(decimal, by: descriptor.multiplier).checked()
                if case .integer = value {
                    let n = try decimal.integerValue()
                    if type != .i64 && (n < Int32.min || n > Int32.max) { throw JedError("22003", "integer out of range") }
                }
            }
        }
        if descriptor.roman {
            let rounded = floatBits == nil ? try decimal.rounded(to: 0) : BinaryFloatDecimal.roundedEven(decimal, scale: 0)
            let n = special == nil ? (try? rounded.integerValue()) : nil
            return Prepared(digits: [], negative: negative, integerDigits: 0, overflow: false, roman: roman(n ?? 0, lower: descriptor.lowerRoman))
        }
        var after = descriptor.after
        if floatBits != nil {
            let integral = BinaryFloatDecimal.roundedEven(absolute(decimal), scale: 0)
            let integerDigits = special?.utf8.count ?? max(1, integral.precision)
            after = min(after, max(0, (float32 ? 6 : 15) - integerDigits))
        }
        var raw: String
        if let special { raw = special }
        else {
            let rounded = floatBits == nil ? try decimal.rounded(to: after) : BinaryFloatDecimal.roundedEven(decimal, scale: min(350, after))
            raw = absolute(rounded).description
            if floatBits == nil { negative = rounded.negative }
            if after > 350 && floatBits != nil { raw += String(repeating: "0", count: after - 350) }
        }
        let integerDigits = raw.utf8.firstIndex(of: 46).map { raw.utf8.distance(from: raw.utf8.startIndex, to: $0) } ?? raw.utf8.count
        let before = descriptor.before + descriptor.multiplier
        let overflow = integerDigits > before
        if overflow { raw = String(repeating: "#", count: before) + (descriptor.after > 0 ? "." + String(repeating: "#", count: descriptor.after) : "") }
        return Prepared(digits: Array(raw.utf8), negative: negative, integerDigits: overflow ? before : integerDigits, overflow: overflow, roman: nil)
    }
    static func toChar(_ value: SQLValue, type: ScalarType, template: SQLText, meter: CostMeter? = nil) throws -> SQLValue {
        if value.isNull { return .null }
        let descriptor = try Descriptor(template)
        let result: String
        if descriptor.scientific {
            let decimal: SQLDecimal, negative: Bool, even: Bool
            switch value {
            case .integer(let n): decimal = SQLDecimal(n); negative = n < 0; even = type != .i64
            case .decimal(let n): decimal = n; negative = n.negative; even = false
            case .f32(let bits):
                let f = Float(bitPattern: bits)
                if !f.isFinite { return try output(" " + String(repeating: "#", count: descriptor.before) + (descriptor.decimal ? "." : "") + String(repeating: "#", count: descriptor.after + 4), meter: meter) }
                decimal = try BinaryFloatDecimal.exact(bits32: bits); negative = bits >> 31 != 0; even = true
            case .f64(let bits):
                let f = Double(bitPattern: bits)
                if !f.isFinite { return try output(" " + String(repeating: "#", count: descriptor.before) + (descriptor.decimal ? "." : "") + String(repeating: "#", count: descriptor.after + 4), meter: meter) }
                decimal = try BinaryFloatDecimal.exact(bits64: bits); negative = bits >> 63 != 0; even = true
            default: throw JedError("42883", "function does not exist")
            }
            result = try scientific(decimal, precision: descriptor.after, negative: negative, even: even)
        } else { result = render(try prepare(value, type: type, descriptor: descriptor), descriptor: descriptor) }
        return try output(result, meter: meter)
    }
    private static func output(_ text: String, meter: CostMeter?) throws -> SQLValue {
        meter?.charge(Int64(text.utf8.count)); try meter?.guardCost()
        return .text(SQLText(validatedUTF8: Array(text.utf8)))
    }
    private static func render(_ number: Prepared, descriptor d: Descriptor) -> String {
        var output = "", digitIndex = 0, position = 0, significant = false, signWritten = false, inFraction = false, tailWritten = false
        let before = d.before + d.multiplier
        let padding = max(0, before - number.integerDigits)
        var lastFraction = d.after
        if d.fill && !number.overflow {
            let fraction = number.digits.dropFirst(number.integerDigits + 1)
            lastFraction = fraction.count
            while lastFraction > max(0, d.lastZero - before) && fraction[fraction.index(fraction.startIndex, offsetBy: lastFraction - 1)] == 48 { lastFraction -= 1 }
        }
        let automaticSign = !d.minus && !d.signed && !d.sign
        func leadingSign() -> String {
            if d.sign && !d.signAfter { return number.negative ? "-" : "+" }
            if d.brackets { return number.negative ? "<" : d.fill ? "" : " " }
            if automaticSign { return number.negative ? "-" : d.fill ? "" : " " }
            return ""
        }
        for node in d.nodes {
            guard let id = node.keyword?.id else { output += String(decoding: node.bytes, as: UTF8.self); continue }
            if let numeral = number.roman {
                if id == "RN" || id == "rn" { output += (d.fill ? "" : String(repeating: " ", count: max(0, 15 - numeral.utf8.count))) + numeral }
                continue
            }
            switch id {
            case "9", "0", "DEC", "D":
                let decimalNode = id == "DEC" || id == "D"
                if !decimalNode { position += 1 }
                if !inFraction && !decimalNode && position <= padding {
                    if let start = d.firstZero, position >= start {
                        if !signWritten { output += leadingSign(); signWritten = true }
                        output += "0"; significant = true
                    } else if !d.fill { output += " " }
                    continue
                }
                if !signWritten { output += leadingSign(); signWritten = true }
                if decimalNode {
                    if digitIndex >= number.digits.count { return output }
                    if number.digits[digitIndex] == 46 { output += "."; digitIndex += 1 }
                    else { output += "." }
                    inFraction = true; significant = true
                } else {
                    if inFraction && d.fill && position - before > lastFraction { continue }
                    if digitIndex >= number.digits.count { return output }
                    let byte = number.digits[digitIndex]
                    if byte == 48 && !inFraction && number.integerDigits == 1 && d.after > 0 && d.firstZero == nil && !number.overflow {
                        if !d.fill { output += " " } else if lastFraction == 0 { output += "0"; significant = true }
                    } else { output.append(Character(UnicodeScalar(byte))); significant = true }
                    digitIndex += 1
                }
                if !tailWritten && position == before + (d.fill ? lastFraction : d.after) && (!d.decimal || inFraction) {
                    tailWritten = true
                    if d.sign && d.signAfter { output += number.negative ? "-" : "+" }
                    if d.brackets { output += number.negative ? ">" : d.fill ? "" : " " }
                }
            case "COMMA", "G": output += significant ? "," : d.fill ? "" : " "
            case "L": output += " "
            case "MI": output += number.negative ? "-" : d.fill ? "" : " "
            case "PL": output += number.negative ? d.fill ? "" : " " : "+"
            case "SG": output += number.negative ? "-" : "+"
            case "TH", "th":
                if !number.negative && !d.decimal && !number.overflow {
                    let end = number.digits.suffix(2).reduce(Int64(0)) { $0 * 10 + Int64($1 - 48) }
                    output += FormatTemplate.ordinal(end, upper: id == "TH")
                }
            default: break
            }
        }
        return output
    }
    static func toNumber(_ input: SQLText, template: SQLText, meter: CostMeter) throws -> SQLValue {
        if template.utf8.isEmpty { return .null }
        let d = try Descriptor(template)
        if d.scientific { throw JedError("0A000", "EEEE not supported for input") }
        let bytes = input.utf8
        var position = 0, digits: [UInt8] = [], fractional = false, negative = false, signSeen = false, scale = 0
        func data(_ byte: UInt8) -> Bool { FormatTemplate.digit(byte) || byte == 46 || byte == 43 || byte == 45 || byte == 60 || byte == 62 }
        for node in d.nodes {
            guard position < bytes.count else { break }
            guard let id = node.keyword?.id else { position += FormatTemplate.length(bytes, position); continue }
            switch id {
            case "9", "0", "DEC", "D":
                if bytes[position] == 32 { position += 1 }
                if position < bytes.count && !signSeen {
                    if bytes[position] == 45 || bytes[position] == 43 || d.brackets && bytes[position] == 60 {
                        negative = bytes[position] != 43; signSeen = true; position += 1
                    }
                }
                if position < bytes.count {
                    if FormatTemplate.digit(bytes[position]) {
                        if !fractional || scale < d.after {
                            digits.append(bytes[position]); if fractional { scale += 1 }
                        }
                        position += 1
                    } else if bytes[position] == 46 && d.decimal && !fractional {
                        digits.append(46); fractional = true; position += 1
                    }
                }
                if d.sign && d.signAfter && position < bytes.count && (bytes[position] == 43 || bytes[position] == 45) {
                    negative = bytes[position] == 45; signSeen = true; position += 1
                }
            case "S": break
            case "MI", "PL", "SG":
                if bytes[position] == 45 || bytes[position] == 43 { negative = bytes[position] == 45; signSeen = true; position += 1 }
            case "COMMA", "G": if bytes[position] == 44 { position += 1 }
            case "L": if !data(bytes[position]) { position += FormatTemplate.length(bytes, position) }
            case "TH", "th":
                for _ in 0..<2 { if position < bytes.count && !data(bytes[position]) { position += FormatTemplate.length(bytes, position) } }
            case "RN", "rn":
                while position < bytes.count && FormatTemplate.space(bytes[position]) { position += 1 }
                let start = position
                while position < bytes.count && [73, 86, 88, 76, 67, 68, 77].contains(bytes[position] & ~UInt8(32)) { position += 1 }
                let numeral = String(decoding: bytes[start..<position], as: UTF8.self).uppercased()
                guard !numeral.isEmpty else { throw JedError("22P02", "invalid Roman numeral") }
                let values: [UInt8: Int64] = [73: 1, 86: 5, 88: 10, 76: 50, 67: 100, 68: 500, 77: 1000]
                var total: Int64 = 0, previous: Int64 = 0
                for byte in numeral.utf8.reversed() {
                    let n = values[byte]!
                    total += n < previous ? -n : n; previous = max(previous, n)
                    if total > 4000 { break }
                }
                guard roman(total) == numeral else { throw JedError("22P02", "invalid Roman numeral") }
                return .decimal(SQLDecimal(total))
            default: break
            }
        }
        if d.roman { throw JedError("22P02", "invalid Roman numeral") }
        guard digits.contains(where: FormatTemplate.digit) else { throw JedError("22P02", "invalid input syntax for type numeric") }
        var value = try SQLDecimal.parse((negative ? "-" : "") + String(decoding: digits, as: UTF8.self))
        if !value.isZero && value.precision - value.scale > d.before + d.multiplier { throw JedError("22003", "numeric field overflow") }
        if d.multiplier > 0 {
            let factor = try DecimalFunctions.call(name: "power", args: [SQLDecimal(10), SQLDecimal(-Int64(d.multiplier))], meter: meter)
            value = try value.multiplying(factor)
        }
        return .decimal(value)
    }
}
