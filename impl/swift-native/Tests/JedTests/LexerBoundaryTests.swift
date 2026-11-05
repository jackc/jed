import XCTest
@testable import Jed

/// Lexer source locations and host script boundaries cannot be asserted by SQL
/// result-only corpus records. SQL behavior remains in the shared corpus.
final class LexerBoundaryTests: XCTestCase {
    func testTokenByteSpansAndScalarPositions() throws {
        let sql = "/* é */ SELECT \"e\u{301}\", '😀', $12"
        let tokens = try SQLLexer(sql).tokenize()
        let bytes = Array(sql.utf8)
        XCTAssertEqual(tokens.map(\.position), [9, 16, 20, 22, 25, 27, 30])
        for token in tokens {
            XCTAssertLessThanOrEqual(token.byteStart, token.byteEnd)
            XCTAssertLessThanOrEqual(token.byteEnd, bytes.count)
        }
        XCTAssertEqual(tokens[0].kind, .word(try SQLText("select"), quoted: false))
        XCTAssertEqual(tokens[1].kind, .word(try SQLText("e\u{301}"), quoted: true))
        XCTAssertEqual(String(decoding: bytes[tokens[1].byteStart..<tokens[1].byteEnd], as: UTF8.self), "\"e\u{301}\"")
        XCTAssertEqual(tokens.last?.byteStart, bytes.count)
    }

    func testUnicodeTokenConsumesClauseAndRetainsSourceRange() throws {
        let sql = "U&'!D83D!DE00' /*x*/ UESCAPE '!' + 1"
        let tokens = try SQLLexer(sql).tokenize()
        XCTAssertEqual(tokens.count, 4)
        XCTAssertEqual(tokens[0].kind, .string(try SQLText("😀")))
        XCTAssertEqual(tokens[0].byteStart, 0)
        XCTAssertEqual(tokens[0].byteEnd, 32)
        XCTAssertEqual(tokens[1].kind, .symbol("+"))
        XCTAssertEqual(tokens[1].position, 34)
    }

    func testUnicodeErrorsUseCollapsedBodyByteCursorAndHint() throws {
        // The second quote is collapsed before calculating the diagnostic offset,
        // as in PostgreSQL's str_udeescape. Cursor positions intentionally differ
        // from the literal's raw byte index after that doubled quote.
        XCTAssertThrowsError(try SQLLexer("U&'a''b\\z'").tokenize()) {
            let error = $0 as? JedError
            XCTAssertEqual(error?.code, "42601")
            XCTAssertEqual(error?.position, 7)
            XCTAssertEqual(error?.hint, "Unicode escapes must be \\XXXX or \\+XXXXXX.")
        }
        XCTAssertThrowsError(try SQLLexer("U&'\\D800'").tokenize()) {
            XCTAssertEqual(($0 as? JedError)?.position, 9)
        }
        XCTAssertThrowsError(try SQLLexer("U&'x' UESCAPE").tokenize()) {
            XCTAssertEqual(($0 as? JedError)?.position, 14)
        }
    }

    func testMalformedByteInputPrecedesTokenizationAndHasNoPosition() {
        XCTAssertThrowsError(try SQLLexer(utf8: Array("SELECT 1 -- ".utf8) + [0xc0, 0x80]).tokenize()) {
            XCTAssertEqual(($0 as? JedError)?.code, "22021")
            XCTAssertNil(($0 as? JedError)?.position)
        }
    }

    func testNumericSpellingAndGreedyOperatorSpansAreRetained() throws {
        let tokens = try SQLLexer("0x_Ff 1_000.5_0e+2 ->> #- !~* != -|-").tokenize()
        XCTAssertEqual(tokens[0].kind, .number("0x_Ff"))
        XCTAssertEqual(tokens[1].kind, .number("1_000.5_0e+2"))
        XCTAssertEqual(tokens[2].kind, .symbol("->>"))
        XCTAssertEqual(tokens[3].kind, .symbol("#-"))
        XCTAssertEqual(tokens[4].kind, .symbol("!~*"))
        XCTAssertEqual(tokens[5].kind, .symbol("<>"))
        XCTAssertEqual(tokens[5].byteEnd - tokens[5].byteStart, 2)
        XCTAssertEqual(tokens[6].kind, .symbol("-|-"))
    }

    func testDeepCommentsUseIterativeBoundaryScanning() throws {
        let sql = String(repeating: "/*", count: 20_000) + ";'" + String(repeating: "*/", count: 20_000) + "1"
        let tokens = try SQLLexer(sql).tokenize()
        XCTAssertEqual(tokens.count, 2)
        XCTAssertEqual(tokens[0].kind, .number("1"))
        XCTAssertEqual(tokens[0].byteStart, 80_002)
        XCTAssertEqual(Array(splitStatements(sql)).map(\.text), ["1"])
    }

    func testSplitterPreservesExactTextAndUTF8Offsets() {
        let sql = " -- leading\n SELECT 'é;😀' /* tail */; /*middle*/ SELECT \"it\"\";s\", U&'x;y' ;"
        let spans = Array(splitStatements(sql))
        XCTAssertEqual(spans.map(\.text), ["SELECT 'é;😀'", "SELECT \"it\"\";s\", U&'x;y'"])
        let source = Array(sql.utf8)
        for span in spans {
            let end = span.offset + span.text.utf8.count
            XCTAssertEqual(Array(source[span.offset..<end]), Array(span.text.utf8))
        }
        XCTAssertEqual(spans.first?.offset, 13)
    }

    func testSplitterDollarQuotesAndParametersHaveDistinctBoundaries() {
        let spans = Array(splitStatements("SELECT $tag$a;$$;b$tag$; SELECT $1; SELECT $$x;y$$"))
        XCTAssertEqual(spans.map(\.text), ["SELECT $tag$a;$$;b$tag$", "SELECT $1", "SELECT $$x;y$$"])
        XCTAssertEqual(Array(splitStatements("SELECT $tag$unclosed; SELECT 2")).count, 1)
        // The splitter is total even though the baseline SQL grammar does not
        // implement dollar-quoted string literals.
        XCTAssertThrowsError(try SQLLexer("SELECT $$x$$").tokenize()) {
            XCTAssertEqual(($0 as? JedError)?.code, "42601")
        }
    }

    func testSplitterIsLazyTotalAndSkipsInsignificantTails() {
        var iterator = splitStatements(";;; /* only */; SELECT 1; SELECT 'unterminated; ;").makeIterator()
        XCTAssertEqual(iterator.next()?.text, "SELECT 1")
        XCTAssertEqual(iterator.next()?.text, "SELECT 'unterminated; ;")
        XCTAssertNil(iterator.next())
        XCTAssertNil(iterator.next())
        XCTAssertTrue(Array(splitStatements("-- a\n; /* unclosed")).isEmpty)
        XCTAssertEqual(Array(splitStatements("SELECT 1 /* unclosed")).map(\.text), ["SELECT 1"])
        XCTAssertEqual(Array(splitStatements("!; SELECT 2")).map(\.text), ["!", "SELECT 2"])
    }
}
