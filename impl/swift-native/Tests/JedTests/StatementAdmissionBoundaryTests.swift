import XCTest
@testable import Jed

final class StatementAdmissionBoundaryTests: XCTestCase {
    func testPrivilegePreflightPrecedesTargetNamesAndTypeErrors() throws {
        let database = try Database(), session = try database.session()
        defer { session.close() }
        try session.execute("CREATE TABLE src (id i32)")
        session.revoke(.select, on: try SQLText("src"))
        for sql in ["CREATE TABLE jed_bad AS SELECT id FROM src",
                    "CREATE TABLE invalid AS SELECT absent FROM src",
                    "SELECT * FROM missing, src"] {
            XCTAssertThrowsError(try session.execute(sql)) { XCTAssertEqual(($0 as? JedError)?.code, "42501", sql) }
        }
        XCTAssertThrowsError(try session.execute("CREATE TABLE jed_bad AS SELECT id FROM src WITH NO DATA")) {
            XCTAssertEqual(($0 as? JedError)?.code, "42939")
        }
        XCTAssertNoThrow(try session.execute("WITH src AS (SELECT 1 AS id) SELECT id FROM src"))
    }

    func testBoundedJSONProductionFinishesBeforeQueryMemoryAdmission() throws {
        let database = try Database()
        var options = SessionOptions(); options.maxQueryMemoryBytes = 1
        let session = try database.session(options: options)
        defer { session.close() }
        // A tiny relation budget must not replace the second row's conversion
        // error with an early reservation failure on the successfully built row.
        for sql in [
            "SELECT * FROM JSON_TABLE('[1,2147483648]'::jsonb, '$[*]' COLUMNS (v i32 PATH '$' ERROR ON ERROR)) AS j",
            "SELECT * FROM jsonb_to_recordset('[{\"v\":1},{\"v\":2147483648}]'::jsonb) AS j(v i32)"
        ] {
            XCTAssertThrowsError(try session.execute(sql)) { XCTAssertEqual(($0 as? JedError)?.code, "22003", sql) }
        }
        XCTAssertEqual(database.core.livePinCount, 0)
    }
}
