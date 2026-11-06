/// Recursive PostgreSQL container text I/O. Tokenizers remove exactly one
/// quoting layer; scalar diagnostics therefore name the decoded field text.
internal enum ContainerText {
    static func input(_ text: SQLText, to descriptor: TypeDescriptor, explicit: Bool = false,
                      context: ScalarCasts.Context = .init()) throws -> SQLValue {
        try input(text, to: descriptor, explicit: explicit, context: context, depth: 0)
    }

    private static func input(_ text: SQLText, to descriptor: TypeDescriptor, explicit: Bool,
                              context: ScalarCasts.Context, depth: Int) throws -> SQLValue {
        guard depth <= 64 else { throw JedError("54001", "container type nesting limit exceeded") }
        var fieldContext = context
        // Clock-relative date words are explicit SQL syntax, never row data.
        fieldContext.statementMicros = nil
        switch descriptor.type {
        case .scalar:
            return try ScalarCasts.input(text, to: descriptor, explicit: explicit, context: context)
        case .array(let element):
            _ = try ValueCodec.arrayElement(element)
            var parser = ArrayInput(bytes: text.utf8)
            let (dimensions, lowerBounds, tokens) = try parser.parse()
            var values: [SQLValue] = []
            let field = elementDescriptor(descriptor, element: element)
            for token in tokens {
                values.append(try token.map { try input($0, to: field, explicit: false, context: fieldContext, depth: depth + 1) } ?? .null)
            }
            return .array(try SQLArray(dimensions: dimensions, lowerBounds: lowerBounds, elements: values))
        case .composite, .record:
            guard let fields = TypeRules.fields(descriptor.type) else { throw JedError("42804", "expected composite type") }
            let tokens = try recordTokens(text, emptyRecord: fields.isEmpty)
            guard tokens.count == fields.count else { throw JedError("22P02", "malformed record literal: \"\(text)\"") }
            var values: [SQLValue] = []
            for (token, field) in zip(tokens, fields) {
                values.append(try token.map { try input($0, to: field.descriptor, explicit: false, context: fieldContext, depth: depth + 1) } ?? .null)
            }
            return .composite(values)
        case .range(let element):
            let bytes = Temporal.trimmed(text.utf8)
            if bytes.map(Temporal.asciiLower) == Array("empty".utf8) { return .range(SQLRange(empty: true)) }
            guard bytes.count >= 3, bytes[0] == 91 || bytes[0] == 40,
                  bytes.last == 93 || bytes.last == 41 else { throw malformedRange(text) }
            let tokens = try rangeTokens(Array(bytes[1..<bytes.count - 1]), source: text)
            let bounds = try tokens.map { token in
                try token.map { try input($0, to: TypeDescriptor(element), explicit: false, context: fieldContext, depth: depth + 1) }
            }
            return .range(try RangeFunctions.canonicalize(SQLRange(lower: bounds[0], upper: bounds[1],
                                                                   lowerInclusive: bytes[0] == 91, upperInclusive: bytes.last == 93), element: element))
        }
    }

    static func elementDescriptor(_ array: TypeDescriptor, element: SQLType) -> TypeDescriptor {
        var result = TypeDescriptor(element)
        result.decimal = array.decimal; result.textLimit = array.textLimit
        result.temporal = array.temporal; result.collation = array.collation
        return result
    }

    static func render(_ value: SQLValue, type: TypeDescriptor,
                       context: ScalarCasts.Context = .init(), growth: Bool = false) throws -> SQLText {
        if case .scalar = type.type { return try ScalarCasts.render(value, context: context, growth: growth) }
        let result = try renderContainer(value, type: type, context: context, depth: 0)
        if growth && !value.isNull { context.meter?.charge(Int64(result.utf8.count)); try context.meter?.guardCost() }
        return result
    }

    private enum QuoteStyle { case array, record, range }
    private static func quoted(_ text: SQLText, style: QuoteStyle) -> [UInt8] {
        let bytes = text.utf8
        let special: [UInt8] = style == .array ? [44, 123, 125, 34, 92]
            : style == .record ? [44, 40, 41, 34, 92] : [44, 91, 93, 40, 41, 34, 92]
        let needsQuotes = bytes.isEmpty || bytes.contains { special.contains($0) || Temporal.space($0) }
            || style == .array && bytes.map(Temporal.asciiLower) == [110, 117, 108, 108]
        if !needsQuotes { return bytes }
        var output: [UInt8] = [34]
        for byte in bytes {
            if byte == 34 || byte == 92 { output.append(style == .array ? 92 : byte) }
            output.append(byte)
        }
        output.append(34); return output
    }

    private static func renderContainer(_ value: SQLValue, type: TypeDescriptor,
                                        context: ScalarCasts.Context, depth: Int) throws -> SQLText {
        guard depth <= 64 else { throw JedError("54001", "container type nesting limit exceeded") }
        if value.isNull { return SQLText(validatedUTF8: Array("NULL".utf8)) }
        func child(_ value: SQLValue, _ type: TypeDescriptor) throws -> SQLText {
            if case .scalar = type.type { return try ScalarCasts.render(value, context: context, containerBoolean: true) }
            return try renderContainer(value, type: type, context: context, depth: depth + 1)
        }
        switch (value, type.type) {
        case (.array(let array), .array(let element)):
            if array.elements.isEmpty { return SQLText(validatedUTF8: [123, 125]) }
            let elementType = elementDescriptor(type, element: element)
            var output: [UInt8] = []
            if array.lowerBounds.contains(where: { $0 != 1 }) {
                for (length, lower) in zip(array.dimensions, array.lowerBounds) {
                    output += Array("[\(lower):\(Int64(lower) + Int64(length) - 1)]".utf8)
                }
                output.append(61)
            }
            var position = 0
            func dimension(_ level: Int) throws {
                output.append(123)
                for i in 0..<array.dimensions[level] {
                    if i != 0 { output.append(44) }
                    if level + 1 < array.dimensions.count { try dimension(level + 1) }
                    else {
                        let value = array.elements[position]; position += 1
                        if value.isNull { output += [78, 85, 76, 76] }
                        else { output += quoted(try child(value, elementType), style: .array) }
                    }
                }
                output.append(125)
            }
            try dimension(0)
            return SQLText(validatedUTF8: output)
        case (.composite(let values), .composite), (.composite(let values), .record):
            guard let fields = TypeRules.fields(type.type), fields.count == values.count else {
                throw JedError("42804", "composite value does not match its field types")
            }
            var output: [UInt8] = [40]
            for index in values.indices {
                if index != 0 { output.append(44) }
                if !values[index].isNull { output += quoted(try child(values[index], fields[index].descriptor), style: .record) }
            }
            output.append(41); return SQLText(validatedUTF8: output)
        case (.range(let range), .range(let element)):
            if range.isEmpty { return SQLText(validatedUTF8: Array("empty".utf8)) }
            let elementType = TypeDescriptor(element)
            var output: [UInt8] = [range.lowerInclusive ? 91 : 40]
            if let lower = range.lower { output += quoted(try child(lower, elementType), style: .range) }
            output.append(44)
            if let upper = range.upper { output += quoted(try child(upper, elementType), style: .range) }
            output.append(range.upperInclusive ? 93 : 41)
            return SQLText(validatedUTF8: output)
        default: throw JedError("42804", "value does not match container type \(type.type.name)")
        }
    }

    private static func recordTokens(_ source: SQLText, emptyRecord: Bool) throws -> [SQLText?] {
        let bytes = Temporal.trimmed(source.utf8)
        func invalid() -> JedError { JedError("22P02", "malformed record literal: \"\(source)\"") }
        guard bytes.first == 40, bytes.last == 41 else { throw invalid() }
        if bytes.count == 2 && emptyRecord { return [] }
        var tokens: [SQLText?] = [], token: [UInt8] = [], position = 1, quote = false, touched = false, nesting = 0
        while position < bytes.count - 1 {
            let byte = bytes[position]; position += 1
            if byte == 92 {
                guard position < bytes.count - 1 else { throw invalid() }
                token.append(bytes[position]); position += 1; touched = true
            } else if byte == 34 {
                if quote && position < bytes.count - 1 && bytes[position] == 34 { token.append(34); position += 1 }
                else { quote.toggle() }
                touched = true
            } else if !quote && byte == 44 && nesting == 0 {
                tokens.append(touched ? SQLText(validatedUTF8: token) : nil); token = []; touched = false
            } else {
                if !quote && byte == 40 { nesting += 1; guard nesting <= 32 else { throw invalid() } }
                if !quote && byte == 41 { guard nesting > 0 else { throw invalid() }; nesting -= 1 }
                token.append(byte); touched = true
            }
        }
        guard !quote && nesting == 0 else { throw invalid() }
        tokens.append(touched ? SQLText(validatedUTF8: token) : nil)
        return tokens
    }

    private static func malformedRange(_ source: SQLText) -> JedError { JedError("22P02", "malformed range literal: \"\(source)\"") }
    private static func rangeTokens(_ bytes: [UInt8], source: SQLText) throws -> [SQLText?] {
        var tokens: [SQLText?] = [], token: [UInt8] = [], position = 0, quote = false, touched = false
        while position < bytes.count {
            let byte = bytes[position]; position += 1
            if byte == 92 {
                guard position < bytes.count else { throw malformedRange(source) }
                token.append(bytes[position]); position += 1; touched = true
            } else if byte == 34 {
                if quote && position < bytes.count && bytes[position] == 34 { token.append(34); position += 1 }
                else { quote.toggle() }
                touched = true
            } else if !quote && byte == 44 {
                guard tokens.isEmpty else { throw malformedRange(source) }
                tokens.append(touched ? SQLText(validatedUTF8: token) : nil); token = []; touched = false
            } else {
                guard quote || ![40, 41, 91, 93].contains(byte) else { throw malformedRange(source) }
                token.append(byte); touched = true
            }
        }
        guard !quote && tokens.count == 1 else { throw malformedRange(source) }
        tokens.append(touched ? SQLText(validatedUTF8: token) : nil)
        return tokens
    }

    private struct ArrayInput {
        let bytes: [UInt8]
        var position = 0
        var tokens: [SQLText?] = []
        var peek: UInt8? { position < bytes.count ? bytes[position] : nil }
        func invalid() -> JedError { JedError("22P02", "malformed array literal: \"\(String(decoding: bytes, as: UTF8.self))\"") }
        mutating func spaces() { while let byte = peek, Temporal.space(byte) { position += 1 } }
        mutating func parse() throws -> ([Int], [Int32], [SQLText?]) {
            spaces()
            var lengths: [Int] = [], lowers: [Int32] = []
            while peek == 91 {
                guard lengths.count < 6 else { throw invalid() }; position += 1; spaces()
                let first = try bound(); spaces()
                let lower: Int32, upper: Int32
                if peek == 58 { position += 1; spaces(); lower = first; upper = try bound(); spaces() }
                else { lower = 1; upper = first }
                guard peek == 93 else { throw invalid() }; position += 1; spaces()
                guard upper >= lower else { throw JedError("2202E", "upper bound cannot be less than lower bound") }
                guard upper < Int32.max else { throw JedError("54000", "array upper bound is too large: \(upper)") }
                let length = Int64(upper) - Int64(lower) + 1
                guard length <= 134_217_727 else { throw JedError("54000", "array size exceeds the maximum allowed (134217727)") }
                lengths.append(Int(length)); lowers.append(lower)
            }
            if !lengths.isEmpty { guard peek == 61 else { throw invalid() }; position += 1; spaces() }
            let dimensions = try dimension(depth: 0); spaces()
            guard peek == nil else { throw invalid() }
            if !lengths.isEmpty { guard lengths == dimensions else { throw invalid() } }
            else { lowers = Array(repeating: 1, count: dimensions.count) }
            return (dimensions, lowers, tokens)
        }
        mutating func bound() throws -> Int32 {
            var negative = false
            if peek == 43 || peek == 45 { negative = peek == 45; position += 1 }
            var value: Int64 = 0, count = 0
            while let byte = peek, (48...57).contains(byte) {
                value = value * 10 + Int64(byte - 48); position += 1; count += 1
                guard value <= 2_147_483_648 else { throw JedError("54000", "array bound is out of integer range") }
            }
            guard count > 0 else { throw invalid() }
            guard negative || value <= Int32.max else { throw JedError("54000", "array bound is out of integer range") }
            return Int32(negative ? -value : value)
        }
        mutating func dimension(depth: Int) throws -> [Int] {
            guard depth < 6, peek == 123 else { throw invalid() }; position += 1; spaces()
            if peek == 125 { position += 1; return [] }
            let nested = peek == 123
            var inner: [Int]?, count = 0
            while true {
                if nested {
                    let shape = try dimension(depth: depth + 1)
                    if let inner { guard shape == inner else { throw invalid() } }
                    else { inner = shape }
                } else {
                    guard peek != 123 else { throw invalid() }
                    tokens.append(try element())
                }
                count += 1; spaces()
                if peek == 125 { position += 1; break }
                guard peek == 44 else { throw invalid() }; position += 1; spaces()
            }
            if nested && inner!.isEmpty { return [] }
            return [count] + (inner ?? [])
        }
        mutating func element() throws -> SQLText? {
            spaces()
            if peek == 34 {
                position += 1
                var output: [UInt8] = [], closed = false
                while let byte = peek {
                    position += 1
                    if byte == 34 { closed = true; break }
                    if byte == 92 { guard let escaped = peek else { throw invalid() }; output.append(escaped); position += 1 }
                    else { output.append(byte) }
                }
                guard closed else { throw invalid() }; spaces()
                guard peek == 44 || peek == 125 else { throw invalid() }
                return SQLText(validatedUTF8: output)
            }
            var output: [UInt8] = [], significant = 0, escaped = false
            while let byte = peek, byte != 44 && byte != 125 {
                position += 1
                if byte == 92 {
                    guard let next = peek else { throw invalid() }
                    output.append(next); position += 1; significant = output.count; escaped = true
                } else {
                    guard byte != 34 && byte != 123 else { throw invalid() }
                    output.append(byte)
                    if !Temporal.space(byte) { significant = output.count }
                }
            }
            guard significant > 0 else { throw invalid() }
            output.removeSubrange(significant..<output.count)
            if !escaped && output.map(Temporal.asciiLower) == [110, 117, 108, 108] { return nil }
            return SQLText(validatedUTF8: output)
        }
    }
}
