import Foundation

/// Convert a native parameter without changing SQL's context-inferred type.
/// Applications may conform their own value types to this protocol.
public protocol SQLValueConvertible {
    func sqlValue() throws -> SQLValue
}

/// Decode one result value. Optional is the nullable destination; SQLValue
/// itself is the lossless escape hatch for every supported SQL type.
public protocol SQLValueDecodable {
    static func fromSQLValue(_ value: SQLValue) throws -> Self
}

private func conversionError(_ value: SQLValue, _ target: String) -> JedError {
    value.isNull ? JedError("22004", "NULL cannot be read as \(target); use Optional")
        : JedError("42804", "SQL value does not match \(target)")
}

extension SQLValue: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() -> SQLValue { self }
    public static func fromSQLValue(_ value: SQLValue) -> SQLValue { value }
}

extension Optional: SQLValueConvertible where Wrapped: SQLValueConvertible {
    public func sqlValue() throws -> SQLValue {
        switch self { case .none: return .null; case .some(let value): return try value.sqlValue() }
    }
}
extension Optional: SQLValueDecodable where Wrapped: SQLValueDecodable {
    public static func fromSQLValue(_ value: SQLValue) throws -> Self {
        value.isNull ? nil : try Wrapped.fromSQLValue(value)
    }
}

public extension SQLValueConvertible where Self: FixedWidthInteger {
    func sqlValue() throws -> SQLValue {
        guard let value = Int64(exactly: self) else { throw JedError("22003", "native integer is outside i64 range") }
        return .integer(value)
    }
}
public extension SQLValueDecodable where Self: FixedWidthInteger {
    static func fromSQLValue(_ value: SQLValue) throws -> Self {
        guard case .integer(let number) = value else { throw conversionError(value, "integer") }
        guard let result = Self(exactly: number) else { throw JedError("22003", "integer is outside destination range") }
        return result
    }
}
extension Int: SQLValueConvertible, SQLValueDecodable {}
extension Int8: SQLValueConvertible, SQLValueDecodable {}
extension Int16: SQLValueConvertible, SQLValueDecodable {}
extension Int32: SQLValueConvertible, SQLValueDecodable {}
extension Int64: SQLValueConvertible, SQLValueDecodable {}
extension UInt: SQLValueConvertible, SQLValueDecodable {}
extension UInt8: SQLValueConvertible, SQLValueDecodable {}
extension UInt16: SQLValueConvertible, SQLValueDecodable {}
extension UInt32: SQLValueConvertible, SQLValueDecodable {}
extension UInt64: SQLValueConvertible, SQLValueDecodable {}

extension Bool: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() -> SQLValue { .boolean(self) }
    public static func fromSQLValue(_ value: SQLValue) throws -> Bool {
        guard case .boolean(let result) = value else { throw conversionError(value, "Bool") }; return result
    }
}
extension String: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() throws -> SQLValue { .text(try SQLText(self)) }
    public static func fromSQLValue(_ value: SQLValue) throws -> String { try SQLText.fromSQLValue(value).description }
}
extension SQLText: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() -> SQLValue { .text(self) }
    public static func fromSQLValue(_ value: SQLValue) throws -> SQLText {
        guard case .text(let result) = value else { throw conversionError(value, "text") }; return result
    }
}
extension Float: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() -> SQLValue { .f32(bitPattern) }
    public static func fromSQLValue(_ value: SQLValue) throws -> Float {
        switch value {
        case .f32(let bits): return Float(bitPattern: bits)
        case .f64(let bits): return Float(Double(bitPattern: bits))
        default: throw conversionError(value, "Float")
        }
    }
}
extension Double: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() -> SQLValue { .f64(bitPattern) }
    public static func fromSQLValue(_ value: SQLValue) throws -> Double {
        switch value {
        case .f64(let bits): return Double(bitPattern: bits)
        case .f32(let bits): return Double(Float(bitPattern: bits))
        case .integer(let value): return Double(value)
        default: throw conversionError(value, "Double")
        }
    }
}

extension Array: SQLValueConvertible where Element: SQLValueConvertible {
    /// A byte array is bytea; other native arrays bind as one SQL array value.
    public func sqlValue() throws -> SQLValue {
        if Element.self == UInt8.self, let bytes = self as? [UInt8] { return .bytea(bytes) }
        return .array(try SQLArray(elements: map { try $0.sqlValue() }))
    }
}
extension Array: SQLValueDecodable where Element: SQLValueDecodable {
    public static func fromSQLValue(_ value: SQLValue) throws -> [Element] {
        if Element.self == UInt8.self {
            let bytes: [UInt8]
            switch value {
            case .bytea(let result): bytes = result
            case .uuid(let result): bytes = result.bytes
            default: throw conversionError(value, "bytes")
            }
            guard let result = bytes as? [Element] else { throw conversionError(value, "bytes") }
            return result
        }
        guard case .array(let array) = value, array.dimensions.count <= 1 else {
            throw conversionError(value, "one-dimensional array")
        }
        return try array.elements.map(Element.fromSQLValue)
    }
}
extension Data: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() -> SQLValue { .bytea(Array(self)) }
    public static func fromSQLValue(_ value: SQLValue) throws -> Data { Data(try [UInt8].fromSQLValue(value)) }
}
extension SQLDecimal: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() -> SQLValue { .decimal(self) }
    public static func fromSQLValue(_ value: SQLValue) throws -> SQLDecimal {
        guard case .decimal(let result) = value else { throw conversionError(value, "SQLDecimal") }; return result
    }
}
extension SQLUUID: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() -> SQLValue { .uuid(self) }
    public static func fromSQLValue(_ value: SQLValue) throws -> SQLUUID {
        guard case .uuid(let result) = value else { throw conversionError(value, "SQLUUID") }; return result
    }
}
extension UUID: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() throws -> SQLValue {
        let v = uuid
        return .uuid(try SQLUUID(bytes: [v.0, v.1, v.2, v.3, v.4, v.5, v.6, v.7,
            v.8, v.9, v.10, v.11, v.12, v.13, v.14, v.15]))
    }
    public static func fromSQLValue(_ value: SQLValue) throws -> UUID {
        let b = try SQLUUID.fromSQLValue(value).bytes
        return UUID(uuid: (b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
            b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]))
    }
}
extension SQLInterval: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() -> SQLValue { .interval(self) }
    public static func fromSQLValue(_ value: SQLValue) throws -> SQLInterval {
        guard case .interval(let result) = value else { throw conversionError(value, "SQLInterval") }; return result
    }
}
extension SQLArray: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() -> SQLValue { .array(self) }
    public static func fromSQLValue(_ value: SQLValue) throws -> SQLArray {
        guard case .array(let result) = value else { throw conversionError(value, "SQLArray") }; return result
    }
}
extension SQLRange: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() -> SQLValue { .range(self) }
    public static func fromSQLValue(_ value: SQLValue) throws -> SQLRange {
        guard case .range(let result) = value else { throw conversionError(value, "SQLRange") }; return result
    }
}
extension SQLJSONValue: SQLValueConvertible, SQLValueDecodable {
    public func sqlValue() -> SQLValue { .jsonb(self) }
    public static func fromSQLValue(_ value: SQLValue) throws -> SQLJSONValue {
        guard case .jsonb(let result) = value else { throw conversionError(value, "SQLJSONValue") }; return result
    }
}
