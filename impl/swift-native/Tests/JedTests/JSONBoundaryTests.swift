import XCTest
@testable import Jed

final class JSONBoundaryTests: XCTestCase {
    /// Host callbacks cannot be represented by the shared SQL corpus. They
    /// expose the lazy boundary without relying on a SQL clock or random seed.
    func testSQLJSONRetainsLazyHostCallbacksAndEscapingCallbackErrors() throws {
        let resolver = ExpressionResolver()
        resolver.hostFunctions = [HostFunctionSignature(id: 42, name: try SQLText("next_value"), arguments: [], result: .i32)]
        var calls = 0
        var execution = ExpressionContext(meter: CostMeter())
        execution.functions.hostCall = { id, values in
            XCTAssertEqual(id, 42); XCTAssertTrue(values.isEmpty)
            calls += 1; return .integer(Int64(calls))
        }
        func evaluate(_ sql: String) throws -> SQLValue { try execution.evaluate(resolver.resolve(SQLParser(sql).parseExpressionOnly())) }
        _ = try evaluate("JSON_VALUE('{\"a\":7}', '$.a' RETURNING i32 DEFAULT next_value() ON EMPTY)")
        XCTAssertEqual(calls, 0)
        guard case .integer(1) = try evaluate("JSON_VALUE('{}', '$.a' RETURNING i32 DEFAULT next_value() ON EMPTY)") else { return XCTFail("default result missing") }
        XCTAssertEqual(calls, 1)
        XCTAssertTrue(try evaluate("JSON_EXISTS(NULL::jsonb, '$n' PASSING next_value() AS n)").isNull)
        XCTAssertEqual(calls, 1)
        execution.functions.hostCall = { _, _ in throw JedError("22012", "injected callback failure") }
        XCTAssertThrowsError(try evaluate("JSON_VALUE('{}', '$.a' RETURNING i32 DEFAULT next_value() ON EMPTY DEFAULT 9 ON ERROR)")) {
            XCTAssertEqual(($0 as? JedError)?.code, "22012")
        }
    }

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

    func testPathProducerBoundsAndCostBoundary() throws {
        let compile: (String) throws -> JSONPathProgram = {
            try JSONPathProgram.compile(SQLText($0), regexCompiler: JSONPathFunctions.regex)
        }
        _ = try compile("$" + String(repeating: ".x", count: 4096))
        _ = try compile(String(repeating: "a/**/", count: 20_000) + "$")
        _ = try compile("$" + String(repeating: " + 1", count: 255))
        code("54001") { _ = try compile("$" + String(repeating: " + 1", count: 256)) }
        _ = try compile(String(repeating: "-", count: 128) + "$")
        code("54001") { _ = try compile(String(repeating: "-", count: 129) + "$") }
        let path = try compile("$.a[*]").text
        let context = try JSONFunctions.node(.text(text(#"{"a":[1,2,3]}"#)))
        let meter = CostMeter()
        let result = try JSONPathFunctions.query(context: context, path: path, meter: meter)
        XCTAssertEqual(meter.accrued, Int64(path.utf8.count))
        XCTAssertNotNil(try result.next())
        XCTAssertEqual(meter.accrued, Int64(path.utf8.count) + 5)
        var count = 1
        while try result.next() != nil { count += 1 }
        XCTAssertEqual(count, 3)
        XCTAssertEqual(meter.accrued, Int64(path.utf8.count) + 7)
        XCTAssertNil(try result.next())
        XCTAssertEqual(meter.accrued, Int64(path.utf8.count) + 7)
        code("54P01") {
            let cursor = try JSONPathFunctions.query(context: context, path: path, meter: CostMeter(limit: meter.accrued))
            while try cursor.next() != nil { }
        }
        let limited = CostMeter(limit: 100)
        code("54P01") {
            _ = try JSONPathFunctions.call("array", context: .array((0..<30).map { .number(SQLDecimal(Int64($0))) }),
                                           path: compile("$[*] ? (@ == $[*])").text, silent: true, meter: limited)
        }
        let closingMeter = CostMeter()
        let closing = try JSONPathFunctions.query(context: context, path: path, meter: closingMeter)
        closing.close()
        XCTAssertNil(try closing.next())
        XCTAssertEqual(closingMeter.accrued, Int64(path.utf8.count))
    }

    func testGINPathExtractionTermBytesAndFallbackBoundaries() throws {
        func extract(_ path: String, match: Bool = true) throws -> GinQueryTree? {
            try GinJSONPath.extract(JSONPathProgram.compile(SQLText(path)), match: match)
        }
        func terms(_ tree: GinQueryTree) -> [[UInt8]] {
            switch tree { case .term(let term): return [term]
            case .all(let nodes), .any(let nodes): return nodes.flatMap(terms) }
        }
        let root = try XCTUnwrap(extract(#"strict $ == "x""#))
        guard case .any = root else { return XCTFail("strict root string needs key OR value") }
        XCTAssertEqual(terms(root), [[1, 120, 0, 1], [5, 120, 0, 1]])
        let array = try XCTUnwrap(extract(#"strict $[*] == "x""#))
        XCTAssertEqual(terms(array), [[1, 120, 0, 1]])
        let member = try XCTUnwrap(extract(#"strict $.a == "x""#))
        XCTAssertEqual(terms(member), [[1, 97, 0, 1], [5, 120, 0, 1]])
        XCTAssertNil(try extract("$.a == 1 || $.b > 2"))
        XCTAssertNil(try extract("!(!($.a == 1) && $.b == 2)"))
        XCTAssertNil(try extract("$.a", match: false))
        let beforeMethod = try XCTUnwrap(extract("$ ? (@.a == 1).type()", match: false))
        let condition = try XCTUnwrap(extract("$.a == 1"))
        XCTAssertEqual(terms(beforeMethod), terms(condition))
    }

    /// This adapter tests the kernel with unchanged shared SQL inputs and expected
    /// results. It deliberately does not resolve SQL types or execute statements.
    /// Table/session cases and unsupported AST shapes are counted as exclusions.
    func testFrozenSharedJSONPathKernelRecords() throws {
        let directory = try XCTUnwrap(Bundle.module.resourceURL?.appendingPathComponent("Fixtures/Corpus"))
        let enumerator = try XCTUnwrap(FileManager.default.enumerator(at: directory, includingPropertiesForKeys: nil))
        let files = enumerator.compactMap { $0 as? URL }.filter { $0.pathExtension == "test" && ($0.lastPathComponent.hasPrefix("jsonpath") || $0.lastPathComponent == "text_to_jsonpath.test") }.sorted { $0.path < $1.path }
        XCTAssertGreaterThan(files.count, 10)
        var tested = 0, excluded = 0, cells = 0
        for file in files {
            let records = JSONPathCorpusAdapter.records(try String(contentsOf: file, encoding: .utf8))
            for record in records {
                let label = "\(file.lastPathComponent):\(record.line) \(record.sql)"
                let adapter = JSONPathCorpusAdapter()
                let statement: SQLStatement
                do { statement = try SQLParser(record.sql).parse() }
                catch { excluded += 1; continue } // SQL grammar/resolve failures are outside this kernel adapter.
                guard adapter.supports(statement) else { excluded += 1; continue }
                tested += 1
                do {
                    let actual = try adapter.run(statement)
                    if let error = record.error { XCTFail("expected \(error): \(label)"); continue }
                    XCTAssertEqual(actual.map { Array($0.utf8) }, record.expected.map { Array($0.utf8) }, label)
                    cells += actual.count
                } catch let error as JedError {
                    XCTAssertEqual(error.code, record.error, "\(label): \(error)")
                } catch { XCTFail("\(label): \(error)") }
            }
        }
        XCTAssertGreaterThan(tested, 250)
        print("Frozen JSONPath kernel adapter: \(tested) records, \(cells) result cells; \(excluded) records excluded (no SQL resolver/executor).")
    }
}

private final class JSONPathCorpusAdapter {
    struct Record { let sql: String; let line: Int; let error: String?; let expected: [String] }
    static func records(_ source: String) -> [Record] {
        let lines = source.components(separatedBy: "\n"); var index = 0, result: [Record] = []
        while index < lines.count {
            let header = lines[index], start = index + 1; index += 1
            let query = header.hasPrefix("query "), failure = header.hasPrefix("statement error ")
            guard query || failure else { continue }
            var sql: [String] = [], expected: [String] = []
            while index < lines.count && !lines[index].isEmpty && lines[index] != "----" { sql.append(lines[index]); index += 1 }
            if query && index < lines.count && lines[index] == "----" {
                index += 1
                while index < lines.count && !lines[index].isEmpty { expected.append(lines[index]); index += 1 }
            }
            result.append(Record(sql: sql.joined(separator: "\n"), line: start,
                                 error: failure ? String(header.split(separator: " ")[2]) : nil, expected: expected))
        }
        return result
    }
    let meter = CostMeter()
    func isNull(_ value: SQLValue) -> Bool { if case .null = value { return true }; return false }
    func isTrue(_ value: SQLValue) -> Bool { if case .boolean(true) = value { return true }; return false }
    func supportedCall(_ call: SQLCall, query: Bool = false) -> Bool {
        let name = call.name.description
        let scalar = ["jsonb_path_exists", "jsonb_path_exists_tz", "jsonb_path_match", "jsonb_path_match_tz", "jsonb_path_query_first", "jsonb_path_query_first_tz", "jsonb_path_query_array", "jsonb_path_query_array_tz"]
        return (query ? ["jsonb_path_query", "jsonb_path_query_tz"].contains(name) : scalar.contains(name))
            && (2...4).contains(call.arguments.count) && call.arguments.allSatisfy { $0.name == nil && supports($0.value) }
    }
    func supports(_ expression: SQLExpression) -> Bool {
        switch expression.kind {
        case .string, .boolean, .null: return true
        case .parenthesized(let value): return supports(value)
        case .cast(let value, let type): return ["json", "jsonb", "jsonpath", "text"].contains(type.name.description) && supports(value)
        case .typedString(let type, _): return ["json", "jsonb", "jsonpath", "text"].contains(type.name.description)
        case .binary(let op, let a, let b): return ["@?", "@@"].contains(op) && supports(a) && supports(b)
        case .call(let call): return supportedCall(call)
        default: return false
        }
    }
    func supports(_ statement: SQLStatement) -> Bool {
        guard case .query(let query) = statement, case .select(let select) = query.body,
              query.orderBy.isEmpty, query.limit == nil, query.offset == nil, select.predicate == nil else { return false }
        if select.from.isEmpty { return select.items.allSatisfy { $0.expression.map(supports) ?? false } }
        guard select.from.count == 1, select.items.count == 1, select.items[0].star,
              case .function(let expression) = select.from[0].source, case .call(let call) = expression.kind else { return false }
        return supportedCall(call, query: true)
    }
    func run(_ statement: SQLStatement) throws -> [String] {
        guard case .query(let query) = statement, case .select(let select) = query.body else { return [] }
        if select.from.isEmpty { return try select.items.map { try render(value($0.expression!)) } }
        guard case .function(let expression) = select.from[0].source, case .call(let call) = expression.kind else { return [] }
        let args = try call.arguments.map { try value($0.value) }
        if args.contains(where: isNull) { return [] }
        let cursor = try JSONPathFunctions.query(context: JSONFunctions.node(args[0]), path: canonicalPath(args[1]),
                                           variables: args.count > 2 ? JSONFunctions.node(args[2]) : .object([]),
                                           silent: args.count > 3 && isTrue(args[3]), meter: meter)
        var results: [String] = []
        while let value = try cursor.next() { results.append(try SQLJSON.render(value).description) }
        return results
    }
    func cast(_ input: SQLValue, to type: String) throws -> SQLValue {
        if isNull(input) { return .null }
        if type == "text" { return .text(try SQLText(render(input))) }
        let source: SQLText
        switch input { case .text(let text), .json(let text), .jsonpath(let text): source = text
        case .jsonb(let value): source = try SQLJSON.render(value)
        default: throw JedError("42846", "unsupported cast in kernel adapter") }
        if type == "jsonpath" { return .jsonpath(try JSONPathProgram.compile(source, regexCompiler: JSONPathFunctions.regex).text) }
        return try SQLJSON.parse(text: source, preserveText: type == "json")
    }
    func canonicalPath(_ value: SQLValue) throws -> SQLText {
        if case .jsonpath(let text) = value { return text }
        guard case .text(let text) = value else { throw JedError("42883", "invalid path argument") }
        return try JSONPathProgram.compile(text, regexCompiler: JSONPathFunctions.regex).text
    }
    func value(_ expression: SQLExpression) throws -> SQLValue {
        switch expression.kind {
        case .string(let text): return .text(text)
        case .boolean(let bool): return .boolean(bool)
        case .null: return .null
        case .parenthesized(let expression): return try value(expression)
        case .cast(let expression, let type): return try cast(value(expression), to: type.name.description)
        case .typedString(let type, let text): return try cast(.text(text), to: type.name.description)
        case .binary(let op, let lhs, let rhs):
            let a = try value(lhs), b = try value(rhs)
            if isNull(a) || isNull(b) { return .null }
            return try JSONPathFunctions.call(op, context: JSONFunctions.node(a), path: canonicalPath(b), constant: true, operatorForm: true, meter: meter)
        case .call(let call):
            let args = try call.arguments.map { try value($0.value) }
            if args.contains(where: isNull) { return .null }
            return try JSONPathFunctions.call(call.name.description, context: JSONFunctions.node(args[0]), path: canonicalPath(args[1]),
                                               variables: args.count > 2 ? JSONFunctions.node(args[2]) : .object([]),
                                               silent: args.count > 3 && isTrue(args[3]), constant: true, meter: meter)
        default: throw JedError("XX000", "unsupported AST in kernel adapter")
        }
    }
    func render(_ value: SQLValue) throws -> String {
        switch value {
        case .null: return "NULL"
        case .boolean(let value): return value ? "true" : "false"
        case .text(let text), .json(let text), .jsonpath(let text): return text.utf8.isEmpty ? "(empty)" : text.description
        case .jsonb(let value): return try SQLJSON.render(value).description
        default: throw JedError("XX000", "unsupported result in kernel adapter")
        }
    }
}
