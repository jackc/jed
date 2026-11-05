/// A SQLSTATE with optional structured diagnostics. Messages are informational;
/// callers should branch on `code` (spec/design/error-fields.md).
public struct JedError: Error, Sendable, CustomStringConvertible {
    public let code: String
    public let message: String
    public var constraintName: String?
    public var tableName: String?
    public var databaseName: String?
    public var columnName: String?
    public var dataTypeName: String?
    public var detail: String?
    public var position: Int?
    public var hint: String?
    internal var locationDecided = false
    internal var hintDecided = false
    internal var insertSlot: (row: Int, column: Int)?

    public init(_ code: String, _ message: String) {
        self.code = code
        self.message = message
    }

    public var description: String { "\(code): \(message)" }

    internal func at(_ position: Int) -> JedError {
        var error = self
        if !error.locationDecided {
            error.position = position > 0 ? position : nil
            error.locationDecided = true
        }
        return error
    }

    internal func withDataType(_ name: String) -> JedError {
        var error = self
        error.dataTypeName = name
        return error
    }

    internal static func corruption(_ message: String) -> JedError {
        JedError("XX001", message)
    }
}
