import XCTest
@testable import Jed

final class TemporarySessionBoundaryTests: XCTestCase {
    private func integers(_ rows: Rows) throws -> [Int64] {
        defer { rows.close() }
        return try rows.collect().map {
            guard case .integer(let value) = $0[0] else { throw JedError("XX000", "expected integer result") }
            return value
        }
    }
    func testPrivateWritesDoNotWaitForMainWriterOrAdvanceMainVersion() throws {
        let database = try Database(), writer = try database.session()
        try writer.begin()
        let version = database.version
        var options = SessionOptions(); options.lockTimeoutMilliseconds = 10
        let reader = try database.readSession(options: options)
        try reader.execute("CREATE TEMP TABLE scratch (value i32)")
        try reader.execute("INSERT INTO scratch VALUES (7)")
        XCTAssertEqual(try integers(reader.query("SELECT value FROM scratch")), [7])
        XCTAssertEqual(database.version, version)
        try writer.rollback()
        XCTAssertThrowsError(try writer.query("SELECT value FROM scratch")) {
            XCTAssertEqual(($0 as? JedError)?.code, "42P01")
        }
        reader.close(); writer.close(); try database.close()
    }
    func testPrivateCursorOutlivesSessionAndKeepsItsOriginalRoot() throws {
        let database = try Database(), session = try database.session()
        try session.execute("CREATE TEMP TABLE scratch (value i32)")
        try session.execute("INSERT INTO scratch VALUES (1), (2)")
        let retained = try session.query("SELECT value FROM scratch ORDER BY value")
        try session.execute("DELETE FROM scratch")
        session.close()
        XCTAssertEqual(try integers(retained), [1, 2])
        try database.close()
    }
    func testReadOnlyBlockDiscardsPrivateChangesButAutocommitKeepsThem() throws {
        let database = try Database(), session = try database.readSession()
        try session.execute("CREATE TEMP TABLE scratch (value i32)")
        try session.execute("INSERT INTO scratch VALUES (1)")
        try session.begin(writable: false)
        try session.execute("INSERT INTO scratch VALUES (2)")
        XCTAssertEqual(try integers(session.query("SELECT value FROM scratch ORDER BY value")), [1, 2])
        try session.commit()
        XCTAssertEqual(try integers(session.query("SELECT value FROM scratch")), [1])
        session.close(); try database.close()
    }
}
