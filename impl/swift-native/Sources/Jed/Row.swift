/// An owned result row. Values and column names retain exact SQL byte identity;
/// conversions happen only when the caller asks for a native destination.
public struct Row: Sendable {
    public let columns: [ResultColumn]
    public let values: [SQLValue]
    public var count: Int { values.count }

    internal init(columns: [ResultColumn], values: [SQLValue]) {
        self.columns = columns; self.values = values
    }
    public func value(_ index: Int) throws -> SQLValue {
        guard values.indices.contains(index) else { throw JedError("42703", "result column index is out of range") }
        return values[index]
    }
    public func value(_ name: String) throws -> SQLValue {
        let bytes = Array(name.utf8)
        guard let index = columns.firstIndex(where: { $0.name.utf8 == bytes }) else {
            throw JedError("42703", "result column does not exist: \(name)")
        }
        return try value(index)
    }
    public func get<T: SQLValueDecodable>(_ index: Int, as type: T.Type = T.self) throws -> T {
        try T.fromSQLValue(value(index))
    }
    public func get<T: SQLValueDecodable>(_ name: String, as type: T.Type = T.self) throws -> T {
        try T.fromSQLValue(value(name))
    }
}

extension Rows {
    public func nextRow() throws -> Row? {
        try next().map { Row(columns: columns, values: $0) }
    }
    public func collectRows() throws -> [Row] {
        try collect().map { Row(columns: columns, values: $0) }
    }
    /// Returning false stops iteration. Exit, a thrown host error, exhaustion
    /// and a SQL error all close this cursor and release its reader pin.
    public func forEachRow(_ body: (Row) throws -> Bool) throws {
        defer { close() }
        while let row = try nextRow() { if try !body(row) { return } }
    }
}
