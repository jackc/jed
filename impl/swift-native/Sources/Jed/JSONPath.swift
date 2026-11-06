internal struct JSONPathRegex {
    let matches: (SQLText, CostMeter) throws -> Bool
}
internal typealias JSONPathRegexCompiler = (SQLText, SQLText, CostMeter?) throws -> JSONPathRegex

internal indirect enum JSONPathExpression {
    case root, current, last
    case variable(SQLText)
    case literal(SQLJSONValue)
    case unary(String, JSONPathExpression)
    case binary(String, JSONPathExpression, JSONPathExpression)
    case predicate(JSONPathPredicate)
    case path(JSONPathExpression, [JSONPathStep])
}

internal indirect enum JSONPathPredicate {
    case compare(String, JSONPathExpression, JSONPathExpression)
    case starts(JSONPathExpression, JSONPathExpression)
    case regex(JSONPathExpression, SQLText, SQLText, JSONPathRegex)
    case exists(JSONPathExpression)
    case unknown(JSONPathPredicate)
    case not(JSONPathPredicate)
    case and(JSONPathPredicate, JSONPathPredicate)
    case or(JSONPathPredicate, JSONPathPredicate)
}

internal enum JSONPathStep {
    case member(SQLText), members, elements
    case subscripts([(JSONPathExpression, JSONPathExpression?)])
    case any(Int?, Int?)
    case method(String, [String])
    case filter(JSONPathPredicate)
}

internal struct JSONPathProgram {
    let expression: JSONPathExpression
    let strict: Bool
    let text: SQLText

    static func compile(_ text: SQLText, meter: CostMeter? = nil,
                        regexCompiler: JSONPathRegexCompiler? = nil) throws -> JSONPathProgram {
        meter?.charge(Int64(text.utf8.count)); try meter?.checkpoint()
        var parser = JSONPathParser(text: text, meter: meter, regexCompiler: regexCompiler ?? JSONPathFunctions.regex)
        let (expression, strict) = try parser.parse()
        let rendered = (strict ? "strict " : "") + JSONPathRender.expression(expression, wrap: true)
        let normalized = SQLText(validatedUTF8: Array(rendered.utf8))
        // Canonical parentheses can be deeper than the source. The same path
        // must pass the producer bound when it is compiled again for evaluation.
        var check = JSONPathParser(text: normalized, meter: nil, regexCompiler: { _, _, _ in JSONPathRegex(matches: { _, _ in false }) })
        _ = try check.parse()
        return JSONPathProgram(expression: expression, strict: strict, text: normalized)
    }
}

private enum JSONPathRender {
    static func quote(_ text: SQLText) -> String { String(decoding: SQLJSON.escaped(text), as: UTF8.self) }
    static func priority(_ expression: JSONPathExpression) -> Int {
        switch expression {
        case .binary(let op, _, _): return op == "+" || op == "-" ? 3 : 4
        case .unary: return 5
        case .predicate(let predicate): return priority(predicate)
        default: return 10
        }
    }
    static func priority(_ predicate: JSONPathPredicate) -> Int {
        switch predicate { case .or: 0; case .and: 1; case .compare, .starts, .regex: 2; default: 10 }
    }
    static func operand(_ value: JSONPathExpression, parent: Int) -> String {
        expression(value, wrap: priority(value) <= parent)
    }
    static func expression(_ value: JSONPathExpression, wrap: Bool) -> String {
        switch value {
        case .root: return "$"
        case .current: return "@"
        case .last: return "last"
        case .variable(let key): return "$" + quote(key)
        case .literal(let node): return (try? SQLJSON.render(node).description) ?? "null"
        case .predicate(let value): return predicate(value, wrap: wrap)
        case .unary(let op, let child):
            let body = op + operand(child, parent: 5)
            return wrap ? "(" + body + ")" : body
        case .binary(let op, let left, let right):
            let precedence = op == "+" || op == "-" ? 3 : 4
            let body = operand(left, parent: precedence) + " " + op + " " + operand(right, parent: precedence)
            return wrap ? "(" + body + ")" : body
        case .path(let base, let steps):
            let body: String
            switch base {
            case .literal(.number): body = "(" + expression(base, wrap: false) + ")"
            case .binary, .unary: body = expression(base, wrap: true)
            case .predicate: body = "(" + expression(base, wrap: false) + ")"
            default: body = expression(base, wrap: false)
            }
            return body + steps.map(step).joined()
        }
    }
    static func predicate(_ value: JSONPathPredicate, wrap: Bool) -> String {
        let body: String
        switch value {
        case .compare(let op, let left, let right): body = operand(left, parent: 2) + " " + (op == "<>" ? "!=" : op) + " " + operand(right, parent: 2)
        case .starts(let left, let right): body = operand(left, parent: 2) + " starts with " + operand(right, parent: 2)
        case .regex(let left, let pattern, let flags, _):
            body = expression(left, wrap: priority(left) < 10) + " like_regex " + quote(pattern) + (flags.utf8.isEmpty ? "" : " flag " + quote(flags))
        case .and(let a, let b): body = predicate(a, wrap: priority(a) <= 1) + " && " + predicate(b, wrap: priority(b) <= 1)
        case .or(let a, let b): body = predicate(a, wrap: priority(a) <= 0) + " || " + predicate(b, wrap: priority(b) <= 0)
        case .not(let child): return "!(" + predicate(child, wrap: false) + ")"
        case .exists(let value): return "exists (" + expression(value, wrap: false) + ")"
        case .unknown(let child): return "(" + predicate(child, wrap: false) + ") is unknown"
        }
        return wrap ? "(" + body + ")" : body
    }
    static func step(_ value: JSONPathStep) -> String {
        switch value {
        case .member(let name): return "." + quote(name)
        case .members: return ".*"
        case .elements: return "[*]"
        case .filter(let value): return "?(" + predicate(value, wrap: false) + ")"
        case .method(let name, let args): return "." + name + "(" + args.joined(separator: ",") + ")"
        case .subscripts(let list): return "[" + list.map { expression($0.0, wrap: false) + ($0.1.map { " to " + expression($0, wrap: false) } ?? "") }.joined(separator: ",") + "]"
        case .any(let first, let last):
            if first == 0 && last == nil { return ".**" }
            let a = first.map(String.init) ?? "last", b = last.map(String.init) ?? "last"
            return ".**{" + a + (first == last ? "" : " to " + b) + "}"
        }
    }
}

private struct JSONPathToken {
    enum Kind { case name, string, number, variable, symbol, end }
    let kind: Kind
    let text: SQLText
    var spelling: String { text.description }
    var keyword: String { text.asciiLowercased.description }
}

private struct JSONPathLexer {
    let bytes: [UInt8]
    var position = 0

    func syntax(_ message: String = "syntax error at or near end of jsonpath input") -> JedError { JedError("42601", message) }
    func special(_ byte: UInt8) -> Bool { Array("?%$.[ ]{}()|&!=<>@#,*:-+/\"\t\n\r\u{0c}".utf8).contains(byte) }
    func space(_ byte: UInt8) -> Bool { [9, 10, 12, 13, 32].contains(byte) }
    func starts(_ a: UInt8, _ b: UInt8) -> Bool { position + 1 < bytes.count && bytes[position] == a && bytes[position + 1] == b }

    mutating func comment() throws {
        position += 2
        while position + 1 < bytes.count && !starts(42, 47) { position += 1 }
        guard position + 1 < bytes.count else { throw syntax("unexpected end of comment") }
        position += 2
    }
    /// Lengths compete exactly as scanner rules do: decimal numeric tokens may
    /// consume one trailing `other` byte; radix tokens contribute their full
    /// accepted length, even though this core deliberately rejects those forms
    /// when the winning token is a number. All indices are UTF-8 byte offsets.
    func numericEnd(at start: Int) -> Int {
        func digit(_ byte: UInt8, radix: Int = 10) -> Bool {
            if (48...57).contains(byte) { return Int(byte - 48) < radix }
            if radix == 16 { return (65...70).contains(byte) || (97...102).contains(byte) }
            return false
        }
        func run(_ first: Int, radix: Int = 10, initialUnderscore: Bool = false) -> Int {
            var cursor = first
            if initialUnderscore && cursor < bytes.count && bytes[cursor] == 95 { cursor += 1 }
            guard cursor < bytes.count && digit(bytes[cursor], radix: radix) else { return first }
            cursor += 1
            while cursor < bytes.count {
                if digit(bytes[cursor], radix: radix) { cursor += 1 }
                else if bytes[cursor] == 95 && cursor + 1 < bytes.count && digit(bytes[cursor + 1], radix: radix) { cursor += 2 }
                else { break }
            }
            return cursor
        }
        var cursor = start
        if bytes[cursor] != 46 { cursor = bytes[cursor] == 48 ? cursor + 1 : run(cursor) }
        if cursor < bytes.count && bytes[cursor] == 46 {
            cursor += 1
            cursor = run(cursor)
        }
        var endpoint = cursor
        if cursor < bytes.count && (bytes[cursor] == 101 || bytes[cursor] == 69) {
            cursor += 1
            if cursor < bytes.count && (bytes[cursor] == 43 || bytes[cursor] == 45) { cursor += 1 }
            let exponent = run(cursor)
            if exponent > cursor { endpoint = exponent }
        }
        var longest = endpoint
        if endpoint < bytes.count && !special(bytes[endpoint]) { longest += 1 }
        if bytes[start] == 48 && start + 1 < bytes.count {
            let radix: Int
            switch bytes[start + 1] { case 120, 88: radix = 16; case 111, 79: radix = 8; case 98, 66: radix = 2; default: radix = 0 }
            if radix != 0 {
                let radixEnd = run(start + 2, radix: radix, initialUnderscore: true)
                if radixEnd > start + 2 { longest = max(longest, radixEnd) }
            }
        }
        return longest
    }
    func validNumber(from start: Int, to end: Int) -> Bool {
        var cursor = start, digits = 0
        if bytes[cursor] == 48 { cursor += 1; digits = 1 }
        else {
            while cursor < end && (48...57).contains(bytes[cursor]) { cursor += 1; digits += 1 }
        }
        if cursor < end && bytes[cursor] == 46 {
            cursor += 1
            while cursor < end && (48...57).contains(bytes[cursor]) { cursor += 1; digits += 1 }
        }
        if digits == 0 { return false }
        if cursor < end && (bytes[cursor] == 101 || bytes[cursor] == 69) {
            cursor += 1
            if cursor < end && (bytes[cursor] == 43 || bytes[cursor] == 45) { cursor += 1 }
            let first = cursor
            while cursor < end && (48...57).contains(bytes[cursor]) { cursor += 1 }
            if cursor == first { return false }
        }
        return cursor == end
    }
    mutating func next() throws -> JSONPathToken {
        // A discarded name followed by a comment restarts this loop. Flat
        // sequences of such tokens must not grow the Swift call stack.
        while true {
            while position < bytes.count {
                if space(bytes[position]) { position += 1; continue }
                if starts(47, 42) { try comment(); continue }
                break
            }
            guard position < bytes.count else { return token(.end, []) }
            let start = position, byte = bytes[position]
            if byte == 34 { return token(.string, try quoted()) }
            if byte == 36 {
                position += 1
                if position < bytes.count && bytes[position] == 34 { return token(.variable, try quoted()) }
                let start = position
                while position < bytes.count && !special(bytes[position]) && bytes[position] != 92 { position += 1 }
                if position > start { return token(.variable, Array(bytes[start..<position])) }
                return token(.symbol, [36])
            }
            if (48...57).contains(byte) || (byte == 46 && position + 1 < bytes.count && (48...57).contains(bytes[position + 1])) {
                let end = numericEnd(at: position)
                if byte != 46 {
                    var candidate = self
                    let word = try candidate.name()
                    if candidate.position > end {
                        position = candidate.position
                        if starts(47, 42) { try comment(); continue }
                        return token(.name, word)
                    }
                }
                guard validNumber(from: start, to: end) else { throw syntax("trailing junk after numeric literal") }
                position = end
                return token(.number, Array(bytes[start..<end]))
            }
            if !special(byte) {
                let word = try name()
                if starts(47, 42) { try comment(); continue }
                return token(.name, word)
            }
            position += 1
            if position < bytes.count {
                let pair = [byte, bytes[position]]
                if ["**", "&&", "||", "==", "!=", "<>", "<=", ">="].contains(String(decoding: pair, as: UTF8.self)) { position += 1; return token(.symbol, pair) }
            }
            return token(.symbol, [byte])
        }
    }
    func token(_ kind: JSONPathToken.Kind, _ bytes: [UInt8]) -> JSONPathToken { JSONPathToken(kind: kind, text: SQLText(validatedUTF8: bytes)) }
    mutating func name() throws -> [UInt8] {
        var output: [UInt8] = []
        while position < bytes.count && !special(bytes[position]) {
            if bytes[position] == 92 { position += 1; output += try escape() }
            else { output.append(bytes[position]); position += 1 }
        }
        return output
    }
    mutating func quoted() throws -> [UInt8] {
        position += 1; var output: [UInt8] = []
        while position < bytes.count {
            let byte = bytes[position]; position += 1
            if byte == 34 { return output }
            if byte == 92 { output += try escape() } else { output.append(byte) }
        }
        throw syntax("unterminated quoted string in jsonpath input")
    }
    mutating func hexadecimal(_ count: Int?, unicode: Bool) throws -> UInt32 {
        let start = position; var result: UInt32 = 0
        while position < bytes.count && (count == nil ? bytes[position] != 125 : position - start < count!) {
            let byte = bytes[position], digit: UInt32
            switch byte { case 48...57: digit = UInt32(byte - 48); case 65...70: digit = UInt32(byte - 55); case 97...102: digit = UInt32(byte - 87); default: throw syntax("invalid Unicode escape sequence") }
            guard position - start < 6 else { throw syntax("invalid Unicode escape sequence") }
            result = result * 16 + digit; position += 1
        }
        if let count { guard position - start == count else { throw syntax("invalid Unicode escape sequence") } }
        else { guard position > start && position < bytes.count && bytes[position] == 125 else { throw syntax("invalid Unicode escape sequence") }; position += 1 }
        guard !unicode || result <= 0x10ffff else { throw syntax("invalid Unicode code point") }
        return result
    }
    mutating func unicodeEscape() throws -> UInt32 {
        if position < bytes.count && bytes[position] == 123 { position += 1; return try hexadecimal(nil, unicode: true) }
        return try hexadecimal(4, unicode: true)
    }
    mutating func escape() throws -> [UInt8] {
        guard position < bytes.count else { throw syntax() }
        let byte = bytes[position]; position += 1
        if byte == 117 {
            var scalars = [try unicodeEscape()]
            while starts(92, 117) { position += 2; scalars.append(try unicodeEscape()) }
            var output: [UInt8] = [], high: UInt32?
            for var scalar in scalars {
                if (0xd800...0xdbff).contains(scalar) {
                    if high != nil { throw surrogate(high: true) }; high = scalar; continue
                }
                if (0xdc00...0xdfff).contains(scalar) {
                    guard let first = high else { throw surrogate(high: false) }
                    scalar = 0x10000 + (first - 0xd800) * 1024 + scalar - 0xdc00; high = nil
                } else if high != nil { throw surrogate(high: false) }
                output += try scalarBytes(scalar)
            }
            if high != nil { throw surrogate(high: false) }
            return output
        }
        if byte == 120 { return try scalarBytes(hexadecimal(2, unicode: false)) }
        switch byte { case 98: return [8]; case 102: return [12]; case 110: return [10]; case 114: return [13]; case 116: return [9]; case 118: return [11]; default: return [byte] }
    }
    func scalarBytes(_ value: UInt32) throws -> [UInt8] {
        guard value != 0 else { var error = JedError("22P05", "unsupported Unicode escape sequence"); error.detail = "\\u0000 cannot be converted to text."; throw error }
        guard let scalar = Unicode.Scalar(value) else { throw syntax("invalid Unicode code point") }
        return Array(String(scalar).utf8)
    }
    func surrogate(high: Bool) -> JedError {
        var error = JedError("22P02", "invalid input syntax for type jsonpath")
        error.detail = high ? "Unicode high surrogate must not follow a high surrogate." : "Unicode low surrogate must follow a high surrogate."
        return error
    }
}

private struct JSONPathParser {
    struct Parsed {
        var expression: JSONPathExpression
        var predicate: JSONPathPredicate?
        var parenthesized = false
        var nesting = 0
        static func predicate(_ value: JSONPathPredicate, nesting: Int) -> Parsed {
            Parsed(expression: .predicate(value), predicate: value, nesting: nesting)
        }
    }
    var lexer: JSONPathLexer
    let meter: CostMeter?
    let regexCompiler: JSONPathRegexCompiler?
    var lookahead: JSONPathToken?
    var depth = 0
    var filterDepth = 0
    var subscriptDepth = 0

    init(text: SQLText, meter: CostMeter?, regexCompiler: JSONPathRegexCompiler?) {
        lexer = JSONPathLexer(bytes: text.utf8); self.meter = meter; self.regexCompiler = regexCompiler
    }
    func syntax() -> JedError { JedError("42601", "syntax error in jsonpath input") }
    func bound(_ nesting: Int) throws {
        guard nesting <= 256 else { throw JedError("54001", "jsonpath nesting depth exceeds the maximum of 256") }
    }
    mutating func enter() throws { depth += 1; try bound(depth) }
    mutating func peek() throws -> JSONPathToken {
        if lookahead == nil { lookahead = try lexer.next() }
        return lookahead!
    }
    mutating func next() throws -> JSONPathToken { let token = try peek(); lookahead = nil; return token }
    mutating func take(_ spelling: String) throws -> Bool {
        let token = try peek()
        if (token.kind == .symbol && token.spelling == spelling) || (token.kind == .name && token.keyword == spelling) {
            _ = try next(); return true
        }
        return false
    }
    mutating func expect(_ spelling: String) throws { guard try take(spelling) else { throw syntax() } }
    mutating func parse() throws -> (JSONPathExpression, Bool) {
        if try peek().kind == .end { throw JedError("22P02", "invalid input syntax for type jsonpath") }
        let strict = try take("strict")
        if !strict { _ = try take("lax") }
        let result = try parseNode(minimum: 0)
        guard try peek().kind == .end else { throw syntax() }
        return (result.expression, strict)
    }
    func requireExpression(_ parsed: Parsed) throws -> JSONPathExpression {
        guard parsed.predicate == nil else { throw syntax() }; return parsed.expression
    }
    func requirePredicate(_ parsed: Parsed) throws -> JSONPathPredicate {
        guard let predicate = parsed.predicate else { throw syntax() }; return predicate
    }
    mutating func parseNode(minimum: Int) throws -> Parsed {
        var left = try prefix()
        left = try accessors(left)
        while true {
            let token = try peek(), spelling = token.kind == .name ? token.keyword : token.spelling
            let precedence: Int
            switch spelling {
            case "||": precedence = 0
            case "&&": precedence = 1
            case "==", "!=", "<>", "<", "<=", ">", ">=", "starts", "like_regex": precedence = 3
            case "+", "-": precedence = 4
            case "*", "/", "%": precedence = 5
            case "is": precedence = 7
            default: return left
            }
            if precedence < minimum { return left }
            _ = try next()
            if spelling == "is" {
                guard left.parenthesized else { throw syntax() }
                try expect("unknown")
                left = .predicate(.unknown(try requirePredicate(left)), nesting: left.nesting)
            } else if spelling == "like_regex" {
                let operand = try requireExpression(left), pattern = try next()
                guard pattern.kind == .string else { throw syntax() }
                var flags: SQLText = SQLText(validatedUTF8: [])
                if try take("flag") {
                    let supplied = try next(); guard supplied.kind == .string else { throw syntax() }
                    for byte in supplied.text.utf8 where !Array("ismxq".utf8).contains(byte) {
                        throw JedError("42601", "Unrecognized flag character \"\(String(Unicode.Scalar(byte)))\" in LIKE_REGEX predicate.")
                    }
                    flags = SQLText(validatedUTF8: Array("ismxq".utf8).filter { supplied.text.utf8.contains($0) })
                    if flags.utf8.contains(120) && !flags.utf8.contains(113) { throw JedError("0A000", "XQuery \"x\" flag (expanded regular expressions) is not implemented") }
                }
                guard let regexCompiler else { throw JedError("0A000", "jsonpath regular-expression provider is not installed") }
                let program = try regexCompiler(pattern.text, flags, meter)
                left = .predicate(.regex(operand, pattern.text, flags, program), nesting: left.nesting)
            } else if spelling == "starts" {
                try expect("with")
                let right = try next(), expression: JSONPathExpression
                if right.kind == .string { expression = .literal(.string(right.text)) }
                else if right.kind == .variable { expression = .variable(right.text) }
                else { throw syntax() }
                left = .predicate(.starts(try requireExpression(left), expression), nesting: left.nesting)
            } else {
                let right = try parseNode(minimum: precedence + 1)
                let nesting = max(left.nesting, right.nesting) + 1
                switch spelling {
                case "||": left = .predicate(.or(try requirePredicate(left), try requirePredicate(right)), nesting: nesting)
                case "&&": left = .predicate(.and(try requirePredicate(left), try requirePredicate(right)), nesting: nesting)
                case "==", "!=", "<>", "<", "<=", ">", ">=":
                    left = .predicate(.compare(spelling, try requireExpression(left), try requireExpression(right)), nesting: nesting - 1)
                default:
                    left = Parsed(expression: .binary(spelling, try requireExpression(left), try requireExpression(right)), nesting: nesting)
                }
            }
            try bound(depth + left.nesting)
        }
    }
    mutating func prefix() throws -> Parsed {
        let token = try next()
        if token.kind == .number { return Parsed(expression: .literal(.number(try SQLDecimal.parse(token.spelling)))) }
        if token.kind == .string { return Parsed(expression: .literal(.string(token.text))) }
        if token.kind == .variable { return Parsed(expression: .variable(token.text)) }
        if token.kind == .name {
            switch token.keyword {
            case "last": guard subscriptDepth > 0 else { throw syntax() }; return Parsed(expression: .last)
            case "null" where token.spelling == "null": return Parsed(expression: .literal(.null))
            case "true" where token.spelling == "true": return Parsed(expression: .literal(.boolean(true)))
            case "false" where token.spelling == "false": return Parsed(expression: .literal(.boolean(false)))
            case "exists":
                try enter(); defer { depth -= 1 }
                try expect("("); let value = try parseNode(minimum: 0); try expect(")")
                return .predicate(.exists(try requireExpression(value)), nesting: value.nesting + 1)
            default: throw syntax()
            }
        }
        switch token.spelling {
        case "$": return Parsed(expression: .root)
        case "@": guard filterDepth > 0 else { throw syntax() }; return Parsed(expression: .current)
        case "(":
            try enter(); defer { depth -= 1 }
            var value = try parseNode(minimum: 0); try expect(")")
            value.parenthesized = true
            return value
        case "!":
            let look = try peek()
            guard look.spelling == "(" || (look.kind == .name && look.keyword == "exists") else { throw syntax() }
            // The mandatory group (or exists operand) contributes the one
            // nesting level; ! does not count that same delimiter again.
            let child = try prefix()
            return .predicate(.not(try requirePredicate(child)), nesting: child.nesting + (look.spelling == "(" ? 1 : 0))
        case "+", "-":
            try enter(); defer { depth -= 1 }
            let child = try parseNode(minimum: 6), expression = try requireExpression(child)
            if case .literal(.number(let number)) = expression {
                return Parsed(expression: .literal(.number(token.spelling == "-" ? number.negated() : number)))
            }
            return Parsed(expression: .unary(token.spelling, expression), nesting: child.nesting + 1)
        default: throw syntax()
        }
    }
    mutating func accessors(_ initial: Parsed) throws -> Parsed {
        var value = initial
        let base: JSONPathExpression
        var steps: [JSONPathStep]
        if case .path(let original, let existing) = initial.expression { base = original; steps = existing }
        else { base = initial.expression; steps = [] }
        while true {
            let symbol = try peek().spelling
            guard [".", "[", "?"].contains(symbol) else {
                if steps.isEmpty { return value }
                return Parsed(expression: .path(base, steps), nesting: value.nesting)
            }
            guard value.predicate == nil || value.parenthesized else { throw syntax() }
            _ = try next()
            let step: JSONPathStep
            var extraDepth = 0
            switch symbol {
            case ".":
                if try take("**") {
                    var first: Int? = 0, last: Int?
                    if try take("{") {
                        first = try level(); last = first
                        if try take("to") { last = try level() }
                        try expect("}")
                    }
                    step = .any(first, last)
                } else if try take("*") { step = .members }
                else {
                    let token = try next(); guard token.kind == .name || token.kind == .string else { throw syntax() }
                    if try token.kind == .name && take("(") {
                        let name = token.keyword
                        let simple = ["type", "size", "double", "ceiling", "floor", "abs", "keyvalue", "bigint", "boolean", "integer", "number", "string"]
                        let datetime = ["datetime", "date", "time", "time_tz", "timestamp", "timestamp_tz"]
                        var args: [String] = []
                        if name == "decimal" {
                            if try !take(")") {
                                args.append(try integer())
                                if try take(",") { args.append(try integer()) }
                                try expect(")")
                            }
                        } else if datetime.contains(name) {
                            if try !take(")") {
                                if name == "datetime" { guard try next().kind == .string else { throw syntax() } }
                                else { _ = try integer() }
                                try expect(")")
                            }
                            throw JedError("0A000", "jsonpath datetime methods are not supported")
                        } else { guard simple.contains(name) else { throw syntax() }; try expect(")") }
                        step = .method(name, args)
                    } else { step = .member(token.text) }
                }
            case "[":
                try enter(); subscriptDepth += 1
                if try take("*") { try expect("]"); step = .elements }
                else {
                    var subscripts: [(JSONPathExpression, JSONPathExpression?)] = []
                    repeat {
                        let first = try parseNode(minimum: 0)
                        var last: Parsed?
                        if try take("to") { last = try parseNode(minimum: 0) }
                        extraDepth = max(extraDepth, max(first.nesting, last?.nesting ?? 0) + 1)
                        subscripts.append((try requireExpression(first), try last.map(requireExpression)))
                    } while try take(",")
                    try expect("]"); step = .subscripts(subscripts)
                }
                subscriptDepth -= 1; depth -= 1
            default:
                try enter(); filterDepth += 1
                try expect("("); let predicate = try parseNode(minimum: 0); try expect(")")
                step = .filter(try requirePredicate(predicate)); extraDepth = predicate.nesting + 1
                filterDepth -= 1; depth -= 1
            }
            steps.append(step)
            value.predicate = nil; value.parenthesized = false
            value.nesting = max(value.nesting, extraDepth)
            try bound(depth + value.nesting)
        }
    }
    mutating func integer() throws -> String {
        let negative = try take("-")
        if !negative { _ = try take("+") }
        let token = try next()
        guard token.kind == .number && !token.text.utf8.isEmpty && token.text.utf8.allSatisfy({ (48...57).contains($0) }) else { throw syntax() }
        return (negative && token.spelling != "0" ? "-" : "") + token.spelling
    }
    mutating func level() throws -> Int? {
        if try take("last") { return nil }
        let token = try next()
        guard token.kind == .number && token.text.utf8.allSatisfy({ (48...57).contains($0) }) else { throw syntax() }
        guard let number = Int(token.spelling), number <= Int32.max else { throw JedError("22003", "value \"\(token.spelling)\" is out of range for type integer") }
        return number
    }
}
