import XCTest
@testable import Jed

final class ExpressionBoundaryTests: XCTestCase {
    /// Parameter inference is host-visible metadata and must remain consistent
    /// across separately prepared expression trees in the same statement.
    func testConditionalParameterAdoptionAndByteInputBoundaries() throws {
        for sql in ["COALESCE($1, 1)", "CASE WHEN true THEN $1 ELSE 1 END", "GREATEST($1, 1)"] {
            let resolver = ExpressionResolver()
            _ = try resolver.resolve(SQLParser(sql).parseExpressionOnly())
            XCTAssertEqual(resolver.parameters[1]?.type, .scalar(.i64))
            XCTAssertThrowsError(try resolver.resolve(SQLParser("$1::text").parseExpressionOnly())) { error in
                XCTAssertEqual((error as? JedError)?.code, "42P08")
            }
        }
        // Exercise every octal byte against the independent hex output parser,
        // including high bytes whose intermediate UInt8 sum must not overflow.
        let bytes = (0...255).map(UInt8.init)
        let escape = bytes.map { "\\" + String($0 / 64) + String(($0 / 8) % 8) + String($0 % 8) }.joined()
        XCTAssertEqual(try ScalarCasts.bytea(SQLText(escape)), bytes)
        XCTAssertEqual(try ScalarCasts.bytea(ScalarCasts.render(.bytea(bytes))), bytes)
    }
    /// Shared records enter the real parser/resolver/evaluator. This deliberately
    /// measures only the expression slice: there is no database/query planner yet.
    func testFrozenSharedScalarExpressionRecords() throws {
        let directory = try XCTUnwrap(Bundle.module.resourceURL?.appendingPathComponent("Fixtures/Corpus"))
        let enumerator = try XCTUnwrap(FileManager.default.enumerator(at: directory, includingPropertiesForKeys: nil))
        let files = enumerator.compactMap { $0 as? URL }.filter { url in
            url.pathExtension == "test" && ["cast", "types", "expr"].contains(url.deletingLastPathComponent().lastPathComponent)
        }.sorted { $0.path < $1.path }
        var tested = 0, excluded = 0, cells = 0, costs = 0, types = 0
        var zones = TimeZoneDatabase()
        let zoneFile = try XCTUnwrap(Bundle.module.url(forResource: "tzdata", withExtension: "jtz", subdirectory: "Fixtures"))
        try zones.load(Array(Data(contentsOf: zoneFile)))
        for file in files {
            for record in ScalarExpressionRecords.read(try String(contentsOf: file, encoding: .utf8)) {
                let label = "\(file.lastPathComponent):\(record.line) \(record.sql)"
                let statement: SQLStatement
                do { statement = try SQLParser(record.sql).parse() } catch { excluded += 1; continue }
                guard case .query(let query) = statement, case .select(let select) = query.body,
                      select.from.isEmpty, select.grouping.isEmpty, select.having == nil, select.predicate == nil,
                      !select.distinct, select.into == nil, select.windows.isEmpty,
                      query.orderBy.isEmpty, query.limit == nil, query.offset == nil,
                      select.items.allSatisfy({ $0.expression.map(ScalarExpressionRecords.supports) == true }) else { excluded += 1; continue }
                tested += 1
                let zone = try record.timezone.map { try zones.resolve(SQLText($0)) } ?? .utc
                let meter = CostMeter(), resolver = ExpressionResolver(scalar: .init(timezone: zone, statementMicros: record.clock, meter: meter))
                do {
                    let expressions = try select.items.map { try resolver.resolve($0.expression!) }
                    // The FROM-less projection's owning row charge precedes its
                    // expression evaluations, exactly as the eventual query drive.
                    meter.charge(1); try meter.guardCost()
                    var context = ExpressionContext(meter: meter, timezone: zone, statementMicros: record.clock)
                    context.functions.timezones = zones
                    let values = try expressions.map { try context.evaluate($0) }
                    let actual = try zip(values, expressions).map { value, expression in
                        value.isNull ? "NULL" : try ContainerText.render(value, type: expression.type ?? TypeDescriptor(.scalar(.text)), context: .init(timezone: zone)).description
                    }
                    if let expected = record.error { XCTFail("expected \(expected): \(label)") }
                    else {
                        XCTAssertEqual(actual.map { Array($0.utf8) }, record.expected.map { Array($0.utf8) }, label)
                        cells += actual.count
                        if let expected = record.cost { XCTAssertEqual(meter.accrued, expected, label); costs += 1 }
                        if let expected = record.types { XCTAssertEqual(expressions.map { $0.type?.type.name ?? "text" }, expected, label); types += 1 }
                    }
                } catch let error as JedError { XCTAssertEqual(error.code, record.error, "\(label): \(error)") }
                catch { XCTFail("\(label): \(error)") }
            }
        }
        XCTAssertGreaterThan(tested, 100)
        print("Frozen scalar-expression slice: \(tested) records, \(cells) result cells, \(costs) cost assertions, \(types) type assertions; \(excluded) records excluded (no database/query plan or remaining expressions).")
    }
}

private enum ScalarExpressionRecords {
    struct Record {
        let sql: String; let line: Int; let error: String?; let expected: [String]
        let cost: Int64?; let types: [String]?
        let clock: Int64?; let timezone: String?
    }
    static func read(_ source: String) -> [Record] {
        let lines = source.components(separatedBy: "\n")
        var index = 0, result: [Record] = [], cost: Int64?, types: [String]?
        var clock: Int64?, timezone: String?
        while index < lines.count {
            let header = lines[index], start = index + 1; index += 1
            if header.hasPrefix("# cost:") { cost = Int64(header.dropFirst(7).trimmingCharacters(in: .whitespaces)); continue }
            if header.hasPrefix("# types:") { types = header.dropFirst(8).split(whereSeparator: { $0 == " " || $0 == "," }).map(String.init); continue }
            if header.hasPrefix("# clock:") { clock = Int64(header.dropFirst(8).trimmingCharacters(in: .whitespaces)); continue }
            if header.hasPrefix("# clock_advance:") { clock = Int64(header.dropFirst(16).split(separator: ",")[0].trimmingCharacters(in: .whitespaces)); continue }
            if header.hasPrefix("# timezone:") { timezone = header.dropFirst(11).trimmingCharacters(in: .whitespaces); continue }
            let query = header.hasPrefix("query "), failure = header.hasPrefix("statement error ")
            guard query || failure || header == "statement ok" else { continue }
            defer { cost = nil; types = nil; clock = nil; timezone = nil }
            var sql: [String] = [], expected: [String] = []
            while index < lines.count && !lines[index].isEmpty && lines[index] != "----" { sql.append(lines[index]); index += 1 }
            if query && index < lines.count && lines[index] == "----" {
                index += 1
                while index < lines.count && !lines[index].isEmpty { expected.append(lines[index]); index += 1 }
            }
            if query || failure {
                result.append(Record(sql: sql.joined(separator: "\n"), line: start,
                                     error: failure ? String(header.split(separator: " ")[2]) : nil,
                                     expected: expected, cost: cost, types: types, clock: clock, timezone: timezone))
            }
        }
        return result
    }
    static func supports(_ expression: SQLExpression) -> Bool {
        switch expression.kind {
        case .number, .string, .boolean, .null: return true
        case .parenthesized(let e), .unary(_, let e), .isNull(let e, _): return supports(e)
        case .cast(let e, let type): return scalar(type) && supports(e)
        case .typedString(let type, _): return scalar(type)
        case .binary(let op, let a, let b):
            return ["+", "-", "*", "/", "%", "||", "=", "<>", "<", ">", "<=", ">=", "and", "or", "@>", "<@", "&&", "<<", ">>", "&<", "&>", "-|-"] .contains(op) && supports(a) && supports(b)
        case .isDistinct(let a, let b, _): return supports(a) && supports(b)
        case .between(let a, let b, let c, _): return supports(a) && supports(b) && supports(c)
        case .member(let a, let candidates, _): return supports(a) && candidates.allSatisfy(supports)
        case .conditional(let operand, let arms, let fallback):
            return (operand.map(supports) ?? true) && arms.allSatisfy { supports($0.condition) && supports($0.result) } && (fallback.map(supports) ?? true)
        case .conditionalFunction(let name, let args): return ["coalesce", "greatest", "least", "nullif"].contains(name) && args.allSatisfy(supports)
        case .row(let args), .array(let args): return args.allSatisfy(supports)
        case .field(let source, let name): return name != nil && supports(source)
        case .subscripted(let source, let subscripts): return supports(source) && subscripts.allSatisfy { ($0.lower.map(supports) ?? true) && ($0.upper.map(supports) ?? true) }
        case .quantified(_, let operand, _, let source): return supports(operand) && supports(source)
        case .call(let call):
            return connectedFunctions.contains(call.name.description) && !call.star && !call.distinct && call.orderBy.isEmpty && call.withinGroup.isEmpty && call.filter == nil && call.over == nil && call.overName == nil && call.arguments.allSatisfy { supports($0.value) }
        default: return false
        }
    }
    static func scalar(_ type: SQLTypeSyntax) -> Bool { (try? TypeRules.resolve(type, composites: [:])) != nil }
    static let connectedFunctions = TextFunctions.names.union([
        "i32range", "int4range", "i64range", "int8range", "numrange", "tsrange", "tstzrange", "daterange", "isempty", "lower_inc", "upper_inc", "lower_inf", "upper_inf", "range_merge",
        "array_ndims", "array_length", "array_lower", "array_upper", "cardinality", "array_dims", "array_append", "array_prepend", "array_cat", "array_remove", "array_replace", "array_position", "array_positions", "array_to_string", "string_to_array",
        "encode", "decode", "get_byte", "set_byte", "get_bit", "set_bit",
        "abs", "round", "trunc", "ceil", "ceiling", "floor", "sqrt", "exp", "ln", "log", "log10", "pow", "power", "sin", "cos", "tan", "cbrt", "pi", "radians", "degrees", "asin", "acos", "atan", "atan2", "cot", "sinh", "cosh", "tanh", "asinh", "acosh", "atanh", "sign", "factorial", "scale", "min_scale", "trim_scale", "mod", "div", "gcd", "lcm", "width_bucket",
        "now", "current_timestamp", "current_date", "localtimestamp", "make_date", "make_timestamp", "make_timestamptz", "make_interval", "justify_hours", "justify_days", "justify_interval", "age", "date_trunc", "date_part", "extract", "timezone", "to_char", "to_date", "to_timestamp", "to_number",
        "uuid_extract_version", "uuid_extract_timestamp", "md5", "sha224", "sha256", "sha384", "sha512", "length", "char_length", "character_length", "octet_length", "bit_length", "lower", "upper", "num_nulls", "num_nonnulls",
        "jsonb_typeof", "json_typeof", "jsonb_array_length", "json_array_length", "jsonb_strip_nulls", "json_strip_nulls", "jsonb_pretty", "to_json", "to_jsonb", "json_scalar", "json_serialize", "json_build_array", "jsonb_build_array", "json_build_object", "jsonb_build_object", "similar_to_escape",
        "regexp_like", "regexp_match", "regexp_count", "regexp_instr", "regexp_substr", "regexp_replace", "regexp_split_to_array"
    ])
}
