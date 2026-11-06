import XCTest
@testable import Jed

/// The SQL corpus cannot invoke the script wrapper or observe its transaction
/// ownership, summary, error coordinates, and cursor cleanup.
final class ScriptExecutionBoundaryTests: XCTestCase {
    private func fails(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) {
            XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line)
        }
    }
    private func count(_ database: Database) throws -> Int64 {
        let rows = try database.query("SELECT count(*) FROM t"); defer { rows.close() }
        guard let row = try rows.next(), case .integer(let count) = row[0] else {
            throw JedError("XX000", "missing script test count")
        }
        return count
    }
    func testSummaryPublishesOnceAndDiscardsResults() throws {
        let database = try Database(); defer { try? database.close() }
        let session = try database.session(); defer { session.close() }
        let version = database.version, accrued = session.lifetimeCost
        let summary = try session.executeScript("""
            CREATE TABLE t (id i32 PRIMARY KEY);
            INSERT INTO t VALUES (1), (2), (3);
            SELECT * FROM t;
            UPDATE t SET id = id + 10 WHERE id = 1;
            DELETE FROM t WHERE id = 2 RETURNING id;
            """)
        XCTAssertEqual(summary.statementsRun, 5)
        XCTAssertEqual(summary.rowsAffectedTotal, 4) // RETURNING has no command count.
        XCTAssertGreaterThan(summary.cost, 0)
        XCTAssertEqual(summary.cost, session.lifetimeCost - accrued)
        XCTAssertEqual(session.status, .idle)
        XCTAssertEqual(database.version, version + 1)
        XCTAssertEqual(try count(database), 2)
        XCTAssertEqual(database.core.livePinCount, 0)
        XCTAssertFalse(database.core.hasLocalWriter)
    }
    func testFailureRollsBackOnlyOwnedTransaction() throws {
        let database = try Database(); defer { try? database.close() }
        try database.execute("CREATE TABLE t (id i32 PRIMARY KEY)")
        let session = try database.session(); defer { session.close() }
        let script = "INSERT INTO t VALUES (1); INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)"
        fails("23505") { try session.executeScript(script) }
        XCTAssertEqual(session.status, .idle)
        XCTAssertEqual(try count(database), 0)
        try session.begin()
        fails("23505") { try session.executeScript(script) }
        XCTAssertEqual(session.status, .failed)
        XCTAssertTrue(database.core.hasLocalWriter)
        fails("25P02") { try session.execute("SELECT 1") }
        try session.rollback()
        XCTAssertEqual(try count(database), 0)
        XCTAssertEqual(database.core.livePinCount, 0)
    }
    func testOpenAndScopedTransactionsKeepTheirOwner() throws {
        let database = try Database(); defer { try? database.close() }
        try database.execute("CREATE TABLE t (id i32 PRIMARY KEY)")
        let session = try database.session(); defer { session.close() }
        try session.begin()
        let summary = try session.executeScript("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)")
        XCTAssertEqual(summary.rowsAffectedTotal, 2)
        XCTAssertEqual(session.status, .open)
        XCTAssertEqual(try count(database), 0)
        try session.rollback()
        try session.update { handle in
            try handle.executeScript("INSERT INTO t VALUES (3)")
            XCTAssertEqual(handle.status, .open)
            fails("2D000") { try handle.commit() }
        }
        XCTAssertEqual(try count(database), 1)
    }
    func testControlStatementsAreRejectedAndPoisonCallerBlock() throws {
        let database = try Database(); defer { try? database.close() }
        try database.execute("CREATE TABLE t (id i32 PRIMARY KEY)")
        let session = try database.session(); defer { session.close() }
        for control in ["BEGIN", "START TRANSACTION", "COMMIT", "ROLLBACK", "SAVEPOINT s", "RELEASE s", "ROLLBACK TO s"] {
            fails("0A000") { try session.executeScript("INSERT INTO t VALUES (1); " + control) }
            XCTAssertEqual(session.status, .idle)
            XCTAssertEqual(try count(database), 0)
        }
        try session.begin()
        fails("0A000") { try session.executeScript("COMMIT") }
        XCTAssertEqual(session.status, .failed)
        try session.rollback()
    }
    func testScalarErrorCoordinatesAndStatementLengthLimits() throws {
        let database = try Database(); defer { try? database.close() }
        let session = try database.session(); defer { session.close() }
        var local: JedError?
        do { try session.execute("SELECT missing") } catch { local = error as? JedError }
        XCTAssertNotNil(local?.position)
        let prefix = "SELECT '😀é';\n  "
        do {
            try session.executeScript(prefix + "SELECT missing")
            XCTFail("script should fail")
        } catch let error as JedError {
            XCTAssertEqual(error.code, local?.code)
            XCTAssertEqual(error.position, local?.position.map { $0 + prefix.unicodeScalars.count })
        }
        var options = SessionOptions(); options.maxSQLLength = 10
        let limited = try database.session(options: options); defer { limited.close() }
        XCTAssertEqual(try limited.executeScript("SELECT 1; SELECT 2; SELECT 3").statementsRun, 3)
        fails("54000") { try limited.executeScript("SELECT 1; SELECT 12345") }
        XCTAssertEqual(limited.status, .idle)
    }
    func testEmptyConvenienceAndReadSessionDoNotRetainWriter() throws {
        let database = try Database(); defer { try? database.close() }
        let version = database.version
        XCTAssertEqual(try database.executeScript(" ; -- empty\n /* empty */ ; "),
                       ScriptSummary(statementsRun: 0, rowsAffectedTotal: 0, cost: 0))
        XCTAssertEqual(database.version, version)
        let reader = try database.readSession(); defer { reader.close() }
        let writer = try database.writeSession(); defer { writer.close() }
        XCTAssertEqual(try reader.executeScript("SELECT 1; SELECT 2").statementsRun, 2)
        XCTAssertEqual(reader.status, .idle)
        fails("25006") { try reader.executeScript("CREATE TABLE forbidden (id i32)") }
        XCTAssertEqual(reader.status, .idle)
        XCTAssertTrue(database.core.hasLocalWriter)
    }
    func testCanceledScriptRollsBackAndRestoresSession() throws {
        let database = try Database(); defer { try? database.close() }
        let session = try database.session(); defer { session.close() }
        let cancellation = CancellationToken(); cancellation.cancel()
        fails("57014") { try session.executeScript("SELECT 1", cancellation: cancellation) }
        XCTAssertEqual(session.status, .idle)
        XCTAssertEqual(try session.executeScript("SELECT 1").statementsRun, 1)
        XCTAssertFalse(database.core.hasLocalWriter)
    }
}
