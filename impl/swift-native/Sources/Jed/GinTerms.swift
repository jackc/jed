/// Pure term extraction shared by index maintenance, query bounds, and ANALYZE.
/// These are bytes: truncation may split UTF-8 and never decodes the cut prefix.
internal enum GinTerms {
    static let maximumContentBytes = 128

    static func terminated(_ bytes: some Sequence<UInt8>) -> [UInt8] {
        var result: [UInt8] = []
        for byte in bytes { result.append(byte); if byte == 0 { result.append(255) } }
        result.append(contentsOf: [0, 1]); return result
    }
    static func distinct(_ terms: [[UInt8]]) -> [[UInt8]] {
        let ordered = terms.sorted { $0.lexicographicallyPrecedes($1) }
        var result: [[UInt8]] = []
        for term in ordered where result.last != term { result.append(term) }
        return result
    }
    private static func jsonFramed(_ flag: UInt8, _ bytes: [UInt8]) -> [UInt8] {
        [flag | (bytes.count > maximumContentBytes ? 0x10 : 0)] + terminated(bytes.prefix(maximumContentBytes))
    }
    static func jsonKey(_ key: SQLText) -> [UInt8] { jsonFramed(1, key.utf8) }
    static func jsonScalar(_ value: SQLJSONValue, asKey: Bool) throws -> [UInt8] {
        switch value {
        case .null: return [2]
        case .boolean(let value): return [3, value ? 1 : 0]
        case .number(let value): return jsonFramed(4, value.keyBytes())
        case .string(let value): return jsonFramed(asKey ? 1 : 5, value.utf8)
        default: throw JedError("42804", "GIN scalar term requires a JSON scalar")
        }
    }
    static func jsonIndex(_ document: SQLJSONValue) throws -> [[UInt8]] {
        var terms: [[UInt8]] = []
        func walk(_ value: SQLJSONValue, asKey: Bool, depth: Int) throws {
            switch value {
            case .object(let members):
                try SQLJSON.checkDepth(depth + 1)
                for member in members {
                    terms.append(jsonKey(member.key))
                    try walk(member.value, asKey: false, depth: depth + 1)
                }
            case .array(let elements):
                try SQLJSON.checkDepth(depth + 1)
                for element in elements { try walk(element, asKey: true, depth: depth + 1) }
            default: terms.append(try jsonScalar(value, asKey: asKey))
            }
        }
        try walk(document, asKey: true, depth: 0)
        return distinct(terms)
    }

    static func tagged(_ term: [UInt8], column: Int, columnCount: Int) throws -> [UInt8] {
        guard (1...32).contains(columnCount), column >= 0, column < columnCount else {
            throw JedError.corruption("invalid GIN column tag")
        }
        return columnCount == 1 ? term : [UInt8(column)] + term
    }
    static func successor(_ prefix: [UInt8]) -> [UInt8]? {
        var next = prefix
        while let last = next.popLast() {
            if last != 255 { next.append(last + 1); return next }
        }
        return nil
    }
}

internal struct GinOpclass: Sendable {
    let element: ScalarType?
    var isJSON: Bool { element == nil }

    init(type: TypeDescriptor) throws {
        switch type.type {
        case .scalar(.jsonb): element = nil
        case .array(.scalar(let element)):
            switch element {
            case .json, .jsonb, .jsonpath: throw JedError("0A000", "GIN array element type is not supported")
            default: self.element = element
            }
        case .array: throw JedError("0A000", "GIN array element type is not supported")
        default: throw JedError("42704", "type \(type.type.name) has no default GIN operator class")
        }
    }

    func elementTerm(_ value: SQLValue) throws -> [UInt8] {
        guard let element else { throw JedError("42804", "array term requires an array operator class") }
        switch (element, value) {
        case (.text, .text(let text)): return GinTerms.terminated(text.utf8.prefix(GinTerms.maximumContentBytes))
        case (.bytea, .bytea(let bytes)): return GinTerms.terminated(bytes.prefix(GinTerms.maximumContentBytes))
        case (.decimal, .decimal(let decimal)): return Array(decimal.keyBytes().prefix(GinTerms.maximumContentBytes))
        default: return try KeyEncoding.encode(value, type: TypeDescriptor(.scalar(element)))
        }
    }
    func indexTerms(_ value: SQLValue) throws -> [[UInt8]] {
        if value.isNull { return [] }
        if isJSON {
            guard case .jsonb(let document) = value else { throw JedError("42804", "GIN value does not match jsonb operator class") }
            return try GinTerms.jsonIndex(document)
        }
        guard case .array(let array) = value else { throw JedError("42804", "GIN value does not match array operator class") }
        return try GinTerms.distinct(array.elements.filter { !$0.isNull }.map(elementTerm))
    }

    /// A comparison operand may be a wider integer, decimal or float than the
    /// indexed element. An inexact conversion cannot equal any stored element.
    func queryTerm(_ value: SQLValue) throws -> [UInt8]? {
        guard let element, !value.isNull else { return nil }
        var converted = value
        if element.integerWidth != nil {
            let integer: Int64
            switch value {
            case .integer(let value): integer = value
            case .decimal(let value):
                let whole = try value.truncated()
                guard value.compare(to: whole) == 0, whole.compare(to: SQLDecimal(Int64.min)) >= 0,
                      whole.compare(to: SQLDecimal(Int64.max)) <= 0 else { return nil }
                integer = try whole.integerValue()
            default: return nil
            }
            guard element.admits(integer) else { return nil }
            converted = .integer(integer)
        } else if element == .decimal, case .integer(let integer) = value { converted = .decimal(SQLDecimal(integer)) }
        else if element == .f32, case .f64(let bits) = value {
            let wide = Double(bitPattern: bits), narrow = Float(wide)
            guard wide.isNaN || Double(narrow) == wide else { return nil }
            converted = .f32(narrow.bitPattern)
        } else if element == .f64, case .f32(let bits) = value { converted = .f64(Double(Float(bitPattern: bits)).bitPattern) }
        return try elementTerm(converted)
    }
}
