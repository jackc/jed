import XCTest
@testable import Jed

final class ViewBoundaryTests: XCTestCase {
    func testRenameRewritesOnlyBoundRelationTokensAndPinsAliasPositions() throws {
        let table = try SQLText("t"), a = try SQLText("a"), b = try SQLText("b")
        let view = CatalogView(name: try SQLText("v"), columns: [.init(name: b)],
            definition: try SQLText("select t.b from t"), dependencies: [.init(kind: 0, name: table, columns: [b])])
        var image = CatalogImage(); image.views = [view]
        try ViewRewrite.references(in: &image, name: table, kind: 0, column: (1, try SQLText("new")), columns: [a, b])
        XCTAssertEqual(image.views[0].definition.description, "select t . b from t as \"t\" ( \"a\" , \"b\" )")
        XCTAssertEqual(image.views[0].dependencies[0].columns, [try SQLText("new")])
        try ViewRewrite.references(in: &image, name: table, kind: 0, renamed: SQLText("renamed"), columns: [a, b])
        XCTAssertEqual(image.views[0].definition.description, "select t . b from \"renamed\" as \"t\" ( \"a\" , \"b\" )")

        image.views[0] = CatalogView(name: try SQLText("v"), columns: [.init(name: b)],
            definition: try SQLText("with t as (select b from main.t) select b from t"),
            dependencies: [.init(kind: 0, name: table, columns: [b])])
        try ViewRewrite.references(in: &image, name: table, kind: 0, renamed: SQLText("renamed"), columns: [a, b])
        XCTAssertEqual(image.views[0].definition.description, "with t as ( select b from main . \"renamed\" as \"t\" ) select b from t")
    }
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
