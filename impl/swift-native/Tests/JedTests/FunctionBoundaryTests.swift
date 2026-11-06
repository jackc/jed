import Synchronization
import XCTest
@testable import Jed

final class FunctionBoundaryTests: XCTestCase {
    func testRegisteredHostOverloadsUseGenericPrecedenceThroughExpressionEvaluation() throws {
        let calls = Mutex([String]())
        var builder = Environment.builder()
        try builder.register(HostFunction(name: SQLText("sqrt"), arguments: [.i64], result: .i64, cost: 7, kernel: { values in
            calls.withLock { $0.append("integer") }; return values[0]
        }))
        try builder.register(HostFunction(name: SQLText("sqrt"), arguments: [.text], result: .text, kernel: { values in
            calls.withLock { $0.append("text") }; return values[0]
        }))
        // A host may register this signature, but an exact built-in wins it.
        try builder.register(HostFunction(name: SQLText("sqrt"), arguments: [.decimal], result: .decimal, kernel: { _ in
            calls.withLock { $0.append("decimal") }; return .decimal(SQLDecimal(99))
        }))
        let environment = builder.build(), resolver = ExpressionResolver()
        resolver.hostFunctions = environment.signatures
        var checked: [String] = []
        resolver.checkFunction = { checked.append($0.description) }
        func evaluate(_ sql: String) throws -> (SQLValue, Int64) {
            let expression = try resolver.resolve(SQLParser(sql).parseExpressionOnly()), meter = CostMeter()
            var execution = ExpressionContext(meter: meter)
            execution.functions.hostCall = { try environment.call($0, arguments: $1) }
            return (try execution.evaluate(expression), meter.accrued)
        }
        let exact = try evaluate("sqrt(4)")
        guard case .integer(4) = exact.0 else { return XCTFail("exact integer host must precede built-in decimal coercion") }
        XCTAssertEqual(exact.1, 7)
        guard case .text(let text) = try evaluate("sqrt('four')").0 else { return XCTFail("exact text host must precede literal fallback") }
        XCTAssertEqual(text.description, "four")
        guard case .decimal(let decimal) = try evaluate("sqrt(4.0)").0 else { return XCTFail("exact built-in expected") }
        XCTAssertEqual(decimal, SQLDecimal(2))
        XCTAssertTrue(try evaluate("sqrt(NULL::i64)").0.isNull)
        XCTAssertEqual(calls.withLock { $0 }, ["integer", "text"])
        XCTAssertEqual(checked, Array(repeating: "sqrt", count: 4))
    }
    private func bind(_ sql: String, resolver: ExpressionResolver = ExpressionResolver(), hosts: [HostFunctionSignature] = []) throws -> ResolvedFunction {
        let syntax = try SQLParser(sql).parseExpressionOnly()
        guard case .call(let call) = syntax.kind else { throw JedError("XX000", "test call expected") }
        return try FunctionResolver.resolve(call, hosts: hosts) { try resolver.resolve($0, expected: $1) }
    }
    func testHostCatalogPrecedenceAndCoercionStayIndependentOfRegistrationOrder() throws {
        let name = try SQLText("host_probe")
        let signatures = [HostFunctionSignature(id: 1, name: name, arguments: [.i64], result: .i64),
                          HostFunctionSignature(id: 2, name: name, arguments: [.i32], result: .i32),
                          HostFunctionSignature(id: 3, name: name, arguments: [.decimal], result: .decimal)]
        for hosts in [signatures, signatures.reversed().map { $0 }] {
            let resolved = try bind("host_probe(1::i16)", hosts: hosts)
            XCTAssertEqual(resolved.hostID, 2)
            XCTAssertEqual(resolved.arguments[0].type?.type, .scalar(.i32))
        }
        let sqrtHost = HostFunctionSignature(id: 4, name: try SQLText("sqrt"), arguments: [.i64], result: .i64)
        XCTAssertEqual(try bind("sqrt(2)", hosts: [sqrtHost]).hostID, 4)
        let sqrtDecimal = HostFunctionSignature(id: 5, name: try SQLText("sqrt"), arguments: [.decimal], result: .decimal)
        XCTAssertNil(try bind("sqrt(2)", hosts: [sqrtDecimal]).hostID)
        XCTAssertThrowsError(try bind("host_probe($1)", hosts: signatures)) { XCTAssertEqual(($0 as? JedError)?.code, "42883") }
    }
    func testCandidateSelectionDoesNotLeakParameterInference() throws {
        let resolver = ExpressionResolver()
        XCTAssertThrowsError(try bind("abs($1)", resolver: resolver)) { XCTAssertEqual(($0 as? JedError)?.code, "42725") }
        XCTAssertTrue(resolver.parameters.isEmpty)
        let selected = try bind("length($1)", resolver: resolver)
        XCTAssertEqual(resolver.parameters[1]?.type, .scalar(.text))
        XCTAssertEqual(selected.arguments[0].type?.type, .scalar(.text))
        let decimal = try bind("power($2,2)", resolver: resolver)
        XCTAssertEqual(resolver.parameters[2]?.type, .scalar(.decimal))
        XCTAssertEqual(decimal.resultType.type, .scalar(.decimal))
    }
    func testNamedMetadataDefaultsAndFloatingCastSchedule() throws {
        let function = try bind("make_interval(secs => 2, days => 3)")
        XCTAssertEqual(function.arguments.count, 7)
        XCTAssertEqual(function.evaluationOrder, Array(0..<7))
        XCTAssertEqual(function.arguments[6].type?.type, .scalar(.f64))
        let meter = CostMeter(), context = ExpressionContext(meter: meter)
        let values = try function.arguments.map { try context.evaluate($0) }
        guard case .interval(let interval) = try ScalarFunctions.evaluate(function, values: values, context: context) else { return XCTFail("interval expected") }
        XCTAssertEqual(interval.days, 3); XCTAssertEqual(interval.microseconds, 2_000_000)
        let widening = try bind("sqrt(4::f32)")
        if case .cast(_, _, .implicitCast) = widening.arguments[0].node {} else { XCTFail("f32 builtin argument must have a charged widening cast") }
        let coerced = try bind("sqrt(4)")
        if case .convert = coerced.arguments[0].node {} else { XCTFail("integer-to-decimal coercion must have no cast charge") }
    }
    func testHostNullDisciplineAndCallbackBoundary() throws {
        var calls = 0
        let signature = HostFunctionSignature(id: 8, name: try SQLText("host_probe"), arguments: [.i64], result: .i64)
        let function = try bind("host_probe(NULL::i64)", hosts: [signature])
        var environment = ScalarFunctions.Environment()
        environment.hostCall = { id, _ in calls += 1; XCTAssertEqual(id, 8); return .integer(9) }
        let result = try ScalarFunctions.evaluate(function, values: [.null], context: ExpressionContext(meter: CostMeter()), environment: environment)
        XCTAssertTrue(result.isNull); XCTAssertEqual(calls, 0)
        let nonnull = try bind("host_probe(1)", hosts: [signature])
        guard case .integer(9) = try ScalarFunctions.evaluate(nonnull, values: [.integer(1)], context: ExpressionContext(meter: CostMeter()), environment: environment) else { return XCTFail("host result expected") }
        XCTAssertEqual(calls, 1)
    }
}
