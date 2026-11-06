import XCTest
@testable import Jed

final class ExpressionBoundaryTests: XCTestCase {
    /// Constructor hints remain live while fixed-context descendants are
    /// retained. Admission counts bound preparation work independently of CPU.
    func testConstructorHintsRetainCompoundChildren() throws {
        let count = 32
        for wrapped in [false, true] {
            var sql = "1"
            for _ in 0..<count {
                sql = "array_length(ARRAY[\(wrapped ? "-(\(sql))" : sql)], 1)"
            }
            let resolver = ExpressionResolver()
            var admissions = 0
            resolver.checkFunction = { _ in
                admissions += 1
                guard admissions <= count * 2 else { throw JedError("XX000", "repeated constructor child admission") }
            }
            _ = try resolver.resolve(SQLParser(sql).parseExpressionOnly())
            XCTAssertEqual(admissions, count)
        }

        let resolver = ExpressionResolver()
        let array = ExpressionResolver.Operand(try SQLParser("ARRAY[$1, 1]").parseExpressionOnly(), resolver: resolver)
        let narrow = TypeDescriptor(.array(.scalar(.i16))), wide = TypeDescriptor(.array(.scalar(.i64)))
        XCTAssertEqual(try array.resolve(expected: narrow).type, narrow)
        XCTAssertEqual(try array.resolve(expected: wide).type, wide)
        XCTAssertEqual(resolver.parameters[1]?.type, .scalar(.i64))
        XCTAssertThrowsError(try array.resolve(expected: TypeDescriptor(.array(.scalar(.text))))) {
            XCTAssertEqual(($0 as? JedError)?.code, "42804")
        }

        var admissions = 0
        resolver.checkFunction = { _ in admissions += 1 }
        let row = ExpressionResolver.Operand(try SQLParser("ROW(NULL, lower('A'))").parseExpressionOnly(), resolver: resolver)
        for scalar in [ScalarType.i16, .i64] {
            let type = TypeDescriptor(.record([
                TypeField(name: try SQLText("f1"), descriptor: TypeDescriptor(.scalar(scalar))),
                TypeField(name: try SQLText("f2"), descriptor: TypeDescriptor(.scalar(.text)))
            ]))
            XCTAssertEqual(try row.resolve(expected: type).type, type)
        }
        XCTAssertEqual(admissions, 1)
    }

    /// Function-family fallthrough must share resolved original operands, while
    /// retaining the capability callback once for every authored call.
    func testFunctionProbesRetainNestedCompoundArguments() throws {
        for mode in 0..<5 {
            // The terminal ARRAY adds one level beyond its scalar literal.
            let count = mode == 2 ? 254 : 255
            var sql = mode == 2 ? "ARRAY['a']" : mode >= 3 ? "1" : "'A'"
            for depth in 0..<count {
                if mode == 2 { sql = "array_append(\(sql), 'b')" }
                else if mode == 3 { sql = "abs(\(sql))" }
                else if mode == 4 { sql = "mod(\(sql), 2)" }
                else { sql = "\(mode == 0 || depth % 2 == 0 ? "lower" : "upper")(\(sql))" }
            }
            let resolver = ExpressionResolver()
            var admissions = 0
            resolver.checkFunction = { _ in
                admissions += 1
                guard admissions <= count * 2 else { throw JedError("XX000", "repeated nested function admission") }
            }
            SQLCorpusTrace.write("Resolver nested function depth: mode=\(mode) calls=\(count)")
            _ = try resolver.resolve(SQLParser(sql).parseExpressionOnly())
            XCTAssertEqual(admissions, count)
        }
    }

    /// Capability admission is observable through the resolver's host hook.
    /// Operator selection must not revisit a resolved compound subtree for
    /// every overloaded candidate; a long chain used to grow exponentially.
    func testOperatorProbesRetainResolvedCompoundOperands() throws {
        let count = 32
        for (operand, operation) in [
            ("abs(1)", "+"),
            ("md5('x')", "||"),
            ("decode('00', 'hex')", "||"),
            ("ARRAY[abs(1)]", "||"),
            ("jsonb_build_object('v', 1)", "||"),
        ] {
            let resolver = ExpressionResolver()
            var admissions = 0
            resolver.checkFunction = { _ in
                admissions += 1
                guard admissions <= count * 2 else { throw JedError("XX000", "repeated function admission: \(operand)") }
            }
            let sql = Array(repeating: operand, count: count).joined(separator: " \(operation) ")
            SQLCorpusTrace.write("Resolver chain begin: \(operand)")
            _ = try resolver.resolve(SQLParser(sql).parseExpressionOnly())
            if operand.hasPrefix("ARRAY") {
                // Array constructors legitimately revisit their elements under
                // the selected polymorphic hint. That work must remain linear.
                XCTAssertGreaterThanOrEqual(admissions, count, sql)
                XCTAssertLessThanOrEqual(admissions, count * 2, sql)
            } else { XCTAssertEqual(admissions, count, sql) }
        }
    }

    /// Parameter inference is host-visible metadata and must remain consistent
    /// across separately prepared expression trees in the same statement.
    func testConditionalParameterAdoptionAndByteInputBoundaries() throws {
        for sql in ["COALESCE($1, 1)", "CASE WHEN true THEN $1 ELSE 1 END", "GREATEST($1, 1)"] {
            let resolver = ExpressionResolver()
            _ = try resolver.resolve(SQLParser(sql).parseExpressionOnly())
            XCTAssertEqual(resolver.parameters[1]?.type, .scalar(.i64))
            XCTAssertThrowsError(try resolver.resolve(SQLParser("$1::text").parseExpressionOnly())) { error in
                XCTAssertEqual((error as? JedError)?.code, "42804")
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
            let folder = url.deletingLastPathComponent().lastPathComponent
            return url.pathExtension == "test" && ["cast", "types", "expr", "json", "compare", "null", "collation", "params", "session"].contains(folder)
        }.sorted { $0.path < $1.path }
        var tested = 0, excluded = 0, cells = 0, costs = 0, types = 0
        func fixture(_ name: String, _ ext: String) throws -> [UInt8] {
            let file = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: ext, subdirectory: "Fixtures"))
            return Array(try Data(contentsOf: file))
        }
        for file in files {
            let source = try String(contentsOf: file, encoding: .utf8)
            var fileBuilder = Environment.builder()
            let directives = source.components(separatedBy: "\n").map { $0.trimmingCharacters(in: .whitespaces) }
            if directives.contains(where: { $0.hasPrefix("# load-collation:") || $0.hasPrefix("# fixture:") }) {
                try fileBuilder.loadUnicodeData(fixture("unicode", "jucd"))
            }
            if directives.contains(where: { $0.hasPrefix("# load-timezone:") }) {
                try fileBuilder.loadTimeZoneData(fixture("tzdata", "jtz"))
            }
            let fileEnvironment = fileBuilder.build()
            var lifetimeAccrued: Int64 = 0
            for record in try ScalarExpressionRecords.read(source) {
                let label = "\(file.lastPathComponent):\(record.line) \(record.sql)"
                let statement: SQLStatement
                do { statement = try SQLParser(record.sql).parse() } catch { excluded += 1; continue }
                guard case .query(let query) = statement, case .select(let select) = query.body,
                      select.from.isEmpty, select.grouping.isEmpty, select.having == nil, select.predicate == nil,
                      !select.distinct, select.into == nil, select.windows.isEmpty,
                      query.orderBy.isEmpty, query.limit == nil, query.offset == nil,
                      select.items.allSatisfy({ $0.expression.map(ScalarExpressionRecords.supports) == true }) else { excluded += 1; continue }
                tested += 1
                do {
                    var builder = fileEnvironment.toBuilder()
                    if let seed = record.seed { builder.setRandomSource(SeededRandomSource(seed: seed)) }
                    if let clock = record.clock {
                        if let step = record.clockStep { builder.setClockSource(AdvancingClock(start: clock, step: step)) }
                        else { builder.setClockSource(ManualClock(microseconds: clock)) }
                    }
                    var options = SessionOptions()
                    options.maxCost = record.maxCost; options.lifetimeMaxCost = record.lifetimeMaxCost
                    options.defaultPrivileges = record.defaultPrivileges; options.allowDDL = record.allowDDL
                    var settings = SessionSettings(environment: builder.build(), options: options)
                    settings.lifetime.accrued = lifetimeAccrued
                    defer { lifetimeAccrued = settings.lifetime.accrued }
                    for grant in record.grants { settings.privileges.grant(grant.privileges, on: grant.name, function: grant.function) }
                    for revoke in record.revocations { settings.privileges.revoke(revoke.privileges, on: revoke.name, function: revoke.function) }
                    if let zone = record.timezone { try settings.setTimeZone(SQLText(zone)) }
                    for (name, value) in record.variables { try settings.setVariable(SQLText(name), value: SQLText(value)) }
                    let meter = try settings.newMeter()
                    let services = StatementServices(settings: settings, meter: meter)
                    var context = services.context()
                    let resolver = ExpressionResolver(scalar: context.scalar)
                    resolver.unicode = settings.environment.unicode
                    resolver.checkFunction = { try settings.privileges.require(.execute, on: $0, function: true) }
                    let expressions = try select.items.map { try resolver.resolve($0.expression!) }
                    context.parameters = try ParameterBinding.bind(record.parameters, types: ParameterBinding.finalize(resolver), timezone: settings.timeZone)
                    // The FROM-less projection's owning row charge precedes its
                    // expression evaluations, exactly as the eventual query drive.
                    meter.charge(1); try meter.guardCost()
                    let values = try expressions.map { try context.evaluate($0) }
                    let actual = try zip(values, expressions).map { value, expression in
                        value.isNull ? "NULL" : try settings.render(value, type: expression.type ?? TypeDescriptor(.scalar(.text))).description
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
    struct Grant { let privileges: PrivilegeSet; let name: SQLText; let function: Bool }
    struct Record {
        let sql: String; let line: Int; let error: String?; let expected: [String]
        let cost: Int64?; let types: [String]?
        let clock: Int64?; let clockStep: Int64?; let timezone: String?; let seed: UInt64?
        let variables: [(String, String)]
        let parameters: [SQLValue]
        let maxCost: Int64; let lifetimeMaxCost: Int64
        let defaultPrivileges: PrivilegeSet; let grants: [Grant]; let revocations: [Grant]; let allowDDL: Bool
    }
    static func read(_ source: String) throws -> [Record] {
        let lines = source.components(separatedBy: "\n")
        var index = 0, result: [Record] = [], cost: Int64?, types: [String]?
        var clock: Int64?, clockStep: Int64?, timezone: String?, seed: UInt64?
        var variables: [(String, String)] = []
        var parameters: [SQLValue] = []
        var maxCost: Int64 = 0, lifetimeMaxCost: Int64 = 0
        var defaultPrivileges = PrivilegeSet.allTables, grants: [Grant] = [], revocations: [Grant] = [], allowDDL = true
        while index < lines.count {
            let header = lines[index], start = index + 1; index += 1
            if header.hasPrefix("# cost:") { cost = Int64(header.dropFirst(7).trimmingCharacters(in: .whitespaces)); continue }
            if header.hasPrefix("# types:") { types = header.dropFirst(8).split(whereSeparator: { $0 == " " || $0 == "," }).map(String.init); continue }
            if header.hasPrefix("# max_cost:") { maxCost = Int64(header.dropFirst(11).trimmingCharacters(in: .whitespaces)) ?? 0; continue }
            if header.hasPrefix("# lifetime_max_cost:") { lifetimeMaxCost = Int64(header.dropFirst(20).trimmingCharacters(in: .whitespaces)) ?? 0; continue }
            if header.hasPrefix("# default_privileges:") { defaultPrivileges = try privileges(String(header.dropFirst(21))); continue }
            if header.hasPrefix("# grant:") { grants.append(try grant(String(header.dropFirst(8)))); continue }
            if header.hasPrefix("# revoke:") { revocations.append(try grant(String(header.dropFirst(9)))); continue }
            if header.hasPrefix("# allow_ddl:") { allowDDL = header.dropFirst(12).trimmingCharacters(in: .whitespaces) == "on"; continue }
            if header.hasPrefix("# params:") { parameters = try ParameterDirective.parse(String(header.dropFirst(9))); continue }
            if header.hasPrefix("# seed:") { seed = UInt64(header.dropFirst(7).trimmingCharacters(in: .whitespaces)); continue }
            if header.hasPrefix("# clock:") { clock = Int64(header.dropFirst(8).trimmingCharacters(in: .whitespaces)); clockStep = nil; continue }
            if header.hasPrefix("# clock_advance:") {
                let fields = header.dropFirst(16).split(separator: ",")
                if fields.count == 2 { clock = Int64(fields[0].trimmingCharacters(in: .whitespaces)); clockStep = Int64(fields[1].trimmingCharacters(in: .whitespaces)) }
                continue
            }
            if header.hasPrefix("# set:") {
                variables = header.dropFirst(6).split(separator: ",").compactMap { item in
                    let fields = item.split(separator: "=", maxSplits: 1, omittingEmptySubsequences: false)
                    guard fields.count == 2 else { return nil }
                    return (fields[0].trimmingCharacters(in: .whitespaces), fields[1].trimmingCharacters(in: .whitespaces))
                }
                continue
            }
            if header.hasPrefix("# timezone:") { timezone = header.dropFirst(11).trimmingCharacters(in: .whitespaces); continue }
            let query = header.hasPrefix("query "), failure = header.hasPrefix("statement error ")
            guard query || failure || header == "statement ok" else { continue }
            defer {
                cost = nil; types = nil; clock = nil; clockStep = nil; timezone = nil; seed = nil; variables = []; parameters = []
                maxCost = 0; defaultPrivileges = .allTables; grants = []; revocations = []; allowDDL = true
            }
            var sql: [String] = [], expected: [String] = []
            while index < lines.count && !lines[index].isEmpty && lines[index] != "----" { sql.append(lines[index]); index += 1 }
            if query && index < lines.count && lines[index] == "----" {
                index += 1
                while index < lines.count && !lines[index].isEmpty { expected.append(lines[index]); index += 1 }
            }
            if query || failure {
                result.append(Record(sql: sql.joined(separator: "\n"), line: start,
                                     error: failure ? String(header.split(separator: " ")[2]) : nil,
                                     expected: expected, cost: cost, types: types, clock: clock, clockStep: clockStep,
                                     timezone: timezone, seed: seed, variables: variables, parameters: parameters,
                                     maxCost: maxCost, lifetimeMaxCost: lifetimeMaxCost, defaultPrivileges: defaultPrivileges,
                                     grants: grants, revocations: revocations, allowDDL: allowDDL))
            }
        }
        return result
    }
    static func privileges(_ source: String) throws -> PrivilegeSet {
        var result = PrivilegeSet()
        for word in source.split(whereSeparator: { $0 == "," || $0.isWhitespace }) {
            switch word {
            case "ALL": result.formUnion(.allTables)
            case "NONE": break
            case "SELECT": result.insert(.select)
            case "INSERT": result.insert(.insert)
            case "UPDATE": result.insert(.update)
            case "DELETE": result.insert(.delete)
            case "EXECUTE": result.insert(.execute)
            default: throw JedError("XX000", "invalid corpus privilege directive")
            }
        }
        return result
    }
    static func grant(_ source: String) throws -> Grant {
        guard let divider = source.range(of: " ON ") else { throw JedError("XX000", "invalid corpus grant directive") }
        let mask = try privileges(String(source[..<divider.lowerBound]))
        let name = try SQLText(source[divider.upperBound...].trimmingCharacters(in: .whitespaces)).asciiLowercased
        return Grant(privileges: mask, name: name, function: mask.contains(.execute))
    }
    static func supports(_ expression: SQLExpression) -> Bool {
        switch expression.kind {
        case .number, .string, .boolean, .null, .parameter, .currentDate, .currentTimestamp: return true
        case .parenthesized(let e), .unary(_, let e), .isNull(let e, _): return supports(e)
        case .cast(let e, let type): return scalar(type) && supports(e)
        case .typedString(let type, _): return scalar(type)
        case .binary(let op, let a, let b):
            return ["+", "-", "*", "/", "%", "||", "=", "<>", "<", ">", "<=", ">=", "and", "or", "at time zone", "@>", "<@", "&&", "<<", ">>", "&<", "&>", "-|-", "->", "->>", "#>", "#>>", "?", "?|", "?&", "#-", "@?", "@@", "~", "~*", "!~", "!~*"].contains(op) && supports(a) && supports(b)
        case .pattern(_, let source, let pattern, let escape, _): return supports(source) && supports(pattern) && (escape.map(supports) ?? true)
        case .extract(_, let source): return supports(source)
        case .collate(let source, _): return supports(source)
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
        case .isJSON(let source, _, _, _): return supports(source)
        case .json(let expression):
            return supports(expression.context) && (expression.path.map(supports) ?? true) && expression.passing.allSatisfy { supports($0.value) } && (expression.onEmpty?.value.map(supports) ?? true) && (expression.onError?.value.map(supports) ?? true)
        case .call(let call):
            return connectedFunctions.contains(call.name.description) && !call.star && !call.distinct && call.orderBy.isEmpty && call.withinGroup.isEmpty && call.filter == nil && call.over == nil && call.overName == nil && call.arguments.allSatisfy { supports($0.value) }
        default: return false
        }
    }
    static func scalar(_ type: SQLTypeSyntax) -> Bool { (try? TypeRules.resolve(type, composites: [:])) != nil }
    static let connectedFunctions = TextFunctions.names.union([
        "clock_timestamp", "uuidv4", "gen_random_uuid", "uuidv7", "current_setting",
        "jsonb_path_exists", "jsonb_path_match", "jsonb_path_query_array", "jsonb_path_query_first", "jsonb_path_exists_tz", "jsonb_path_match_tz", "jsonb_path_query_array_tz", "jsonb_path_query_first_tz", "jsonb_set", "jsonb_insert", "jsonb_set_lax", "json_object", "jsonb_object", "row_to_json",
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
