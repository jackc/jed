import XCTest
@testable import Jed

final class JSONBoundaryTests: XCTestCase {
    private func text(_ value: String) throws -> SQLText { try SQLText(value) }
    private func code(_ expected: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, expected, "\($0)", file: file, line: line) }
    }

    func testInputModesKeepBytesAndCanonicalizeOnlyDecodedValues() throws {
        let original = try text(" { \"aa\": 1e2, \"b\":1.50, \"aa\":2 } \n")
        guard case .json(let stored) = try SQLJSON.parse(text: original, preserveText: true),
              case .jsonb(let value) = try SQLJSON.parse(text: original) else { return XCTFail("wrong input mode") }
        XCTAssertEqual(stored, original)
        XCTAssertEqual(try SQLJSON.render(value).description, "{\"b\": 1.50, \"aa\": 2}")
        let body = try ValueCodec.encodeBody(.jsonb(value), type: TypeDescriptor(.scalar(.jsonb)))
        XCTAssertEqual(try ValueCodec.encodeBody(ValueCodec.decodeBody(body, type: TypeDescriptor(.scalar(.jsonb))), type: TypeDescriptor(.scalar(.jsonb))), body)
        for input in [#""\u0000""#, #""\ud800""#, #""\udc00""#] {
            guard case .json(let stored) = try SQLJSON.parse(text: text(input), preserveText: true) else { return XCTFail() }
            XCTAssertEqual(stored.description, input)
        }
        code("22P05") { _ = try SQLJSON.parse(text: text(#""\u0000""#)) }
        code("22P02") { _ = try SQLJSON.parse(text: text(#""\ud800""#)) }
        guard case .jsonb(.string(let emoji)) = try SQLJSON.parse(text: text(#""\uD83D\uDE00""#)) else { return XCTFail() }
        XCTAssertEqual(emoji.utf8, [0xf0, 0x9f, 0x98, 0x80])
        guard case .jsonb(.string(let bom)) = try SQLJSON.parse(text: text("\"\u{feff}x\"")) else { return XCTFail() }
        XCTAssertEqual(bom.utf8, [0xef, 0xbb, 0xbf, 0x78])
    }

    func testParsingDepthAndErrorOrderAreBoundedAtTheProducer() throws {
        for preserve in [false, true] {
            let allowed = String(repeating: "[", count: 256) + "0" + String(repeating: "]", count: 256)
            _ = try SQLJSON.parse(text: text(allowed), preserveText: preserve)
            code("54001") { _ = try SQLJSON.parse(text: text("[" + allowed + "]"), preserveText: preserve) }
            code("22P02") { _ = try SQLJSON.parse(text: text("[x," + allowed + "]"), preserveText: preserve) }
        }
        code("22003") { _ = try SQLJSON.parse(text: text(#"[1e999999,"\u0000"]"#)) }
        code("22P05") { _ = try SQLJSON.parse(text: text(#"["\u0000",1e999999]"#)) }
        let largeNumber = try text("1e999999")
        _ = try SQLJSON.parse(text: largeNumber, preserveText: true)
        var deepest = SQLJSONValue.null
        for _ in 0..<256 { deepest = .array([deepest]) }
        code("54001") { _ = try JSONFunctions.joinArray([.jsonb(deepest)], binary: true) }
    }

    func testPrettySizingChargesBeforeAllocationAndCostWins() throws {
        let node = SQLJSONValue.object([SQLJSONMember(key: try text("a"), value: .array([]))])
        let expected = "{\n    \"a\": [\n    ]\n}"
        XCTAssertEqual(try SQLJSON.render(node, pretty: true).description, expected)
        let payload = MemorySizes.payload(.jsonb(node)), count = Int64(expected.utf8.count)
        let meter = CostMeter(resources: StatementResources(scalarLimit: count))
        _ = try SQLJSON.render(node, pretty: true, meter: meter)
        XCTAssertEqual(meter.accrued, payload + count)
        XCTAssertEqual(meter.resources.scalarUsed, count)
        code("54P04") { _ = try SQLJSON.render(node, pretty: true, meter: CostMeter(resources: StatementResources(scalarLimit: count - 1))) }
        code("54P01") { _ = try SQLJSON.render(node, pretty: true, meter: CostMeter(limit: payload + count, resources: StatementResources(scalarLimit: 1))) }
        let constant = CostMeter()
        _ = try JSONFunctions.chargeGrowth(.jsonb(node), constant: true, meter: constant)
        XCTAssertEqual(constant.accrued, 0)
        let variable = CostMeter()
        _ = try JSONFunctions.chargeGrowth(.jsonb(node), constant: false, meter: variable)
        XCTAssertEqual(variable.accrued, payload)
    }

    func testUnicodeDiagnosticsAndWholeDocumentValidation() throws {
        do { _ = try SQLJSON.parse(text: text(#""\ud800\ud800""#)); XCTFail() }
        catch let error as JedError { XCTAssertEqual(error.detail, "Unicode high surrogate must not follow a high surrogate.") }
        do { _ = try SQLJSON.parse(text: text(#""\u0000""#)); XCTFail() }
        catch let error as JedError { XCTAssertEqual(error.detail, "\\u0000 cannot be converted to text.") }
        let source = SQLValue.json(try text(#"{"a":1,"b":"\u0000"}"#))
        code("22P05") { _ = try JSONFunctions.field(source, key: text("a"), meter: CostMeter()) }
        let invalidInsideArray = try text(#"["\u0000"]"#)
        code("22023") { _ = try JSONDocument(invalidInsideArray, expectedRoot: "object", rootError: JedError("22023", "wrong root")) }
    }
}
