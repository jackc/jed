internal enum TokenKind: Equatable, Sendable {
    case word(SQLText, quoted: Bool)
    /// Original unsigned numeric spelling, including radix/separators/exponent.
    case number(String)
    case string(SQLText)
    case parameter(Int)
    case symbol(String)
    case end
}

internal struct SQLToken: Equatable, Sendable {
    let kind: TokenKind
    let byteStart: Int
    let byteEnd: Int
    /// One-based Unicode scalar position, never a Swift grapheme index.
    let position: Int
}

/// Shared boundary recognition for the strict single-statement lexer and total
/// host script splitter. It performs no parsing or SQL execution.
internal enum SQLLexicalBoundary {
    static func isLetter(_ byte: UInt8) -> Bool {
        (65...90).contains(byte) || (97...122).contains(byte)
    }
    static func isDigit(_ byte: UInt8) -> Bool { (48...57).contains(byte) }
    static func isWordStart(_ byte: UInt8) -> Bool { isLetter(byte) || byte == 95 }
    static func isWhitespace(_ byte: UInt8) -> Bool {
        byte == 32 || byte == 9 || byte == 10 || byte == 13
    }
    static func quotedEnd(_ bytes: [UInt8], afterOpening: Int, quote: UInt8) -> (end: Int, closed: Bool) {
        var cursor = afterOpening
        while cursor < bytes.count {
            if bytes[cursor] == quote {
                if cursor + 1 < bytes.count && bytes[cursor + 1] == quote { cursor += 2 }
                else { return (cursor + 1, true) }
            } else { cursor += 1 }
        }
        return (cursor, false)
    }
    static func blockCommentEnd(_ bytes: [UInt8], afterOpening: Int) -> (end: Int, closed: Bool) {
        var cursor = afterOpening
        var depth = 1
        while cursor + 1 < bytes.count {
            if bytes[cursor] == 47 && bytes[cursor + 1] == 42 { depth += 1; cursor += 2 }
            else if bytes[cursor] == 42 && bytes[cursor + 1] == 47 {
                depth -= 1
                cursor += 2
                if depth == 0 { return (cursor, true) }
            } else { cursor += 1 }
        }
        return (bytes.count, false)
    }
    static func dollarDelimiterLength(_ bytes: [UInt8], at start: Int) -> Int {
        var cursor = start + 1
        if cursor < bytes.count && bytes[cursor] == 36 { return 2 }
        guard cursor < bytes.count && isWordStart(bytes[cursor]) else { return 0 }
        cursor += 1
        while cursor < bytes.count && (isWordStart(bytes[cursor]) || isDigit(bytes[cursor])) { cursor += 1 }
        return cursor < bytes.count && bytes[cursor] == 36 ? cursor + 1 - start : 0
    }
}

/// Handwritten scanner for grammar.ebnf's lexical layer. Admission limits on
/// the complete statement belong to the parser/session, before constructing it.
internal struct SQLLexer {
    private let bytes: [UInt8]
    private static let maximumIdentifierBytes = 63
    private static let maximumParameter = 65_535
    private static let multiSymbols: [[UInt8]] = [
        "->>", "#>>", "!~*", "-|-", "<=", ">=", "<>", "!=", "||", "::", "=>",
        "<@", "<<", ">>", "!~", "~*", "@>", "@?", "@@", "#>", "#-", "?|", "?&",
        "&&", "&<", "&>", "->",
    ].map { Array($0.utf8) }
    private static let singleSymbols: Set<UInt8> = Set(",()[]*+-/%:=<>~?.".utf8)

    init(_ sql: String) throws { try self.init(utf8: Array(sql.utf8)) }
    init(utf8: [UInt8]) throws {
        try validateSQLUTF8(utf8)
        bytes = utf8
    }

    private func characterPosition(at byteOffset: Int) -> Int {
        1 + bytes.prefix(byteOffset).reduce(0) { $0 + (($1 & 0xc0) == 0x80 ? 0 : 1) }
    }
    private func syntax(_ message: String, at byteOffset: Int) -> JedError {
        JedError("42601", message).at(characterPosition(at: byteOffset))
    }
    private func nameTooLong(at byteOffset: Int) -> JedError {
        JedError("42622", "identifier exceeds the maximum length of 63 bytes").at(characterPosition(at: byteOffset))
    }
    private func text(_ start: Int, _ end: Int) -> String {
        String(decoding: bytes[start..<end], as: UTF8.self)
    }
    private func at(_ offset: Int) -> UInt8 { offset < bytes.count ? bytes[offset] : 0 }

    func tokenize() throws -> [SQLToken] {
        var cursor = 0
        var tokens: [SQLToken] = []
        var unicodeTokens = Set<Int>()
        func append(_ kind: TokenKind, from start: Int) {
            tokens.append(SQLToken(kind: kind, byteStart: start, byteEnd: cursor, position: 0))
        }
        while cursor < bytes.count {
            let start = cursor
            let first = bytes[cursor]
            if SQLLexicalBoundary.isWhitespace(first) { cursor += 1; continue }
            if first == 45 && at(cursor + 1) == 45 {
                cursor += 2
                while cursor < bytes.count && bytes[cursor] != 10 && bytes[cursor] != 13 { cursor += 1 }
                continue
            }
            if first == 47 && at(cursor + 1) == 42 {
                let boundary = SQLLexicalBoundary.blockCommentEnd(bytes, afterOpening: cursor + 2)
                guard boundary.closed else { throw syntax("unterminated /* comment", at: start) }
                cursor = boundary.end
                continue
            }
            let unicode = (first == 85 || first == 117) && at(cursor + 1) == 38
                && (at(cursor + 2) == 39 || at(cursor + 2) == 34)
            if first == 39 || first == 34 || unicode {
                let quote = unicode ? bytes[cursor + 2] : first
                let bodyStart = cursor + (unicode ? 3 : 1)
                let boundary = SQLLexicalBoundary.quotedEnd(bytes, afterOpening: bodyStart, quote: quote)
                guard boundary.closed else {
                    throw syntax(quote == 39 ? "unterminated string literal" : "unterminated quoted identifier", at: start)
                }
                var body: [UInt8] = []
                var index = bodyStart
                while index < boundary.end - 1 {
                    body.append(bytes[index])
                    index += bytes[index] == quote ? 2 : 1
                }
                if quote == 34 {
                    guard !body.isEmpty else { throw syntax("zero-length delimited identifier", at: start) }
                    if !unicode && body.count > Self.maximumIdentifierBytes { throw nameTooLong(at: start) }
                }
                if unicode { unicodeTokens.insert(tokens.count) }
                cursor = boundary.end
                let value = SQLText(validatedUTF8: body)
                append(quote == 39 ? .string(value) : .word(value, quoted: true), from: start)
                continue
            }
            if SQLLexicalBoundary.isDigit(first) || (first == 46 && SQLLexicalBoundary.isDigit(at(cursor + 1))) {
                cursor = try numberEnd(start)
                append(.number(text(start, cursor)), from: start)
                continue
            }
            if first == 36 {
                cursor += 1
                let digitsStart = cursor
                while cursor < bytes.count && SQLLexicalBoundary.isDigit(bytes[cursor]) { cursor += 1 }
                guard cursor > digitsStart else { throw syntax("expected a parameter number after '$'", at: start) }
                let digits = text(digitsStart, cursor)
                guard bytes[digitsStart] != 48 else {
                    throw syntax("invalid parameter number $\(digits): parameters are 1-based with no leading zero", at: start)
                }
                var parameter = 0
                var overflow = false
                for digit in bytes[digitsStart..<cursor] {
                    if parameter > (Self.maximumParameter - Int(digit - 48)) / 10 { overflow = true; break }
                    parameter = parameter * 10 + Int(digit - 48)
                }
                guard !overflow else {
                    throw syntax("parameter number out of range: $\(digits) (the maximum is $65535)", at: start)
                }
                append(.parameter(parameter), from: start)
                continue
            }
            if SQLLexicalBoundary.isWordStart(first) {
                cursor += 1
                while cursor < bytes.count && (SQLLexicalBoundary.isWordStart(bytes[cursor]) || SQLLexicalBoundary.isDigit(bytes[cursor])) { cursor += 1 }
                guard cursor - start <= Self.maximumIdentifierBytes else { throw nameTooLong(at: start) }
                append(.word(SQLText(validatedUTF8: Array(bytes[start..<cursor])).asciiLowercased, quoted: false), from: start)
                continue
            }
            if let symbol = Self.multiSymbols.first(where: { bytes[start...].starts(with: $0) }) {
                cursor += symbol.count
                let name = String(decoding: symbol, as: UTF8.self)
                append(.symbol(name == "!=" ? "<>" : name), from: start)
                continue
            }
            if Self.singleSymbols.contains(first) {
                cursor += 1
                append(.symbol(text(start, cursor)), from: start)
                continue
            }
            // The baseline's bare words are ASCII-only. Report the offending byte
            // without normalizing or interpreting a host locale.
            let character = String(UnicodeScalar(UInt32(first))!)
            throw syntax("unexpected character '\(character)'", at: start)
        }
        tokens.append(SQLToken(kind: .end, byteStart: bytes.count, byteEnd: bytes.count, position: 0))
        if !unicodeTokens.isEmpty { tokens = try applyUnicodeEscapes(tokens, indices: unicodeTokens) }
        // Starts are ascending even after UESCAPE clauses are consumed. One pass
        // gives scalar positions without an O(tokens × input) prefix recount.
        var counted = 0
        var position = 1
        return tokens.map { token in
            while counted < token.byteStart {
                if bytes[counted] & 0xc0 != 0x80 { position += 1 }
                counted += 1
            }
            return SQLToken(kind: token.kind, byteStart: token.byteStart, byteEnd: token.byteEnd, position: position)
        }
    }

    private func digitValue(_ byte: UInt8, base: UInt64) -> UInt64? {
        let value: UInt64
        switch byte {
        case 48...57: value = UInt64(byte - 48)
        case 65...70: value = UInt64(byte - 65 + 10)
        case 97...102: value = UInt64(byte - 97 + 10)
        default: return nil
        }
        return value < base ? value : nil
    }
    private func digitRunEnd(_ start: Int, base: UInt64) -> Int {
        var end = start
        while end < bytes.count {
            if digitValue(bytes[end], base: base) != nil { end += 1 }
            else if bytes[end] == 95 && end + 1 < bytes.count && digitValue(bytes[end + 1], base: base) != nil { end += 2 }
            else { break }
        }
        return end
    }
    private func gluedWordEnd(_ start: Int) -> Int {
        func starts(_ byte: UInt8) -> Bool { SQLLexicalBoundary.isWordStart(byte) || byte >= 128 }
        guard start < bytes.count && starts(bytes[start]) else { return start }
        var end = start + 1
        while end < bytes.count && (starts(bytes[end]) || SQLLexicalBoundary.isDigit(bytes[end]) || bytes[end] == 36) { end += 1 }
        return end
    }
    private func validateMagnitude(_ start: Int, digitsStart: Int, end: Int, base: UInt64) throws {
        var magnitude: UInt64 = 0
        let maximum: UInt64 = 1 << 63
        for byte in bytes[digitsStart..<end] where byte != 95 {
            let digit = digitValue(byte, base: base)!
            guard magnitude <= (maximum - digit) / base else {
                throw syntax("integer literal out of range: \(text(start, end))", at: start)
            }
            magnitude = magnitude * base + digit
        }
    }
    private func numberEnd(_ start: Int) throws -> Int {
        func junk(_ end: Int) -> JedError {
            syntax("trailing junk after numeric literal at or near \"\(text(start, end))\"", at: start)
        }
        if at(start) == 48 {
            let radix: (UInt64, String)?
            switch at(start + 1) {
            case 88, 120: radix = (16, "hexadecimal")
            case 79, 111: radix = (8, "octal")
            case 66, 98: radix = (2, "binary")
            default: radix = nil
            }
            if let (base, name) = radix {
                let end = digitRunEnd(start + 2, base: base)
                let junkEnd = gluedWordEnd(start + 1)
                if end > start + 2 {
                    if junkEnd > end { throw junk(junkEnd) }
                    try validateMagnitude(start, digitsStart: start + 2, end: end, base: base)
                    return end
                }
                let failEnd = start + 2 + (at(start + 2) == 95 ? 1 : 0)
                if junkEnd > failEnd { throw junk(junkEnd) }
                throw syntax("invalid \(name) integer at or near \"\(text(start, failEnd))\"", at: start)
            }
        }
        let integerEnd = SQLLexicalBoundary.isDigit(at(start)) ? digitRunEnd(start, base: 10) : start
        var end = integerEnd
        var decimal = false
        if at(end) == 46 {
            if at(end + 1) == 46 {
                try validateMagnitude(start, digitsStart: start, end: end, base: 10)
                return end
            }
            decimal = true
            end += 1
            if SQLLexicalBoundary.isDigit(at(end)) { end = digitRunEnd(end, base: 10) }
        }
        if at(end) == 69 || at(end) == 101 {
            var digitStart = end + 1
            let signed = at(digitStart) == 43 || at(digitStart) == 45
            if signed { digitStart += 1 }
            if SQLLexicalBoundary.isDigit(at(digitStart)) {
                decimal = true
                end = digitRunEnd(digitStart, base: 10)
            } else if signed { throw junk(digitStart) }
        }
        let junkEnd = gluedWordEnd(end)
        if junkEnd > end { throw junk(junkEnd) }
        if !decimal { try validateMagnitude(start, digitsStart: start, end: end, base: 10) }
        return end
    }

    private func applyUnicodeEscapes(_ tokens: [SQLToken], indices: Set<Int>) throws -> [SQLToken] {
        var output: [SQLToken] = []
        var index = 0
        while index < tokens.count {
            let token = tokens[index]
            guard indices.contains(index) else { output.append(token); index += 1; continue }
            var escape: UInt8 = 92
            var next = index + 1
            var byteEnd = token.byteEnd
            if case .word(let name, quoted: false) = tokens[index + 1].kind,
               name.utf8 == Array("uescape".utf8) {
                let argument = tokens[index + 2]
                guard case .string(let value) = argument.kind, !indices.contains(index + 2) else {
                    throw syntax("UESCAPE must be followed by a simple string literal", at: argument.byteStart)
                }
                guard value.utf8.count == 1, let byte = value.utf8.first,
                      digitValue(byte, base: 16) == nil,
                      ![UInt8(43), 39, 34, 11, 12].contains(byte),
                      !SQLLexicalBoundary.isWhitespace(byte) else {
                    throw syntax("invalid Unicode escape character", at: argument.byteStart)
                }
                escape = byte
                next = index + 3
                byteEnd = argument.byteEnd
            }
            let body: SQLText
            let identifier: Bool
            switch token.kind {
            case .string(let value): body = value; identifier = false
            case .word(let value, quoted: true): body = value; identifier = true
            default: preconditionFailure("Unicode token invariant")
            }
            let decoded = try decodeUnicode(body.utf8, escape: escape, tokenStart: token.byteStart)
            if identifier && decoded.count > Self.maximumIdentifierBytes { throw nameTooLong(at: token.byteStart) }
            let value = SQLText(validatedUTF8: decoded)
            output.append(SQLToken(kind: identifier ? .word(value, quoted: true) : .string(value),
                                   byteStart: token.byteStart, byteEnd: byteEnd, position: 0))
            index = next
        }
        return output
    }

    private func decodeUnicode(_ body: [UInt8], escape: UInt8, tokenStart: Int) throws -> [UInt8] {
        var output: [UInt8] = []
        var cursor = 0
        var highSurrogate: UInt32?
        func failure(_ message: String, at offset: Int) -> JedError {
            syntax(message, at: tokenStart + 3 + offset)
        }
        func hex(_ start: Int, count: Int) -> UInt32? {
            guard start <= body.count && count <= body.count - start else { return nil }
            var value: UInt32 = 0
            for byte in body[start..<start + count] {
                guard let digit = digitValue(byte, base: 16) else { return nil }
                value = (value << 4) | UInt32(digit)
            }
            return value
        }
        while cursor < body.count {
            if body[cursor] != escape {
                if highSurrogate != nil { throw failure("invalid Unicode surrogate pair", at: cursor) }
                output.append(body[cursor]); cursor += 1; continue
            }
            if cursor + 1 < body.count && body[cursor + 1] == escape {
                if highSurrogate != nil { throw failure("invalid Unicode surrogate pair", at: cursor) }
                output.append(escape); cursor += 2; continue
            }
            var codepoint: UInt32
            let length: Int
            if let value = hex(cursor + 1, count: 4) { codepoint = value; length = 5 }
            else if cursor + 1 < body.count && body[cursor + 1] == 43, let value = hex(cursor + 2, count: 6) {
                codepoint = value; length = 8
            } else {
                var error = failure("invalid Unicode escape", at: cursor)
                error.hint = "Unicode escapes must be \\XXXX or \\+XXXXXX."
                throw error
            }
            guard codepoint > 0 && codepoint <= 0x10ffff else { throw failure("invalid Unicode escape value", at: cursor) }
            if let high = highSurrogate {
                guard (0xdc00...0xdfff).contains(codepoint) else { throw failure("invalid Unicode surrogate pair", at: cursor) }
                codepoint = ((high & 0x3ff) << 10 | (codepoint & 0x3ff)) + 0x10000
                highSurrogate = nil
            } else if (0xdc00...0xdfff).contains(codepoint) { throw failure("invalid Unicode surrogate pair", at: cursor) }
            else if (0xd800...0xdbff).contains(codepoint) { highSurrogate = codepoint; cursor += length; continue }
            output.append(contentsOf: String(UnicodeScalar(codepoint)!).utf8)
            cursor += length
        }
        if highSurrogate != nil { throw failure("invalid Unicode surrogate pair", at: body.count) }
        return output
    }
}
