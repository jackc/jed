import XCTest
@testable import Jed

/// Source identity, retained unresolved syntax, and parser stack limits are
/// internal boundaries; SQL results/errors remain in the shared corpus.
final class ParserBoundaryTests: XCTestCase {
    func testExpressionSpansDistinguishBytesFromScalarPositions() throws {
        let sql = "SELECT \"e\u{301}\" + 7"
        guard case .query(let query) = try SQLParser(sql).parse(),
              case .select(let select) = query.body,
              let expression = select.items.first?.expression,
              case .binary("+", let column, let number) = expression.kind else {
            return XCTFail("expected unresolved expression")
        }
        XCTAssertEqual(expression.position, 13)
        XCTAssertEqual(expression.span.byteStart, 7)
        XCTAssertEqual(expression.span.byteEnd, sql.utf8.count)
        XCTAssertEqual(column.position, 8)
        XCTAssertEqual(number.position, 15)
        guard case .column(_, let name) = column.kind else { return XCTFail("expected identifier") }
        XCTAssertEqual(name.utf8, Array("e\u{301}".utf8))
        XCTAssertNotEqual(name, try SQLText("é"))
    }

    func testStoredSyntaxSuppressesHostLocations() throws {
        let parser = try SQLParser("one + 2", recordPositions: false)
        let expression = try parser.parseExpressionOnly()
        XCTAssertEqual(expression.position, 0)
        XCTAssertEqual(expression.span.position, 0)
        XCTAssertEqual(expression.span.byteEnd, 7)
        XCTAssertThrowsError(try SQLParser("one +", recordPositions: false).parseExpressionOnly()) {
            XCTAssertNil(($0 as? JedError)?.position)
        }
    }

    func testRawLiteralSpellingAndCastBoundaryRemainUnresolved() throws {
        let literal = try SQLParser("-0x_8000_0000_0000_0000").parseExpressionOnly()
        guard case .number(let spelling) = literal.kind else { return XCTFail("expected signed literal") }
        XCTAssertEqual(spelling, "-0x_8000_0000_0000_0000")
        let cast = try SQLParser("-9_223_372_036_854_775_808::i64").parseExpressionOnly()
        guard case .unary("-", let operand) = cast.kind,
              case .cast(let raw, let type) = operand.kind,
              case .number(let magnitude) = raw.kind else { return XCTFail("expected unary over cast") }
        XCTAssertEqual(magnitude, "9_223_372_036_854_775_808")
        XCTAssertEqual(type.name, try SQLText("i64"))
    }

    func testNestingGateIncludesParenthesesAndLeftDeepChains() throws {
        let maximum = SQLParser.maximumNesting
        let valid = String(repeating: "(", count: maximum - 1) + "1" + String(repeating: ")", count: maximum - 1)
        let parser = try SQLParser(valid)
        _ = try parser.parseExpressionOnly()
        XCTAssertEqual(parser.maximumDepth, maximum)
        _ = try SQLParser(String(repeating: "1+", count: maximum - 1) + "1").parseExpressionOnly()
        for sql in ["(" + valid + ")", String(repeating: "1+", count: maximum) + "1"] {
            XCTAssertThrowsError(try SQLParser(sql).parseExpressionOnly()) {
                XCTAssertEqual(($0 as? JedError)?.code, "54001")
                XCTAssertNil(($0 as? JedError)?.position)
            }
        }
    }

    func testFlatMembershipListHasIndependentExpansionLimit() throws {
        let maximum = SQLParser.maximumInElements
        let body = Array(repeating: "1", count: maximum).joined(separator: ",")
        let parser = try SQLParser("1 IN (" + body + ")")
        _ = try parser.parseExpressionOnly()
        XCTAssertEqual(parser.maximumDepth, 1)
        XCTAssertThrowsError(try SQLParser("1 IN (" + body + ",1)").parseExpressionOnly()) {
            XCTAssertEqual(($0 as? JedError)?.code, "54001")
            XCTAssertNil(($0 as? JedError)?.position)
        }
    }
}
