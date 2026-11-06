/// Constant-size totals for a completed script. Result rows are discarded as
/// each ordinary statement cursor is drained.
public struct ScriptSummary: Sendable, Equatable {
    public let statementsRun: Int64
    public let rowsAffectedTotal: Int64
    public let cost: Int64
}

extension Database {
    /// Execute a script on a fresh session, atomically publishing its writes.
    @discardableResult public func executeScript(_ sql: String, cancellation: CancellationToken? = nil) throws -> ScriptSummary {
        let handle = try session(); defer { handle.close() }
        return try handle.executeScript(sql, cancellation: cancellation)
    }
}

extension Session {
    /// An idle session owns one implicit transaction for the whole script.
    /// An open session joins its caller's transaction and leaves it open.
    @discardableResult public func executeScript(_ sql: String, cancellation: CancellationToken? = nil) throws -> ScriptSummary {
        let ownsTransaction = try admitScript(cancellation: cancellation)
        do {
            let result = try runScript(sql, cancellation: cancellation)
            if ownsTransaction { try commit() }
            return result
        } catch {
            if ownsTransaction { try? rollback() }
            throw error
        }
    }

    private func runScript(_ sql: String, cancellation: CancellationToken?) throws -> ScriptSummary {
        var statements: Int64 = 0, affected: Int64 = 0, cost: Int64 = 0
        // Positions are Unicode scalar counts; StatementSpan offsets are bytes.
        // Advance once through the original text instead of recounting prefixes.
        let bytes = sql.utf8
        var cursor = bytes.startIndex, counted = 0, before = 0
        for span in splitStatements(sql) {
            while counted < span.offset {
                if bytes[cursor] & 0xc0 != 0x80 { before += 1 }
                cursor = bytes.index(after: cursor); counted += 1
            }
            do {
                let prepared = try prepare(span.text)
                switch prepared.statement {
                case .begin, .commit, .rollback, .savepoint, .release, .rollbackTo:
                    throw JedError("0A000", "transaction control is not supported inside executeScript; use splitStatements for a host-managed transaction")
                default: break
                }
                let rows = try queryPrepared(prepared, cancellation: cancellation)
                defer { rows.close() }
                while try rows.next() != nil {}
                statements = saturatingAdd(statements, 1)
                affected = saturatingAdd(affected, rows.rowsAffected ?? 0)
                cost = saturatingAdd(cost, rows.cost)
            } catch {
                transaction?.failed = true
                if var error = error as? JedError {
                    if let position = error.position { error.position = position + before }
                    throw error
                }
                throw error
            }
        }
        return ScriptSummary(statementsRun: statements, rowsAffectedTotal: affected, cost: cost)
    }
}
