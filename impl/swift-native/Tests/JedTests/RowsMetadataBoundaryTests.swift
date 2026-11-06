import XCTest
import Jed

/// The shared SQL corpus checks values and costs. This tests the optional host
/// metadata that distinguishes a command count from a query's returned rows.
final class RowsMetadataBoundaryTests: XCTestCase {
    private func check(_ session: Session, _ sql: String, affected: Int64?, returned: Int64 = 0,
                       file: StaticString = #filePath, line: UInt = #line) throws {
        let rows = try session.query(sql)
        defer { rows.close() }
        XCTAssertEqual(rows.rowsAffected, affected, sql, file: file, line: line)
        var count: Int64 = 0
        while try rows.next() != nil { count += 1 }
        XCTAssertEqual(count, returned, sql, file: file, line: line)
        XCTAssertEqual(rows.rowsAffected, affected, sql, file: file, line: line)
        XCTAssertEqual(rows.commandTag.rowsAffected, affected ?? count, sql, file: file, line: line)
        rows.close()
        XCTAssertEqual(rows.rowsAffected, affected, sql, file: file, line: line)
    }

    func testStatementCountPresenceIncludingZeroAndTransactionControl() throws {
        let database = try Database(), session = try database.session()
        defer { session.close(); try? database.close() }
        try check(session, "CREATE TABLE t (id i32 PRIMARY KEY, value i32)", affected: nil)
        try check(session, "INSERT INTO t VALUES (1, 10), (2, 20)", affected: 2)
        try check(session, "UPDATE t SET value = 30 WHERE id = 99", affected: 0)
        try check(session, "UPDATE t SET value = value + 1 WHERE id = 1", affected: 1)
        try check(session, "SELECT value FROM t", affected: nil, returned: 2)
        try check(session, "ANALYZE t", affected: 0)
        try check(session, "DELETE FROM t WHERE id = 99", affected: 0)
        try check(session, "BEGIN", affected: nil)
        try check(session, "SAVEPOINT s", affected: nil)
        try check(session, "DELETE FROM t", affected: 2)
        try check(session, "ROLLBACK TO s", affected: nil)
        try check(session, "RELEASE s", affected: nil)
        try check(session, "COMMIT", affected: nil)
        try check(session, "BEGIN", affected: nil)
        try check(session, "ROLLBACK", affected: nil)
        try check(session, "DROP TABLE t", affected: nil)
    }

    func testReturningAndWritableWithRemainQueryResults() throws {
        let database = try Database(), session = try database.session()
        defer { session.close(); try? database.close() }
        try session.execute("CREATE TABLE t (id i32 PRIMARY KEY, value i32)")
        try check(session, "INSERT INTO t VALUES (1, 10) RETURNING id", affected: nil, returned: 1)
        try check(session, "UPDATE t SET value = 11 RETURNING value", affected: nil, returned: 1)
        try check(session, "DELETE FROM t WHERE id = 99 RETURNING id", affected: nil)
        try check(session, "INSERT INTO t VALUES (1, 20) ON CONFLICT DO NOTHING", affected: 0)
        try check(session, "INSERT INTO t VALUES (1, 20) ON CONFLICT DO NOTHING RETURNING id", affected: nil)
        try check(session, "INSERT INTO t VALUES (1, 20) ON CONFLICT (id) DO UPDATE SET value = excluded.value RETURNING id", affected: nil, returned: 1)
        try check(session, "WITH changed AS (UPDATE t SET value = 21 RETURNING id) SELECT id FROM changed", affected: nil, returned: 1)
        try check(session, "WITH input AS (SELECT 2 AS id) INSERT INTO t SELECT id, 30 FROM input", affected: 1)
        try check(session, "MERGE INTO t USING (VALUES (2, 31)) AS s(id, value) ON t.id = s.id WHEN MATCHED THEN UPDATE SET value = s.value", affected: 1)
        try check(session, "MERGE INTO t USING (VALUES (2, 32)) AS s(id, value) ON t.id = s.id WHEN MATCHED THEN UPDATE SET value = s.value RETURNING t.id", affected: nil, returned: 1)
        try check(session, "DELETE FROM t RETURNING id", affected: nil, returned: 2)
    }

    func testCreateTableAsDistinguishesFillFromNoDataAndSkip() throws {
        let database = try Database(), session = try database.session()
        defer { session.close(); try? database.close() }
        try check(session, "CREATE TABLE populated AS SELECT 1 AS id", affected: 1)
        try check(session, "CREATE TABLE empty_fill AS SELECT 1 AS id WHERE false", affected: 0)
        try check(session, "CREATE TABLE unfilled AS SELECT 1 AS id WITH NO DATA", affected: nil)
        try check(session, "CREATE TABLE IF NOT EXISTS populated AS SELECT 2 AS id", affected: nil)
        try session.execute("CREATE TABLE source (id i32 PRIMARY KEY)")
        try check(session, "CREATE TABLE written AS WITH changed AS (INSERT INTO source VALUES (1) RETURNING id) SELECT id FROM changed", affected: 1)
        let explained = try session.query("EXPLAIN INSERT INTO source VALUES (2)")
        XCTAssertNil(explained.rowsAffected)
        _ = try explained.finish()
        XCTAssertNil(explained.rowsAffected)
        let rows = try session.query("EXPLAIN ANALYZE UPDATE source SET id = 1")
        defer { rows.close() }
        XCTAssertNil(rows.rowsAffected)
        _ = try rows.finish()
        XCTAssertNil(rows.rowsAffected)
    }

    func testMetadataSurvivesEarlyCloseAndTerminalQueryError() throws {
        let database = try Database(), session = try database.session()
        defer { session.close(); try? database.close() }
        try session.execute("CREATE TABLE t (id i32 PRIMARY KEY)")
        let inserted = try session.query("INSERT INTO t VALUES (1), (2)")
        XCTAssertEqual(inserted.rowsAffected, 2)
        inserted.close()
        XCTAssertEqual(inserted.rowsAffected, 2)
        let returning = try session.query("DELETE FROM t RETURNING id")
        XCTAssertNil(returning.rowsAffected)
        XCTAssertNotNil(try returning.next())
        returning.close()
        XCTAssertNil(returning.rowsAffected)
        let failure = try session.query("SELECT 1 / 0")
        XCTAssertNil(failure.rowsAffected)
        XCTAssertThrowsError(try failure.next())
        XCTAssertTrue(failure.isClosed)
        XCTAssertNil(failure.rowsAffected)
    }
}
