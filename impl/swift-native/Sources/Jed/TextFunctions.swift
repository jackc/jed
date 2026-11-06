/// SQL text operations count Unicode scalars and compare UTF-8 bytes. Swift's
/// Character/String equality and host Unicode case tables are not SQL semantics.
internal enum TextFunctions {
    static let maximumResultBytes: Int64 = 0x3fff_ffff
    static let names: Set<String> = ["substr", "substring", "overlay", "left", "right", "lpad", "rpad", "btrim", "ltrim", "rtrim", "replace", "translate", "repeat", "reverse", "strpos", "position", "split_part", "starts_with", "ascii", "chr", "initcap", "to_hex", "quote_literal", "quote_ident", "quote_nullable", "like_escape", "concat", "concat_ws", "format"]
    static func call(name: String, args: [SQLValue], types: [TypeDescriptor], constant: Bool,
                     variadicArray: Bool = false, meter: CostMeter,
                     render: JSONFunctions.ScalarRenderer) throws -> SQLValue {
        if ["concat", "concat_ws", "format"].contains(name) {
            return try build(name: name, args: args, types: types, variadicArray: variadicArray, constant: constant, meter: meter, render: render)
        }
        if name == "quote_nullable", args[0].isNull { return try growth(SQLText(validatedUTF8: Array("NULL".utf8)), constant: constant, meter: meter) }
        if args.contains(where: \.isNull) { return .null }
        func text(_ i: Int) throws -> SQLText { guard case .text(let s) = args[i] else { throw JedError("42804", "text argument required") }; return s }
        func integer(_ i: Int) throws -> Int64 { guard case .integer(let n) = args[i] else { throw JedError("42804", "integer argument required") }; return n }
        if name == "chr" {
            let n = try integer(0)
            guard n >= 0 else { throw JedError("22023", "character number must be positive") }
            guard n != 0 else { throw JedError("54000", "null character not permitted") }
            guard n <= 0x10ffff else { throw JedError("54000", "requested character too large for encoding") }
            guard let scalar = Unicode.Scalar(UInt32(n)) else { throw JedError("54000", "requested character not valid for encoding") }
            return .text(SQLText(validatedUTF8: Array(String(scalar).utf8)))
        }
        if name == "to_hex" { return .text(SQLText(validatedUTF8: Array(String(UInt64(bitPattern: try integer(0)), radix: 16).utf8))) }
        let input = try text(0)
        switch name {
        case "repeat":
            let count = try integer(1)
            if count <= 0 || input.utf8.isEmpty { return .text(empty) }
            guard count <= maximumResultBytes / Int64(input.utf8.count) else { throw tooLarge() }
            meter.charge(Int64(input.utf8.count)); try meter.guardCost()
            let size = count * Int64(input.utf8.count)
            try admit(size, meter: meter)
            var output: [UInt8] = []; output.reserveCapacity(Int(size))
            for _ in 0..<count { output.append(contentsOf: input.utf8) }
            return .text(SQLText(validatedUTF8: output))
        case "lpad", "rpad":
            let requested = try integer(1), fill = try args.count > 2 ? text(2) : space
            if requested <= 0 { return .text(empty) }
            guard requested <= maximumResultBytes else { throw tooLarge() }
            meter.charge(Int64(input.utf8.count) + Int64(fill.utf8.count)); try meter.guardCost()
            let inputLength = input.scalarCount, fillLength = fill.scalarCount
            let kept = min(Int(requested), inputLength), headBytes = byteOffset(input.utf8, scalars: kept)
            let padding = fillLength == 0 ? 0 : max(0, Int(requested) - inputLength)
            let full = fillLength == 0 ? 0 : padding / fillLength, remainder = fillLength == 0 ? 0 : padding % fillLength
            let tailBytes = byteOffset(fill.utf8, scalars: remainder)
            let size = Int128(headBytes) + Int128(full) * Int128(fill.utf8.count) + Int128(tailBytes)
            guard size <= maximumResultBytes else { throw tooLarge() }; try admit(Int64(size), meter: meter)
            var output: [UInt8] = []; output.reserveCapacity(Int(size))
            if name == "rpad" { output.append(contentsOf: input.utf8.prefix(headBytes)) }
            for _ in 0..<full { output.append(contentsOf: fill.utf8) }
            output.append(contentsOf: fill.utf8.prefix(tailBytes))
            if name == "lpad" { output.append(contentsOf: input.utf8.prefix(headBytes)) }
            return .text(SQLText(validatedUTF8: output))
        case "substr", "substring":
            let subject = RegexSubject(input)
            let window = try ByteaFunctions.window(length: subject.scalars.count, start: integer(1), count: args.count > 2 ? integer(2) : nil)
            return .text(subject.slice(window))
        case "overlay":
            let replacement = try text(1), start = try integer(2), count = try args.count > 3 ? integer(3) : Int64(replacement.scalarCount)
            guard start > 0 else { throw JedError("22011", "negative substring length not allowed") }
            let end = Int128(start) + Int128(count)
            guard end >= Int32.min, end <= Int32.max else { throw JedError("22003", "integer out of range") }
            let subject = RegexSubject(input)
            let head = try ByteaFunctions.window(length: subject.scalars.count, start: 1, count: start - 1)
            let tail = try ByteaFunctions.window(length: subject.scalars.count, start: Int64(end), count: nil)
            return try growth(SQLText(validatedUTF8: subject.slice(head).utf8 + replacement.utf8 + subject.slice(tail).utf8), constant: constant, meter: meter)
        case "left", "right":
            let subject = RegexSubject(input), count = Int128(try integer(1)), length = Int128(subject.scalars.count)
            if name == "left" {
                let end = Int(max(0, min(length, count < 0 ? length + count : count)))
                return .text(subject.slice(0..<end))
            }
            let start = Int(max(0, min(length, count < 0 ? -count : length - count)))
            return .text(subject.slice(start..<Int(length)))
        case "btrim", "ltrim", "rtrim":
            let subject = RegexSubject(input), set = Set(try (args.count > 1 ? text(1) : space).description.unicodeScalars.map(\.value))
            var lo = 0, hi = subject.scalars.count
            if name != "rtrim" { while lo < hi && set.contains(subject.scalars[lo]) { lo += 1 } }
            if name != "ltrim" { while hi > lo && set.contains(subject.scalars[hi - 1]) { hi -= 1 } }
            return .text(subject.slice(lo..<hi))
        case "replace":
            let source = try text(1), replacement = try text(2)
            meter.charge(Int64(input.utf8.count)); try meter.guardCost()
            let pattern = BytePattern(source.utf8)
            let size = Int128(input.utf8.count) + Int128(pattern.count(in: input.utf8)) * (Int128(replacement.utf8.count) - Int128(source.utf8.count))
            guard size <= maximumResultBytes else { throw tooLarge() }; try admit(Int64(size), meter: meter)
            var output: [UInt8] = [], position = 0; output.reserveCapacity(Int(size))
            pattern.forEachPosition(in: input.utf8) { match in
                output.append(contentsOf: input.utf8[position..<match]); output.append(contentsOf: replacement.utf8); position = match + source.utf8.count
            }
            output.append(contentsOf: input.utf8[position...]); return .text(SQLText(validatedUTF8: output))
        case "translate":
            let source = try text(1).description.unicodeScalars.map(\.value), replacement = try text(2).description.unicodeScalars.map(\.value)
            var mapping: [UInt32: UInt32] = [:], seen = Set<UInt32>()
            for (index, scalar) in source.enumerated() where seen.insert(scalar).inserted { mapping[scalar] = index < replacement.count ? replacement[index] : 0 }
            var output: [UInt8] = []
            for scalar in input.description.unicodeScalars {
                let mapped = mapping[scalar.value] ?? scalar.value
                if mapped != 0 { output.append(contentsOf: String(Unicode.Scalar(mapped)!).utf8) }
            }
            return .text(SQLText(validatedUTF8: output))
        case "reverse": return .text(SQLText(validatedUTF8: input.description.unicodeScalars.reversed().flatMap { Array(String($0).utf8) }))
        case "strpos", "position":
            guard let position = BytePattern(try text(1).utf8).first(in: input.utf8) else { return .integer(0) }
            return .integer(Int64(input.utf8.prefix(position).reduce(1) { $0 + ($1 & 0xc0 == 0x80 ? 0 : 1) }))
        case "split_part":
            let delimiter = try text(1), n = try integer(2)
            guard n != 0 else { throw JedError("22023", "field position must not be zero") }
            let pattern = BytePattern(delimiter.utf8)
            let index = n > 0 ? Int128(n) - 1 : Int128(pattern.count(in: input.utf8)) + 1 + Int128(n)
            guard index >= 0, index <= Int.max else { return .text(empty) }
            let slot = Int(index)
            var field = 0, start = 0, end = input.utf8.count
            pattern.forEachPosition(in: input.utf8) { boundary in
                if field == slot { end = boundary }
                if field + 1 == slot { start = boundary + delimiter.utf8.count }
                field += 1
            }
            guard slot <= field else { return .text(empty) }
            return .text(SQLText(validatedUTF8: Array(input.utf8[start..<end])))
        case "starts_with": return .boolean(input.utf8.starts(with: try text(1).utf8))
        case "ascii": return .integer(Int64(input.description.unicodeScalars.first?.value ?? 0))
        case "initcap":
            var word = false, output: [UInt8] = []
            for byte in input.utf8 {
                let alpha = (65...90).contains(byte) || (97...122).contains(byte), digit = (48...57).contains(byte)
                if alpha { output.append(word ? ((65...90).contains(byte) ? byte + 32 : byte) : ((97...122).contains(byte) ? byte - 32 : byte)) }
                else { output.append(byte) }
                word = alpha || digit
            }
            return .text(SQLText(validatedUTF8: output))
        case "quote_literal", "quote_nullable": return try growth(quoteLiteral(input), constant: constant, meter: meter)
        case "quote_ident": return try growth(quoteIdentifier(input), constant: constant, meter: meter)
        case "like_escape": return try growth(likeEscape(input, escape: text(1)), constant: constant, meter: meter)
        default: throw JedError("0A000", "text kernel is not connected: \(name)")
        }
    }
    static let empty = SQLText(validatedUTF8: []), space = SQLText(validatedUTF8: [32])
    static func tooLarge() -> JedError { JedError("54000", "requested length too large") }
    static func admit(_ size: Int64, meter: CostMeter) throws {
        guard size <= maximumResultBytes else { throw tooLarge() }
        meter.charge(size); try meter.guardCost(); try meter.reserveScalar(size)
    }
    static func growth(_ result: SQLText, constant: Bool, meter: CostMeter) throws -> SQLValue {
        if !constant { meter.charge(Int64(result.utf8.count)); try meter.guardCost() }
        return .text(result)
    }
    /// Return a byte boundary without allocating a scalar/offset buffer. Padding
    /// uses it during sizing, before admission permits an amplified allocation.
    static func byteOffset(_ bytes: [UInt8], scalars: Int) -> Int {
        if scalars <= 0 { return 0 }
        var count = 0
        for index in bytes.indices where bytes[index] & 0xc0 != 0x80 {
            if count == scalars { return index }; count += 1
        }
        return bytes.count
    }
    static let quotedKeywords: Set<SQLText> = {
        guard case .array(let rows) = GeneratedSpec.keywords["keywords"] else { return [] }
        return Set(rows.compactMap { row in
            guard case .table(let fields) = row, case .text(let word) = fields["word"] else { return nil }
            return SQLText(validatedUTF8: Array(word.utf8))
        })
    }()
    static func quoteIdentifier(_ text: SQLText) -> SQLText {
        let bytes = text.utf8
        let safe = bytes.first.map { (97...122).contains($0) || $0 == 95 } == true && bytes.allSatisfy { (97...122).contains($0) || (48...57).contains($0) || $0 == 95 }
        if safe && !quotedKeywords.contains(text) { return text }
        var output: [UInt8] = [34]
        for byte in bytes { output.append(byte); if byte == 34 { output.append(byte) } }
        output.append(34); return SQLText(validatedUTF8: output)
    }
    static func quoteLiteral(_ text: SQLText) -> SQLText {
        let escaped = text.utf8.contains(92)
        var output: [UInt8] = escaped ? [69, 39] : [39]
        for byte in text.utf8 { output.append(byte); if byte == 39 || byte == 92 { output.append(byte) } }
        output.append(39); return SQLText(validatedUTF8: output)
    }
    static func likeEscape(_ pattern: SQLText, escape: SQLText) throws -> SQLText {
        let markers = escape.description.unicodeScalars.map(\.value)
        guard markers.count <= 1 else { throw JedError("22025", "invalid escape string") }
        if markers.first == 92 { return pattern }
        var output: [UInt8] = [], afterEscape = false
        for scalar in pattern.description.unicodeScalars {
            if !afterEscape && scalar.value == markers.first { output.append(92); afterEscape = true }
            else {
                if scalar.value == 92 && !afterEscape { output.append(92) }
                output.append(contentsOf: String(scalar).utf8); afterEscape = false
            }
        }
        return SQLText(validatedUTF8: output)
    }

    private static func build(name: String, args: [SQLValue], types: [TypeDescriptor], variadicArray: Bool,
                              constant: Bool, meter: CostMeter, render: JSONFunctions.ScalarRenderer) throws -> SQLValue {
        let leading = name == "concat" ? 0 : 1
        if leading == 1 && args[0].isNull { return .null }
        var values = Array(args.dropFirst(leading)), descriptors = Array(types.dropFirst(leading))
        if variadicArray {
            guard values.count == 1 else { throw JedError("42804", "invalid VARIADIC argument shape") }
            if values[0].isNull {
                if name != "format" { return .null }; values = []; descriptors = []
            } else {
                guard case .array(let array) = values[0], case .array(let element) = descriptors[0].type else { throw JedError("42804", "VARIADIC argument must be an array") }
                values = array.elements; descriptors = values.map { _ in TypeDescriptor(element) }
            }
        }
        func output(_ index: Int) throws -> SQLText {
            guard values.indices.contains(index) else { throw JedError("22023", "too few arguments for format()") }
            if values[index].isNull { return empty }
            if case .boolean(let b) = values[index] { return SQLText(validatedUTF8: b ? [116] : [102]) }
            return try render(values[index], descriptors[index], meter)
        }
        if name == "format" {
            guard case .text(let format) = args[0] else { throw JedError("42804", "format string must be text") }
            return .text(try formatText(format, values: values, output: output, meter: meter))
        }
        var rendered: [SQLText] = []
        for index in values.indices where !values[index].isNull { rendered.append(try output(index)) }
        let separator: [UInt8]
        if name == "concat_ws" { guard case .text(let text) = args[0] else { throw JedError("42804", "separator must be text") }; separator = text.utf8 }
        else { separator = [] }
        let size = rendered.reduce(Int128(0)) { $0 + Int128($1.utf8.count) } + Int128(max(0, rendered.count - 1)) * Int128(separator.count)
        if name == "concat_ws" { guard size <= maximumResultBytes else { throw tooLarge() }; try admit(Int64(size), meter: meter) }
        var result: [UInt8] = []
        for (index, text) in rendered.enumerated() { if index > 0 { result.append(contentsOf: separator) }; result.append(contentsOf: text.utf8) }
        if name == "concat" { return try growth(SQLText(validatedUTF8: result), constant: constant, meter: meter) }
        return .text(SQLText(validatedUTF8: result))
    }
    private struct FormatPart { let bytes: [UInt8]; let padding: Int; let left: Bool }
    private static func formatText(_ format: SQLText, values: [SQLValue], output: (Int) throws -> SQLText, meter: CostMeter) throws -> SQLText {
        meter.charge(Int64(format.utf8.count)); try meter.guardCost()
        let bytes = format.utf8
        var cursor = 0, nextArgument = 1, parts: [FormatPart] = [], size: Int128 = 0
        var rendered: [Int: [UInt8]] = [:]
        func invalid(_ message: String, hint: Bool = false) -> JedError {
            var error = JedError("22023", message)
            if hint { error.hint = "For a single \"%\" use \"%%\"." }; return error
        }
        func digits() throws -> Int? {
            var value = 0, found = false
            while cursor < bytes.count && (48...57).contains(bytes[cursor]) {
                found = true
                let digit = Int(bytes[cursor] - 48)
                guard value <= (Int(Int32.max) - digit) / 10 else { throw JedError("22003", "number is out of range") }
                value = value * 10 + digit; cursor += 1
            }
            return found ? value : nil
        }
        func argument(_ position: Int) throws -> Int {
            guard position != 0 else { throw invalid("format specifies argument 0, but arguments are numbered from 1") }
            guard position <= values.count else { throw invalid("too few arguments for format()") }
            return position - 1
        }
        while cursor < bytes.count {
            if bytes[cursor] != 37 {
                let start = cursor
                while cursor < bytes.count && bytes[cursor] != 37 { cursor += 1 }
                let literal = Array(bytes[start..<cursor]); parts.append(FormatPart(bytes: literal, padding: 0, left: false)); size += Int128(literal.count); continue
            }
            cursor += 1
            guard cursor < bytes.count else { throw invalid("unterminated format() type specifier", hint: true) }
            if bytes[cursor] == 37 { parts.append(FormatPart(bytes: [37], padding: 0, left: false)); size += 1; cursor += 1; continue }
            var position: Int?, width = 0, left = false
            let first = try digits()
            if let first, cursor < bytes.count && bytes[cursor] == 36 {
                guard first != 0 else { throw invalid("format specifies argument 0, but arguments are numbered from 1") }
                position = first; cursor += 1
            } else if let first { width = first }
            let widthAlreadyRead = first != nil && position == nil
            if !widthAlreadyRead && cursor < bytes.count && bytes[cursor] == 45 { left = true; cursor += 1 }
            if !widthAlreadyRead && cursor < bytes.count && bytes[cursor] == 42 {
                cursor += 1
                let explicit = try digits()
                if explicit != nil {
                    guard cursor < bytes.count && bytes[cursor] == 36 else { throw invalid("width argument position must be ended by \"$\"") }; cursor += 1
                }
                let widthPosition = explicit ?? nextArgument, index = try argument(widthPosition)
                nextArgument = widthPosition + 1
                let amount: Int64
                if values[index].isNull { amount = 0 }
                else if case .integer(let n) = values[index] { amount = try ScalarCasts.checkedInteger(n, type: .i32) }
                else { amount = try ScalarCasts.integer(output(index), type: .i32) }
                guard amount != Int32.min else { throw JedError("22003", "number is out of range") }
                if amount < 0 { left = true }; width = Int(abs(amount))
            } else if first == nil || position != nil { width = try digits() ?? 0 }
            guard cursor < bytes.count else { throw invalid("unterminated format() type specifier", hint: true) }
            let specifier = bytes[cursor]; cursor += 1
            guard [UInt8(115), 73, 76].contains(specifier) else {
                var end = cursor
                while end < bytes.count && bytes[end] & 0xc0 == 0x80 { end += 1 }
                throw invalid("unrecognized format() type specifier \"\(String(decoding: bytes[(cursor - 1)..<end], as: UTF8.self))\"", hint: true)
            }
            let selected = position ?? nextArgument, index = try argument(selected); nextArgument = selected + 1
            let text: SQLText
            if specifier == 73 {
                guard !values[index].isNull else { throw JedError("22004", "null values cannot be formatted as an SQL identifier") }
                text = quoteIdentifier(try output(index))
            } else if specifier == 76 { text = values[index].isNull ? SQLText(validatedUTF8: Array("NULL".utf8)) : quoteLiteral(try output(index)) }
            else { text = try output(index) }
            let padding = max(0, width - text.scalarCount)
            // Repeated positional conversions share their immutable bytes. We
            // still render each use above for its prescribed timezone cost, but
            // sizing must not retain a copy of an amplified result before admission.
            let cacheKey = index * 256 + Int(specifier)
            if rendered[cacheKey] == nil { rendered[cacheKey] = text.utf8 }
            parts.append(FormatPart(bytes: rendered[cacheKey]!, padding: padding, left: left)); size += Int128(text.utf8.count) + Int128(padding)
        }
        guard size <= maximumResultBytes else { throw tooLarge() }; try admit(Int64(size), meter: meter)
        var result: [UInt8] = []; result.reserveCapacity(Int(size))
        for part in parts {
            if !part.left { result.append(contentsOf: repeatElement(UInt8(32), count: part.padding)) }
            result.append(contentsOf: part.bytes)
            if part.left { result.append(contentsOf: repeatElement(UInt8(32), count: part.padding)) }
        }
        return SQLText(validatedUTF8: result)
    }
}
