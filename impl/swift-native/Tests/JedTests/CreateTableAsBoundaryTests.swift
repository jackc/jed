import XCTest
@testable import Jed

final class CreateTableAsBoundaryTests: XCTestCase {
    func testBoundParametersAffectedCountAndFailedCreation() throws {
        let database = try Database(), session = try database.session()
        defer { session.close() }
        try session.execute("CREATE TABLE src (id i32 PRIMARY KEY, name text)")
        try session.execute("INSERT INTO src VALUES (1, 'a'), (2, 'b'), (3, 'c')")
        let tag = try session.execute("CREATE TABLE t AS SELECT id, name, $2::text AS tag FROM src WHERE id >= $1",
            parameters: [.integer(2), .text(SQLText("x"))])
        XCTAssertEqual(tag.rowsAffected, 2)
        let rows = try session.query("SELECT id, name, tag FROM t ORDER BY id")
        XCTAssertEqual(try rows.next()?.map { try ScalarCasts.render($0).description }, ["2", "b", "x"])
        XCTAssertEqual(try rows.next()?.map { try ScalarCasts.render($0).description }, ["3", "c", "x"])
        XCTAssertNil(try rows.next())
        XCTAssertThrowsError(try session.execute("CREATE TABLE u AS SELECT $1::i32 AS a")) {
            XCTAssertEqual(($0 as? JedError)?.code, "42601")
        }
        XCTAssertThrowsError(try session.query("SELECT a FROM u")) { XCTAssertEqual(($0 as? JedError)?.code, "42P01") }
        let empty = try session.execute("CREATE TABLE v AS SELECT $1::i32 / 0 AS a WITH NO DATA", parameters: [.integer(1)])
        XCTAssertEqual(empty.cost, 0)
        XCTAssertNil(try session.query("SELECT a FROM v").next())
        let into = try session.execute("SELECT id INTO x FROM src WHERE id <= $1", parameters: [.integer(2)])
        XCTAssertEqual(into.rowsAffected, 2)
        XCTAssertThrowsError(try session.execute("CREATE TABLE ordinary (a i32)", parameters: [.integer(1)])) {
            XCTAssertEqual(($0 as? JedError)?.code, "42601")
        }
        XCTAssertEqual(database.core.livePinCount, 0)
    }

    func testFillPrivilegeExceptionStaysOnNewTargetAndNoDataDoesNotExecute() throws {
        let database = try Database(), session = try database.session()
        defer { session.close() }
        try session.execute("CREATE TABLE src (id i32)")
        try session.execute("INSERT INTO src VALUES (1)")
        session.revoke(.insert, on: try SQLText("dest"))
        XCTAssertEqual(try session.execute("CREATE TABLE dest AS SELECT id FROM src").rowsAffected, 1)
        XCTAssertThrowsError(try session.execute("INSERT INTO dest VALUES (2)")) { XCTAssertEqual(($0 as? JedError)?.code, "42501") }
        session.revoke(.select, on: try SQLText("src"))
        XCTAssertThrowsError(try session.execute("CREATE TABLE denied AS SELECT id FROM src")) { XCTAssertEqual(($0 as? JedError)?.code, "42501") }
        session.revoke(.execute, on: try SQLText("abs"), function: true)
        XCTAssertNoThrow(try session.execute("CREATE TABLE empty AS SELECT abs(id) AS id FROM src WITH NO DATA"))
        XCTAssertNil(try session.query("SELECT id FROM empty").next())
    }
}
