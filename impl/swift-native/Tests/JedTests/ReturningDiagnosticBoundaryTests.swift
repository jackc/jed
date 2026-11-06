import XCTest
@testable import Jed

final class ReturningDiagnosticBoundaryTests: XCTestCase {
    func testFuzzyHintOrderUsesTargetBeforeRowVersionAliases() throws {
        let database = try Database(); defer { try? database.close() }
        let setup = try database.query("CREATE TABLE t (id i32)"); setup.close()
        for (label, other) in [("new", "old"), ("old", "new")] {
            for expression in ["idd", "(SELECT idd)"] {
                let sql = "INSERT INTO t AS \(label) VALUES (1) RETURNING \(expression)"
                var caught: JedError?
                do {
                    let rows = try database.query(sql); defer { rows.close() }
                    while try rows.next() != nil {}
                } catch let error as JedError { caught = error }
                let error = try XCTUnwrap(caught, sql)
                XCTAssertEqual(error.code, "42703", sql)
                XCTAssertEqual(error.hint, "Perhaps you meant to reference the column \"\(label).id\" or the column \"\(other).id\".", sql)
            }
        }
    }
}
