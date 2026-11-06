import Synchronization
import XCTest
@testable import Jed

final class HostBatchBoundaryTests: XCTestCase {
    private func expression(_ host: HostFunction, arguments: [ResolvedExpression]? = nil) -> ResolvedExpression {
        let type = TypeDescriptor(.scalar(host.result))
        let arguments = arguments ?? [ResolvedExpression(type: TypeDescriptor(.scalar(.i64)), node: .column(0), position: 0)]
        let function = ResolvedFunction(metadata: FunctionSpecFact(category: "host", name: host.name.description,
            kind: "function", metadata: ["cost": .integer(host.cost)]), arguments: arguments, resultType: type,
            evaluationOrder: Array(arguments.indices), constantArguments: arguments.map(\.isConstant),
            hostID: 0, hostCallIdentity: HostCallIdentity())
        return ResolvedExpression(type: type, node: .call(function), position: 0)
    }
    private func context(_ host: HostFunction, meter: CostMeter = CostMeter()) -> ExpressionContext {
        var result = ExpressionContext(meter: meter)
        result.functions.hostFunctions = [host]
        result.functions.hostCall = { _, values in try host.evaluate(values) }
        return result
    }
    private func integer(_ value: SQLValue?) -> Int64? { if case .integer(let n) = value { n } else { nil } }

    func testPrefixFailureNullAndUnfetchedRowsReplayAtTheirOriginalRow() throws {
        let calls = Mutex<[Int]>([])
        let host = try HostFunction(name: SQLText("prefix"), arguments: [.i64], result: .i64, cost: 3, volatility: .stable,
            batchKernel: { columns, output in
                calls.withLock { $0.append(columns[0].count) }
                for value in columns[0] {
                    if case .integer(4) = value { throw JedError("22012", "host failure") }
                    output.append(value)
                }
            })
        let expr = expression(host), rows: [SQLValue] = [.integer(1), .null, .integer(3), .integer(4), .integer(5)]
        var context = context(host)
        let batch = try XCTUnwrap(HostBatch(expressions: [expr], context: context)); context.hostBatch = batch
        for row in 0..<4 {
            try batch.setRow(row, count: rows.count, context: context) { r, _ in r == 2 ? nil : rows[r] }
            XCTAssertEqual(context.meter.accrued, Int64(row * 3))
            context.column = { _ in rows[row] }
            if row == 3 {
                XCTAssertThrowsError(try context.evaluate(expr)) { XCTAssertEqual(($0 as? JedError)?.code, "22012") }
            } else {
                let result = try context.evaluate(expr)
                if row == 1 { XCTAssertTrue(result.isNull) } else { XCTAssertEqual(integer(result), Int64(row + 1)) }
            }
        }
        XCTAssertEqual(calls.withLock { $0 }, [3, 1])
        XCTAssertEqual(context.meter.accrued, 12)
    }

    func testMalformedPrefixAndResultTypeValidationAreDeferredToReplay() throws {
        for mode in 0..<4 {
            let host = try HostFunction(name: SQLText("bad"), arguments: [.i64], result: .i16, volatility: .immutable,
                batchKernel: { _, output in
                    switch mode {
                    case 0: output = [.integer(1)]
                    case 1: output = [.integer(1), .integer(2), .integer(3)]
                    case 2: output = [.integer(1), .integer(2)]; throw JedError("22012", "late error")
                    default: output = [.integer(1), .integer(100_000)]
                    }
                })
            let expr = expression(host); var context = context(host)
            let batch = try XCTUnwrap(HostBatch(expressions: [expr], context: context)); context.hostBatch = batch
            try batch.setRow(0, count: 2, context: context) { r, _ in .integer(Int64(r)) }
            XCTAssertEqual(context.meter.accrued, 0)
            if mode == 1 || mode == 2 {
                XCTAssertThrowsError(try context.evaluate(expr)) { XCTAssertEqual(($0 as? JedError)?.code, "22000") }
            } else {
                XCTAssertEqual(integer(try context.evaluate(expr)), 1)
                try batch.setRow(1, count: 2, context: context) { r, _ in .integer(Int64(r)) }
                XCTAssertThrowsError(try context.evaluate(expr)) { XCTAssertEqual(($0 as? JedError)?.code, mode == 3 ? "22003" : "22000") }
            }
        }
    }

    func testSpeculationRespectsStatementLifetimeAndInHandBounds() throws {
        let calls = Mutex<[Int]>([])
        let host = try HostFunction(name: SQLText("bounded"), arguments: [.i64], result: .i64, cost: 3, volatility: .stable,
            batchKernel: { columns, output in calls.withLock { $0.append(columns[0].count) }; output = columns[0] })
        for meter in [CostMeter(), CostMeter(limit: 7), CostMeter(limit: 100, lifetime: LifetimeCost(limit: 8, accrued: 5))] {
            let expr = expression(host), context = context(host, meter: meter)
            let batch = try XCTUnwrap(HostBatch(expressions: [expr], context: context))
            try batch.setRow(0, count: 2000, context: context) { r, _ in .integer(Int64(r)) }
            XCTAssertEqual(meter.accrued, 0)
        }
        XCTAssertEqual(calls.withLock { $0 }, [1024, 3, 2])
    }

    func testReadAheadDefersSourceErrorsAndResetDropsThem() throws {
        let ahead = HostReadAhead<Int>(); var pulls = 0
        func pull() throws -> Int? {
            pulls += 1
            if pulls == 3 { throw JedError("58030", "source failed") }
            return pulls
        }
        let first = try XCTUnwrap(ahead.next(cap: 64, pull: pull))
        XCTAssertEqual(ahead.rows[first], 1); XCTAssertEqual(pulls, 3)
        let second = try XCTUnwrap(ahead.next(cap: 64, pull: pull))
        XCTAssertEqual(ahead.rows[second], 2)
        XCTAssertThrowsError(try ahead.next(cap: 64, pull: pull)) { XCTAssertEqual(($0 as? JedError)?.code, "58030") }
        ahead.reset(); pulls = 0
        _ = try ahead.next(cap: 64, pull: pull)
        ahead.reset()
        XCTAssertNil(try ahead.next(cap: 64, pull: { nil }))
    }
}
