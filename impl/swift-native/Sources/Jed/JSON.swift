/// JSON input and canonical values use the engine's exact decimal and bytewise
/// text types. The source parser also retains spans for verbatim json accessors.
public enum SQLJSON {
    public static func parse(text: SQLText, preserveText: Bool = false) throws -> SQLValue {
        let document = try JSONDocument(text, decodeStrings: !preserveText, validateNumbers: !preserveText)
        return preserveText ? .json(text) : .jsonb(try document.materialize())
    }

    internal static func canonicalMembers(_ members: [SQLJSONMember]) -> [SQLJSONMember] {
        let order = members.indices.sorted {
            let a = members[$0].key, b = members[$1].key
            if a.utf8.count != b.utf8.count { return a.utf8.count < b.utf8.count }
            if a != b { return a < b }
            return $0 > $1
        }
        var result: [SQLJSONMember] = []
        for index in order where result.last?.key != members[index].key { result.append(members[index]) }
        return result
    }

    internal static func canonicalize(_ value: SQLJSONValue, depth: Int = 0) throws -> SQLJSONValue {
        switch value {
        case .array(let elements):
            try checkDepth(depth + 1)
            return .array(try elements.map { try canonicalize($0, depth: depth + 1) })
        case .object(let members):
            try checkDepth(depth + 1)
            return .object(canonicalMembers(try members.map {
                SQLJSONMember(key: $0.key, value: try canonicalize($0.value, depth: depth + 1))
            }))
        default: return value
        }
    }

    internal static func checkDepth(_ depth: Int) throws {
        guard depth <= 256 else { throw JedError("54001", "json nesting depth exceeds the maximum of 256") }
    }

    internal static func validateDepth(_ value: SQLJSONValue, depth: Int = 0) throws {
        switch value {
        case .array(let elements):
            try checkDepth(depth + 1)
            for element in elements { try validateDepth(element, depth: depth + 1) }
        case .object(let members):
            try checkDepth(depth + 1)
            for member in members { try validateDepth(member.value, depth: depth + 1) }
        default: break
        }
    }

    internal static func kind(_ value: SQLJSONValue) -> String {
        switch value {
        case .null: "null"
        case .boolean: "boolean"
        case .number: "number"
        case .string: "string"
        case .array: "array"
        case .object: "object"
        }
    }

    internal static func compare(_ left: SQLJSONValue, _ right: SQLJSONValue, root: Bool = true) -> Int {
        func rank(_ value: SQLJSONValue) -> Int {
            switch value { case .null: 0; case .string: 1; case .number: 2; case .boolean: 3; case .array: 4; case .object: 5 }
        }
        if root {
            if case .array(let a) = left, a.isEmpty, rank(right) < 4 { return -1 }
            if case .array(let b) = right, b.isEmpty, rank(left) < 4 { return 1 }
        }
        let a = rank(left), b = rank(right)
        if a != b { return a < b ? -1 : 1 }
        switch (left, right) {
        case (.null, .null): return 0
        case (.boolean(let a), .boolean(let b)): return a == b ? 0 : a ? 1 : -1
        case (.string(let a), .string(let b)): return a == b ? 0 : a < b ? -1 : 1
        case (.number(let a), .number(let b)): return a.compare(to: b)
        case (.array(let a), .array(let b)):
            if a.count != b.count { return a.count < b.count ? -1 : 1 }
            for (a, b) in zip(a, b) { let result = compare(a, b, root: false); if result != 0 { return result } }
            return 0
        case (.object(let a), .object(let b)):
            if a.count != b.count { return a.count < b.count ? -1 : 1 }
            for (a, b) in zip(a, b) {
                if a.key != b.key { return a.key < b.key ? -1 : 1 }
                let result = compare(a.value, b.value, root: false)
                if result != 0 { return result }
            }
            return 0
        default: return 0 // Ranks matched, so this branch is unreachable.
        }
    }

    internal static func escaped(_ text: SQLText) -> [UInt8] {
        var output: [UInt8] = [34]
        let hex = Array("0123456789abcdef".utf8)
        for byte in text.utf8 {
            switch byte {
            case 34, 92: output += [92, byte]
            case 8: output += [92, 98]
            case 12: output += [92, 102]
            case 10: output += [92, 110]
            case 13: output += [92, 114]
            case 9: output += [92, 116]
            case 0..<32: output += [92, 117, 48, 48, hex[Int(byte >> 4)], hex[Int(byte & 15)]]
            default: output.append(byte)
            }
        }
        output.append(34)
        return output
    }

    /// `emit` is also used as a counting pass, so pretty-print admission precedes
    /// allocating its indentation-expanded output.
    internal static func renderWalk(_ node: SQLJSONValue, pretty: Bool, depth: Int = 0,
                                    emit: ([UInt8]) throws -> Void) throws {
        func literal(_ text: String) throws { try emit(Array(text.utf8)) }
        func newline(_ level: Int) throws {
            try emit([10])
            for _ in 0..<(level * 4) { try emit([32]) }
        }
        switch node {
        case .null: try literal("null")
        case .boolean(let value): try literal(value ? "true" : "false")
        case .number(let value): try literal(value.description)
        case .string(let value): try emit(escaped(value))
        case .array(let elements):
            try checkDepth(depth + 1); try emit([91])
            for (index, element) in elements.enumerated() {
                if index > 0 { try emit(pretty ? [44] : [44, 32]) }
                if pretty { try newline(depth + 1) }
                try renderWalk(element, pretty: pretty, depth: depth + 1, emit: emit)
            }
            if pretty { try newline(depth) }
            try emit([93])
        case .object(let members):
            try checkDepth(depth + 1); try emit([123])
            for (index, member) in members.enumerated() {
                if index > 0 { try emit(pretty ? [44] : [44, 32]) }
                if pretty { try newline(depth + 1) }
                try emit(escaped(member.key)); try emit([58, 32])
                try renderWalk(member.value, pretty: pretty, depth: depth + 1, emit: emit)
            }
            if pretty { try newline(depth) }
            try emit([125])
        }
    }

    internal static func render(_ value: SQLJSONValue, pretty: Bool = false,
                                meter: CostMeter? = nil, growth: Bool = false) throws -> SQLText {
        var count: Int64 = 0
        if pretty { meter?.charge(MemorySizes.payload(.jsonb(value))); try meter?.guardCost() }
        try renderWalk(value, pretty: pretty) { count = saturatingAdd(count, Int64($0.count)) }
        if pretty {
            guard count <= 1_073_741_823 else { throw JedError("54000", "requested length too large") }
            meter?.charge(count); try meter?.guardCost(); try meter?.reserveScalar(count)
        } else if growth { meter?.charge(count); try meter?.guardCost() }
        var result: [UInt8] = []
        result.reserveCapacity(Int(count))
        try renderWalk(value, pretty: pretty) { result.append(contentsOf: $0) }
        return SQLText(validatedUTF8: result)
    }
}

internal indirect enum JSONSourceNode {
    case null(Range<Int>)
    case boolean(Bool, Range<Int>)
    case string(SQLText?, Range<Int>)
    case number(Range<Int>)
    case array([JSONSourceNode], Range<Int>)
    case object([JSONSourceMember], Range<Int>)

    var span: Range<Int> {
        switch self {
        case .null(let span), .number(let span): span
        case .boolean(_, let span), .string(_, let span), .array(_, let span), .object(_, let span): span
        }
    }
    var kind: String {
        switch self { case .null: "null"; case .boolean: "boolean"; case .string: "string"; case .number: "number"; case .array: "array"; case .object: "object" }
    }
}

internal struct JSONSourceMember {
    let key: SQLText?
    let value: JSONSourceNode
}

internal struct JSONDocument {
    let text: SQLText
    let root: JSONSourceNode

    init(_ text: SQLText, decodeStrings: Bool = true, uniqueKeys: Bool = false, validateNumbers: Bool = false,
         expectedRoot: String? = nil, rootError: JedError? = nil) throws {
        var parser = JSONTextParser(bytes: text.utf8, decodeStrings: decodeStrings, uniqueKeys: uniqueKeys, validateNumbers: validateNumbers)
        parser.whitespace()
        // PG raises a container-kind error at its opener, but decodes a scalar
        // token first. The whole-document escape check still precedes extraction.
        if let expectedRoot, let rootError, parser.position < text.utf8.count {
            let first = text.utf8[parser.position]
            if (first == 91 && expectedRoot != "array") || (first == 123 && expectedRoot != "object") { throw rootError }
        }
        let root = try parser.value(depth: 0)
        parser.whitespace()
        guard parser.position == text.utf8.count else { throw parser.invalid() }
        if let expectedRoot, let rootError, root.kind != expectedRoot { throw rootError }
        self.text = text; self.root = root
    }

    func materialize(_ node: JSONSourceNode? = nil, canonical: Bool = true) throws -> SQLJSONValue {
        switch node ?? root {
        case .null: return .null
        case .boolean(let value, _): return .boolean(value)
        case .string(let value, _):
            guard let value else { throw JedError("XX000", "JSON string was not decoded") }
            return .string(value)
        case .number(let span): return .number(try SQLDecimal.parse(String(decoding: text.utf8[span], as: UTF8.self)))
        case .array(let elements, _): return .array(try elements.map { try materialize($0, canonical: canonical) })
        case .object(let members, _):
            let output = try members.map { member -> SQLJSONMember in
                guard let key = member.key else { throw JedError("XX000", "JSON key was not decoded") }
                return SQLJSONMember(key: key, value: try materialize(member.value, canonical: canonical))
            }
            return .object(canonical ? SQLJSON.canonicalMembers(output) : output)
        }
    }

    func extract(_ node: JSONSourceNode, asText: Bool, whole: Bool = false) -> SQLValue {
        if asText {
            if case .null = node { return .null }
            if case .string(let value, _) = node, let value { return .text(value) }
        }
        let start = whole && node.kind != "array" && node.kind != "object" ? 0 : node.span.lowerBound
        let result = SQLText(validatedUTF8: Array(text.utf8[start..<node.span.upperBound]))
        return asText ? .text(result) : .json(result)
    }
}

private struct JSONTextParser {
    let bytes: [UInt8]
    let decodeStrings: Bool
    let uniqueKeys: Bool
    let validateNumbers: Bool
    var position = 0

    func invalid(_ detail: String? = nil) -> JedError {
        var error = JedError("22P02", "invalid input syntax for type json")
        error.detail = detail
        return error
    }
    mutating func whitespace() {
        while position < bytes.count && [9, 10, 13, 32].contains(bytes[position]) { position += 1 }
    }
    mutating func take(_ byte: UInt8) -> Bool {
        if position < bytes.count && bytes[position] == byte { position += 1; return true }
        return false
    }
    mutating func value(depth: Int) throws -> JSONSourceNode {
        whitespace()
        let start = position
        guard position < bytes.count else { throw invalid("The input string ended unexpectedly.") }
        let first = bytes[position]
        if first == 91 || first == 123 { try SQLJSON.checkDepth(depth + 1) }
        if take(91) {
            var elements: [JSONSourceNode] = []; whitespace()
            if !take(93) {
                repeat { elements.append(try value(depth: depth + 1)); whitespace() } while take(44)
                guard take(93) else { throw invalid() }
            }
            return .array(elements, start..<position)
        }
        if take(123) {
            var members: [JSONSourceMember] = []; var keys = Set<SQLText>(); whitespace()
            if !take(125) {
                repeat {
                    whitespace(); guard position < bytes.count && bytes[position] == 34 else { throw invalid() }
                    let key = try string(); whitespace(); guard take(58) else { throw invalid() }
                    let member = try value(depth: depth + 1)
                    if uniqueKeys, let key, !keys.insert(key).inserted { throw JedError("22030", "duplicate JSON object key value") }
                    members.append(JSONSourceMember(key: key, value: member)); whitespace()
                } while take(44)
                guard take(125) else { throw invalid() }
            }
            return .object(members, start..<position)
        }
        if first == 34 { let decoded = try string(); return .string(decoded, start..<position) }
        if first == 45 || (48...57).contains(first) {
            _ = take(45)
            if !take(48) {
                guard position < bytes.count && (49...57).contains(bytes[position]) else { throw invalid() }
                while position < bytes.count && (48...57).contains(bytes[position]) { position += 1 }
            }
            if take(46) {
                let fraction = position
                while position < bytes.count && (48...57).contains(bytes[position]) { position += 1 }
                guard position > fraction else { throw invalid() }
            }
            if take(101) || take(69) {
                if !take(43) { _ = take(45) }
                let exponent = position
                while position < bytes.count && (48...57).contains(bytes[position]) { position += 1 }
                guard position > exponent else { throw invalid() }
            }
            if validateNumbers { _ = try SQLDecimal.parse(String(decoding: bytes[start..<position], as: UTF8.self)) }
            return .number(start..<position)
        }
        for word in ["null", "true", "false"] {
            let token = Array(word.utf8)
            if bytes[position...].starts(with: token) {
                position += token.count
                return word == "null" ? .null(start..<position) : .boolean(word == "true", start..<position)
            }
        }
        throw invalid()
    }

    mutating func hex4() throws -> UInt32 {
        guard bytes.count - position >= 4 else { throw invalid("\"\\u\" must be followed by four hexadecimal digits.") }
        var value: UInt32 = 0
        for _ in 0..<4 {
            let byte = bytes[position]; position += 1
            let digit: UInt32
            switch byte {
            case 48...57: digit = UInt32(byte - 48)
            case 65...70: digit = UInt32(byte - 55)
            case 97...102: digit = UInt32(byte - 87)
            default: throw invalid("\"\\u\" must be followed by four hexadecimal digits.")
            }
            value = value * 16 + digit
        }
        return value
    }

    mutating func string() throws -> SQLText? {
        guard take(34) else { throw invalid() }
        var output: [UInt8] = []
        var high: UInt32?
        func surrogate(_ detail: String) -> JedError { invalid(detail) }
        while position < bytes.count {
            let byte = bytes[position]; position += 1
            if byte == 34 {
                if high != nil { throw surrogate("Unicode low surrogate must follow a high surrogate.") }
                return decodeStrings ? SQLText(validatedUTF8: output) : nil
            }
            guard byte >= 32 else { throw invalid("Character with value \(byte) must be escaped.") }
            if byte != 92 {
                if high != nil { throw surrogate("Unicode low surrogate must follow a high surrogate.") }
                if decodeStrings { output.append(byte) }
                continue
            }
            guard position < bytes.count else { throw invalid() }
            let escaped = bytes[position]; position += 1
            if escaped == 117 {
                var scalar = try hex4()
                if !decodeStrings { continue }
                if (0xd800...0xdbff).contains(scalar) {
                    guard high == nil else { throw surrogate("Unicode high surrogate must not follow a high surrogate.") }
                    high = scalar; continue
                }
                if (0xdc00...0xdfff).contains(scalar) {
                    guard let preceding = high else { throw surrogate("Unicode low surrogate must follow a high surrogate.") }
                    scalar = 0x10000 + (preceding - 0xd800) * 1024 + scalar - 0xdc00; high = nil
                } else if high != nil { throw surrogate("Unicode low surrogate must follow a high surrogate.") }
                guard scalar != 0 else {
                    var error = JedError("22P05", "unsupported Unicode escape sequence")
                    error.detail = "\\u0000 cannot be converted to text."
                    throw error
                }
                guard let unicode = Unicode.Scalar(scalar) else { throw invalid() }
                output.append(contentsOf: String(unicode).utf8)
                continue
            }
            if high != nil { throw surrogate("Unicode low surrogate must follow a high surrogate.") }
            let decoded: UInt8
            switch escaped {
            case 34, 92, 47: decoded = escaped
            case 98: decoded = 8
            case 102: decoded = 12
            case 110: decoded = 10
            case 114: decoded = 13
            case 116: decoded = 9
            default: throw invalid("Escape sequence \"\\\(String(Unicode.Scalar(escaped)))\" is invalid.")
            }
            if decodeStrings { output.append(decoded) }
        }
        throw invalid("The input string ended unexpectedly.")
    }
}
