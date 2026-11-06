import XCTest
@testable import Jed

final class FunctionBoundaryTests: XCTestCase {
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
