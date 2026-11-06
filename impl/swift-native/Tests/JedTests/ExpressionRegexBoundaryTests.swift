import XCTest
@testable import Jed

final class ExpressionRegexBoundaryTests: XCTestCase {
    func testPreparedFunctionOwnsCompileChargePerExecutionAndReusesSearchScratch() throws {
        let resolver = ExpressionResolver(columns: [ExpressionColumn(name: try SQLText("s"), qualifier: nil,
            type: TypeDescriptor(.scalar(.text)), index: 0)])
        for sql in ["regexp_count(s,'a+')", "substring(s,'(a+)')", "substring(s,'%a%','#')"] {
            let expression = try resolver.resolve(SQLParser(sql).parseExpressionOnly())
            guard case .call(let call) = expression.node, let prepared = call.regex else { return XCTFail("regex preparation expected") }
            let compile = Int64(prepared.program?.instructionCount ?? prepared.similar!.instructionCount)
            let meter = CostMeter(), text = try SQLText("aaa")
            let first = ExpressionContext(meter: meter, column: { _ in .text(text) })
            _ = try first.evaluate(expression)
            let initial = meter.accrued
            _ = try first.evaluate(expression)
            XCTAssertEqual(meter.accrued - initial, initial - compile, sql)
            let copied = first
            XCTAssertTrue(first.patterns.functionState(for: prepared) === copied.patterns.functionState(for: prepared))
            let secondMeter = CostMeter(), second = ExpressionContext(meter: secondMeter, column: { _ in .text(text) })
            _ = try second.evaluate(expression)
            XCTAssertEqual(secondMeter.accrued, initial, sql)
            XCTAssertFalse(first.patterns.functionState(for: prepared) === second.patterns.functionState(for: prepared))
        }
    }

    func testLiteralPreparationAndDynamicFlagErrorsRetainTheirPhases() throws {
        let resolver = ExpressionResolver()
        XCTAssertThrowsError(try resolver.resolve(SQLParser("regexp_count(NULL,'[',0)").parseExpressionOnly())) {
            XCTAssertEqual(($0 as? JedError)?.code, "2201B")
        }
        let dynamic = ExpressionResolver(columns: [ExpressionColumn(name: try SQLText("f"), qualifier: nil,
            type: TypeDescriptor(.scalar(.text)), index: 0)])
        let expression = try dynamic.resolve(SQLParser("regexp_count('x','[',0,f)").parseExpressionOnly())
        let execution = ExpressionContext(meter: CostMeter(), column: { _ in .text(SQLText(validatedUTF8: [])) })
        XCTAssertThrowsError(try execution.evaluate(expression)) { XCTAssertEqual(($0 as? JedError)?.code, "22023") }
        let null = try resolver.resolve(SQLParser("substring(NULL,'[','#')").parseExpressionOnly())
        XCTAssertTrue(try execution.evaluate(null).isNull)
    }
}
