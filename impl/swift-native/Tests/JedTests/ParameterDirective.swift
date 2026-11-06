@testable import Jed

/// The shared corpus's host-value spelling, not SQL expression evaluation.
/// In particular ARRAY elements remain untyped until real parameter binding.
struct ParameterDirective {
    let bytes: [UInt8]
    var offset = 0
    static func parse(_ source: String) throws -> [SQLValue] {
        var parser = ParameterDirective(bytes: Array(source.utf8))
        return try parser.list(until: nil, depth: 0)
    }
    mutating func spaces() { while offset < bytes.count && [9, 10, 13, 32].contains(bytes[offset]) { offset += 1 } }
    mutating func list(until closer: UInt8?, depth: Int) throws -> [SQLValue] {
        guard depth <= 64 else { throw malformed() }
        var values: [SQLValue] = []
        spaces()
        if let closer, offset < bytes.count, bytes[offset] == closer { offset += 1; return values }
        if closer == nil && offset == bytes.count { return values }
        while true {
            values.append(try value(depth: depth)); spaces()
            if let closer {
                guard offset < bytes.count else { throw malformed() }
                if bytes[offset] == closer { offset += 1; return values }
            } else if offset == bytes.count { return values }
            guard offset < bytes.count, bytes[offset] == 44 else { throw malformed() }
            offset += 1; spaces()
        }
    }
    mutating func value(depth: Int) throws -> SQLValue {
        spaces()
        guard offset < bytes.count else { throw malformed() }
        if bytes[offset] == 39 {
            offset += 1
            var text: [UInt8] = []
            while offset < bytes.count {
                let byte = bytes[offset]; offset += 1
                if byte != 39 { text.append(byte); continue }
                if offset < bytes.count && bytes[offset] == 39 { text.append(39); offset += 1; continue }
                return .text(try SQLText(utf8: text))
            }
            throw malformed()
        }
        let start = offset
        while offset < bytes.count && ![9, 10, 13, 32, 44, 41, 93, 40, 91].contains(bytes[offset]) { offset += 1 }
        guard start < offset else { throw malformed() }
        let token = Array(bytes[start..<offset]), word = String(decoding: token.map { (65...90).contains($0) ? $0 + 32 : $0 }, as: UTF8.self)
        if word == "null" { return .null }
        if word == "true" || word == "false" { return .boolean(word == "true") }
        if word == "array" || word == "row" {
            spaces()
            let opening: UInt8 = word == "array" ? 91 : 40
            guard offset < bytes.count, bytes[offset] == opening else { throw malformed() }; offset += 1
            let items = try list(until: word == "array" ? 93 : 41, depth: depth + 1)
            if word == "array" { return .array(try SQLArray(elements: items)) }
            guard !items.isEmpty else { throw malformed() }
            return .composite(items)
        }
        var digits = token
        if digits.first == 45 { digits.removeFirst() }
        let parts = digits.split(separator: 46, omittingEmptySubsequences: false)
        guard parts.count == 1 || parts.count == 2,
              parts.allSatisfy({ !$0.isEmpty && $0.allSatisfy { (48...57).contains($0) } }) else { throw malformed() }
        let spelling = String(decoding: token, as: UTF8.self)
        if parts.count == 2 { return .decimal(try SQLDecimal.parse(spelling)) }
        guard let integer = Int64(spelling) else { throw malformed() }
        return .integer(integer)
    }
    func malformed() -> JedError { JedError("XX000", "malformed corpus parameter directive at byte \(offset + 1)") }
}
