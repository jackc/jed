/// A common handle for data-access helpers. Explicit transactions execute on
/// their owning Session; Database convenience calls create a fresh session.
public protocol SQLQuerying {
    func query(_ sql: String, parameters: [SQLValue], cancellation: CancellationToken?) throws -> Rows
    func query(_ sql: String, arguments: [any SQLValueConvertible], cancellation: CancellationToken?) throws -> Rows
    func queryPrepared(_ statement: PreparedStatement, parameters: [SQLValue], cancellation: CancellationToken?) throws -> Rows
    func queryPrepared(_ statement: PreparedStatement, arguments: [any SQLValueConvertible], cancellation: CancellationToken?) throws -> Rows
}

extension Session: SQLQuerying {
    public func query(_ sql: String, arguments: [any SQLValueConvertible], cancellation: CancellationToken? = nil) throws -> Rows {
        try query(sql, parameters: convert(arguments), cancellation: cancellation)
    }
    public func queryPrepared(_ statement: PreparedStatement, arguments: [any SQLValueConvertible],
                              cancellation: CancellationToken? = nil) throws -> Rows {
        try queryPrepared(statement, parameters: convert(arguments), cancellation: cancellation)
    }
    private func convert(_ arguments: [any SQLValueConvertible]) throws -> [SQLValue] {
        try checkOpen()
        do { return try arguments.map { try $0.sqlValue() } }
        catch { transaction?.failed = true; throw error }
    }
}

extension Database: SQLQuerying {
    public func query(_ sql: String, arguments: [any SQLValueConvertible], cancellation: CancellationToken? = nil) throws -> Rows {
        try session().query(sql, arguments: arguments, cancellation: cancellation)
    }
    public func queryPrepared(_ statement: PreparedStatement, arguments: [any SQLValueConvertible],
                              cancellation: CancellationToken? = nil) throws -> Rows {
        try session().queryPrepared(statement, arguments: arguments, cancellation: cancellation)
    }
    public func view<T>(_ body: (Session) throws -> T) throws -> T {
        let handle = try session(); defer { handle.close() }; return try handle.view(body)
    }
    public func update<T>(_ body: (Session) throws -> T) throws -> T {
        let handle = try session(); defer { handle.close() }; return try handle.update(body)
    }
}

public extension SQLQuerying {
    @discardableResult func execute(_ sql: String, arguments: [any SQLValueConvertible],
                                    cancellation: CancellationToken? = nil) throws -> CommandTag {
        try query(sql, arguments: arguments, cancellation: cancellation).finish()
    }
    @discardableResult func executePrepared(_ statement: PreparedStatement, arguments: [any SQLValueConvertible],
                                            cancellation: CancellationToken? = nil) throws -> CommandTag {
        try queryPrepared(statement, arguments: arguments, cancellation: cancellation).finish()
    }
    func queryRow(_ sql: String, parameters: [SQLValue] = [], cancellation: CancellationToken? = nil) throws -> Row? {
        let rows = try query(sql, parameters: parameters, cancellation: cancellation)
        defer { rows.close() }; return try rows.nextRow()
    }
    func queryRow(_ sql: String, arguments: [any SQLValueConvertible], cancellation: CancellationToken? = nil) throws -> Row? {
        let rows = try query(sql, arguments: arguments, cancellation: cancellation)
        defer { rows.close() }; return try rows.nextRow()
    }
    func queryRowPrepared(_ statement: PreparedStatement, parameters: [SQLValue] = [],
                          cancellation: CancellationToken? = nil) throws -> Row? {
        let rows = try queryPrepared(statement, parameters: parameters, cancellation: cancellation)
        defer { rows.close() }; return try rows.nextRow()
    }
    func queryRowPrepared(_ statement: PreparedStatement, arguments: [any SQLValueConvertible],
                          cancellation: CancellationToken? = nil) throws -> Row? {
        let rows = try queryPrepared(statement, arguments: arguments, cancellation: cancellation)
        defer { rows.close() }; return try rows.nextRow()
    }
    func queryRows(_ sql: String, parameters: [SQLValue] = [], cancellation: CancellationToken? = nil) throws -> [Row] {
        let rows = try query(sql, parameters: parameters, cancellation: cancellation)
        defer { rows.close() }; return try rows.collectRows()
    }
    func queryRows(_ sql: String, arguments: [any SQLValueConvertible], cancellation: CancellationToken? = nil) throws -> [Row] {
        let rows = try query(sql, arguments: arguments, cancellation: cancellation)
        defer { rows.close() }; return try rows.collectRows()
    }
    func queryRowsPrepared(_ statement: PreparedStatement, parameters: [SQLValue] = [],
                           cancellation: CancellationToken? = nil) throws -> [Row] {
        let rows = try queryPrepared(statement, parameters: parameters, cancellation: cancellation)
        defer { rows.close() }; return try rows.collectRows()
    }
    func queryRowsPrepared(_ statement: PreparedStatement, arguments: [any SQLValueConvertible],
                           cancellation: CancellationToken? = nil) throws -> [Row] {
        let rows = try queryPrepared(statement, arguments: arguments, cancellation: cancellation)
        defer { rows.close() }; return try rows.collectRows()
    }
    func queryMap<T>(_ sql: String, parameters: [SQLValue] = [], cancellation: CancellationToken? = nil,
                     _ transform: (Row) throws -> T) throws -> [T] {
        try queryRows(sql, parameters: parameters, cancellation: cancellation).map(transform)
    }
    func queryMap<T>(_ sql: String, arguments: [any SQLValueConvertible], cancellation: CancellationToken? = nil,
                     _ transform: (Row) throws -> T) throws -> [T] {
        try queryRows(sql, arguments: arguments, cancellation: cancellation).map(transform)
    }
}
