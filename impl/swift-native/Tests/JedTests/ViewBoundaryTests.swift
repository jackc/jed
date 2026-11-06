import XCTest
@testable import Jed

final class ViewBoundaryTests: XCTestCase {
    func testExpansionCountsPathsAndResetsForIndependentReferences() throws {
        let state = ViewExpansionState(), outer = try SQLText("outer"), child = try SQLText("child")
        for _ in 0..<2 {
            try state.withBody(outer, referenceDepth: 0, offset: 0, bodyDepth: 2) {
                for offset in 0..<1023 {
                    try state.withBody(child, referenceDepth: 1, offset: offset, bodyDepth: 2) {}
                }
                // Planning the same body again is not another textual expansion.
                for _ in 0..<2000 { try state.withBody(child, referenceDepth: 1, offset: 0, bodyDepth: 2) {} }
                XCTAssertThrowsError(try state.withBody(child, referenceDepth: 1, offset: 1023, bodyDepth: 2) {}) {
                    XCTAssertEqual(($0 as? JedError)?.code, "54001")
                }
            }
        }
    }
    func testTextualDepthBoundaryAndUnwind() throws {
        let state = ViewExpansionState(), outer = try SQLText("outer"), child = try SQLText("child")
        try state.withBody(outer, referenceDepth: 254, offset: 0, bodyDepth: 2) {
            XCTAssertThrowsError(try state.withBody(child, referenceDepth: 1, offset: 0, bodyDepth: 2) {}) {
                XCTAssertEqual(($0 as? JedError)?.code, "54001")
            }
        }
        try state.withBody(outer, referenceDepth: 0, offset: 0, bodyDepth: 2) {}
        let parser = try SQLParser("select a from t")
        let query = try parser.nestedQuery()
        guard case .select(let select) = query.body else { return XCTFail("expected parsed SELECT") }
        XCTAssertEqual(select.from.first?.parserDepth, 1)
        XCTAssertEqual(parser.maximumDepth, 2)
    }
    func testDefinerTableCapabilityDoesNotEraseFunctionDenial() throws {
        let table = try SQLText("hidden"), function = try SQLText("secret")
        var capabilities = SessionPrivileges(defaults: [])
        capabilities.revoke(.select, on: table)
        capabilities.revoke(.execute, on: function, function: true)
        let body = capabilities.withTableAccess()
        XCTAssertNoThrow(try body.require(.select, on: table))
        XCTAssertThrowsError(try body.require(.execute, on: function, function: true)) {
            XCTAssertEqual(($0 as? JedError)?.code, "42501")
        }
        XCTAssertThrowsError(try capabilities.require(.select, on: table))
    }
}
