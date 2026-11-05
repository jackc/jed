/// Valid, NUL-free UTF-8 with byte identity. Swift String's canonical Unicode
/// equivalence is deliberately not SQL equality (spec/design/types.md §11).
public struct SQLText: Sendable, Hashable, Comparable, CustomStringConvertible {
    public let utf8: [UInt8]

    public init(_ text: String) throws {
        try self.init(utf8: Array(text.utf8))
    }

    public init(utf8: [UInt8]) throws {
        try validateSQLUTF8(utf8)
        self.utf8 = utf8
    }

    /// Only for engine-produced text whose construction preserves valid UTF-8.
    internal init(validatedUTF8: [UInt8]) { self.utf8 = validatedUTF8 }

    public var description: String { String(decoding: utf8, as: UTF8.self) }
    public var scalarCount: Int {
        utf8.reduce(0) { $0 + (($1 & 0xc0) == 0x80 ? 0 : 1) }
    }

    public static func < (lhs: SQLText, rhs: SQLText) -> Bool {
        lhs.utf8.lexicographicallyPrecedes(rhs.utf8)
    }

    /// SQL identifiers fold only ASCII A-Z; locale and Unicode case folding are
    /// separate, explicitly loaded operations.
    internal var asciiLowercased: SQLText {
        SQLText(validatedUTF8: utf8.map { (65...90).contains($0) ? $0 + 32 : $0 })
    }
}

/// Validate at the host/SQL boundary before tokenization, including comments.
/// Diagnostic bytes include up to the lead byte's expected sequence length,
/// matching the shared UTF-8 input contract, and deliberately have no position.
internal func validateSQLUTF8(_ bytes: [UInt8]) throws {
    var index = 0
    while index < bytes.count {
        let first = bytes[index]
        if first > 0 && first < 0x80 { index += 1; continue }
        let length: Int
        if first >= 0xc0 && first < 0xe0 { length = 2 }
        else if first >= 0xe0 && first < 0xf0 { length = 3 }
        else if first >= 0xf0 && first < 0xf8 { length = 4 }
        else { length = 1 }
        var valid = length > 1 && length <= bytes.count - index
        if valid {
            for offset in 1..<length where bytes[index + offset] & 0xc0 != 0x80 {
                valid = false
            }
            if first < 0xc2 || first > 0xf4 { valid = false }
            if first == 0xe0 && bytes[index + 1] < 0xa0 { valid = false }
            if first == 0xed && bytes[index + 1] >= 0xa0 { valid = false }
            if first == 0xf0 && bytes[index + 1] < 0x90 { valid = false }
            if first == 0xf4 && bytes[index + 1] >= 0x90 { valid = false }
        }
        guard valid else {
            let digits = Array("0123456789abcdef".utf8)
            let end = index + min(length, bytes.count - index)
            let hex = bytes[index..<end].map {
                String(decoding: [48, 120, digits[Int($0 >> 4)], digits[Int($0 & 15)]], as: UTF8.self)
            }.joined(separator: " ")
            throw JedError("22021", "invalid byte sequence for encoding \"UTF8\": \(hex)")
        }
        index += length
    }
}
