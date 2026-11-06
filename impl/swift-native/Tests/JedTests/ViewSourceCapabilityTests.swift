import XCTest
@testable import Jed

/// Host capability checks complement the shared SQL/dependency regression.
final class ViewSourceCapabilityTests: XCTestCase {
    func testFromProducerAndDefaultFunctionDenialsRemainSessionScoped() throws {
        let database = try Database(), session = try database.session()
        defer { session.close() }
        let cases = [
            ("series_view", "generate_series", "SELECT * FROM generate_series(1, 2) AS g"),
            ("argument_view", "abs", "SELECT * FROM generate_series(1, abs(-2)) AS g"),
            ("json_view", "abs", "SELECT * FROM JSON_TABLE('[{}]', '$[*]' COLUMNS (x int DEFAULT abs(-3) ON EMPTY)) AS jt")
        ]
        for (name, function, body) in cases {
            try session.execute("CREATE VIEW \(name) AS \(body)")
            let reader = try database.session()
            defer { reader.close() }
            reader.revoke(.execute, on: try SQLText(function), function: true)
            XCTAssertThrowsError(try reader.execute("SELECT * FROM \(name)")) {
                XCTAssertEqual(($0 as? JedError)?.code, "42501")
            }
            XCTAssertThrowsError(try reader.execute("CREATE VIEW denied_\(name) AS \(body)")) {
                XCTAssertEqual(($0 as? JedError)?.code, "42501")
            }
            XCTAssertNoThrow(try session.execute("SELECT * FROM \(name)"))
        }
    }
}
