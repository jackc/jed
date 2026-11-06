import XCTest
@testable import Jed

/// Session setters and concurrent cursor ownership cannot be expressed by one
/// sequential SQL record. SQL kernel thresholds remain in the shared corpus.
final class FrozenResourceBoundaryTests: XCTestCase {
    private func fixture() throws -> Database {
        let database = try Database()
        try database.execute("CREATE TABLE t(id i32 PRIMARY KEY)")
        try database.execute("INSERT INTO t VALUES (1),(2),(3)")
        return database
    }
    private func fails<T>(_ code: String, _ operation: () throws -> T) {
        XCTAssertThrowsError(try operation()) { XCTAssertEqual(($0 as? JedError)?.code, code) }
    }

    func testScalarAllowanceIsFrozenAndOtherStatementsCannotResetAnOpenCursor() throws {
        let database = try fixture(); defer { try? database.close() }
        let session = try database.session(); defer { session.close() }
        session.maxScalarBytes = 8
        let older = try session.query("SELECT repeat('x',4) FROM t ORDER BY id")
        defer { older.close() }
        XCTAssertNotNil(try older.next())
        session.maxScalarBytes = 1
        let denied = try session.query("SELECT repeat('x',4) FROM t ORDER BY id LIMIT 1")
        fails("54P04") { try denied.next() }
        XCTAssertTrue(denied.isClosed)
        session.maxScalarBytes = 8
        let newer = try session.query("SELECT repeat('x',4) FROM t ORDER BY id LIMIT 2")
        XCTAssertEqual(try newer.collect().count, 2)
        XCTAssertNotNil(try older.next(), "the captured allowance ignores the later lower limit")
        fails("54P04") { try older.next() } // Opening/draining newer did not reset older's cumulative eight bytes.
        XCTAssertTrue(older.isClosed)
        XCTAssertEqual(session.status, .idle)
        session.maxScalarBytes = 0
        XCTAssertEqual(session.maxScalarBytes, GeneratedSpec.defaultScalarBytes)
        session.maxScalarBytes = -1
        XCTAssertEqual(session.maxScalarBytes, GeneratedSpec.defaultScalarBytes)
        XCTAssertEqual(database.core.livePinCount, 0)
    }

    func testCollectorUsesItsFrozenQueryBudgetAfterOtherCursorsOpen() throws {
        let database = try fixture(); defer { try? database.close() }
        let session = try database.session(); defer { session.close() }
        let rowBytes = MemorySizes.row([.integer(1)])
        session.maxQueryMemoryBytes = rowBytes
        let older = try session.query("SELECT id FROM t ORDER BY id LIMIT 1")
        defer { older.close() }
        session.maxQueryMemoryBytes = 1
        let denied = try session.query("SELECT id FROM t ORDER BY id LIMIT 1")
        fails("54P05") { try denied.collect() }
        XCTAssertTrue(denied.isClosed)
        XCTAssertEqual(try older.collect().count, 1)
        XCTAssertEqual(session.maxQueryMemoryBytes, 1)
        session.maxQueryMemoryBytes = -1
        XCTAssertEqual(session.maxQueryMemoryBytes, 0)
        let unlimited = try session.query("SELECT id FROM t ORDER BY id")
        XCTAssertEqual(try unlimited.collect().count, 3)
        XCTAssertEqual(database.core.livePinCount, 0)
    }
}
