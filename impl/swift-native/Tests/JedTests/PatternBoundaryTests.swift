import XCTest
@testable import Jed

final class PatternBoundaryTests: XCTestCase {
    func testPreparedPatternChargesOncePerExecutionAndCopiesShareScratchOwnership() throws {
        let resolver = ExpressionResolver(columns: [ExpressionColumn(name: try SQLText("s"), qualifier: nil,
            type: TypeDescriptor(.scalar(.text)), index: 0)])
        let expression = try resolver.resolve(SQLParser("s ~ '(a|b)+'").parseExpressionOnly())
        guard case .pattern(let resolved) = expression.node, let program = resolved.program else { return XCTFail("literal must precompile") }
        let subject = try SQLText("ab"), kernel = CostMeter()
        _ = try program.isMatch(subject, meter: kernel)
        let perRow = kernel.accrued + 1, compile = Int64(program.instructionCount)
        let meter = CostMeter()
        var first = ExpressionContext(meter: meter, column: { _ in .null })
        XCTAssertTrue(try first.evaluate(expression).isNull)
        XCTAssertEqual(meter.accrued, 1) // NULL skips both compilation charge and VM.
        first.column = { _ in .text(subject) }
        _ = try first.evaluate(expression)
        XCTAssertEqual(meter.accrued, 1 + compile + perRow)
        let copied = first
        XCTAssertTrue(first.patterns.state(for: resolved) === copied.patterns.state(for: resolved))
        _ = try copied.evaluate(expression)
        XCTAssertEqual(meter.accrued, 1 + compile + 2 * perRow)
        let nextMeter = CostMeter(), next = ExpressionContext(meter: nextMeter, column: { _ in .text(subject) })
        _ = try next.evaluate(expression)
        XCTAssertEqual(nextMeter.accrued, compile + perRow)
        XCTAssertFalse(first.patterns.state(for: resolved) === next.patterns.state(for: resolved))
    }

    func testDynamicPatternCompilesOnEveryNonNullRowAndErrorsKeepTheirPhase() throws {
        let text = TypeDescriptor(.scalar(.text))
        let resolver = ExpressionResolver(columns: [
            ExpressionColumn(name: try SQLText("s"), qualifier: nil, type: text, index: 0),
            ExpressionColumn(name: try SQLText("p"), qualifier: nil, type: text, index: 1)])
        let expression = try resolver.resolve(SQLParser("s ~ p").parseExpressionOnly())
        guard case .pattern(let resolved) = expression.node else { return XCTFail("pattern node expected") }
        XCTAssertNil(resolved.program)
        let meter = CostMeter(), subject = try SQLText("ab"), pattern = try SQLText("a+")
        let execution = ExpressionContext(meter: meter, column: { .text($0 == 0 ? subject : pattern) })
        _ = try execution.evaluate(expression)
        let row = meter.accrued
        _ = try execution.evaluate(expression)
        XCTAssertEqual(meter.accrued, 2 * row)
        let malformed = try SQLText("[")
        let skipped = ExpressionContext(meter: CostMeter(), column: { $0 == 0 ? .null : .text(malformed) })
        XCTAssertTrue(try skipped.evaluate(expression).isNull)
        XCTAssertThrowsError(try resolver.resolve(SQLParser("NULL ~ '['").parseExpressionOnly())) {
            XCTAssertEqual(($0 as? JedError)?.code, "2201B")
        }
        // Compile errors are preparation errors even when execution would skip
        // the node. A valid skipped node incurs no compile or VM charge.
        let short = try resolver.resolve(SQLParser("false AND s ~ 'a+'").parseExpressionOnly())
        let skipMeter = CostMeter(), skip = ExpressionContext(meter: skipMeter, column: { _ in throw JedError("XX000", "unexpected row access") })
        _ = try skip.evaluate(short)
        XCTAssertEqual(skipMeter.accrued, 1)
    }
}
