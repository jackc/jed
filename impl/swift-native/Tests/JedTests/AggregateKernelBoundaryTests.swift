import XCTest
@testable import Jed

/// SQL corpus owns aggregate mathematics. These probes verify that the new
/// scalar/nullness entries preserve the general adapter's internal state,
/// restored spill state, failure boundaries, and retained-input ownership.
final class AggregateKernelBoundaryTests: XCTestCase {
    private func encoded(_ values: [SQLValue]) throws -> [UInt8] { try SpillRowCodec.encode(values) }
    private func state(_ plan: AggregatePlan, limit: Int64 = 0) -> (AggregateState, CostMeter) {
        let meter = CostMeter(limit: limit)
        return (plan.makeState(context: ExpressionContext(meter: meter)), meter)
    }
    private func assertCode(_ expected: String, _ action: () throws -> Void,
                            file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try action(), file: file, line: line) {
            XCTAssertEqual(($0 as? JedError)?.code, expected, file: file, line: line)
        }
    }

    func testScalarAndTupleEntriesShareTransitionsAndRestoredState() throws {
        let exact: [SQLValue] = [.decimal(try SQLDecimal.parse("1.25")), .null,
            .decimal(try SQLDecimal.parse("-2.5")), .decimal(try SQLDecimal.parse("4.75"))]
        let integral: [SQLValue] = [.integer(-7),.null,.integer(3),.integer(11)]
        let probes: [(String, ScalarType, [SQLValue])] = [
            ("count", .i32, integral), ("sum", .i16, integral), ("sum", .i32, integral),
            ("sum", .i64, integral), ("sum", .decimal, exact), ("avg", .decimal, exact),
            ("sum", .f32, [.f32(Float(1e20).bitPattern),.null,.f32(Float(-1e20).bitPattern),.f32(Float(3).bitPattern)]),
            ("avg", .f64, [.f64(Double(1e30).bitPattern),.null,.f64(Double(-1e30).bitPattern),.f64(Double(3).bitPattern)]),
            ("min", .text, [.text(try SQLText("é")),.null,.text(try SQLText("a")),.text(try SQLText("z"))]),
            ("max", .bytea, [.bytea([0,255]),.null,.bytea([0]),.bytea([255])]),
            ("bool_and", .boolean, [.boolean(true),.null,.boolean(false),.boolean(true)]),
            ("bit_xor", .i32, integral), ("var_pop", .decimal, exact),
            ("var_pop", .f64, [.f64(Double(1).bitPattern),.null,.f64(Double(2).bitPattern),.f64(Double(4).bitPattern)]),
        ]
        for (name, scalar, values) in probes {
            let plan = try AggregatePlan.resolve(name: name, inputTypes: [TypeDescriptor(.scalar(scalar))])
            let (tuple, tupleMeter) = state(plan), (single, singleMeter) = state(plan), (restored, restoredMeter) = state(plan)
            defer { tuple.close(); single.close(); restored.close() }
            for value in values.prefix(2) { try tuple.fold([value]); try single.foldScalar(value) }
            XCTAssertEqual(try encoded(single.packed()), try encoded(tuple.packed()), name)
            try restored.restore(tuple.packed())
            let prefixCost = tupleMeter.accrued
            for value in values.dropFirst(2) {
                try tuple.fold([value]); try single.foldScalar(value); try restored.foldScalar(value)
            }
            XCTAssertEqual(try encoded(single.packed()), try encoded(tuple.packed()), name)
            XCTAssertEqual(try encoded(restored.packed()), try encoded(tuple.packed()), name)
            XCTAssertEqual(try encoded([single.finish()]), try encoded([tuple.finish()]), name)
            XCTAssertEqual(try encoded([restored.finish()]), try encoded([tuple.finish()]), name)
            XCTAssertEqual(singleMeter.accrued, tupleMeter.accrued, name)
            XCTAssertEqual(restoredMeter.accrued, tupleMeter.accrued - prefixCost, name)
        }
    }

    func testNullnessStarArityClosedAndCountOverflowBoundaries() throws {
        let countPlan = try AggregatePlan.resolve(name: "count", inputTypes: [TypeDescriptor(.scalar(.bytea))])
        let starPlan = try AggregatePlan.resolve(name: "count", inputTypes: [], star: true)
        let (count, meter) = state(countPlan), (star, _) = state(starPlan)
        defer { count.close(); star.close() }
        try count.foldCount(isNull: true); try count.foldCount(isNull: false)
        try count.foldScalar(.bytea([])); try star.foldStar(); try star.fold([])
        XCTAssertEqual(try encoded([count.finish()]), try encoded([.integer(2)]))
        XCTAssertEqual(try encoded([star.finish()]), try encoded([.integer(2)]))
        XCTAssertEqual(meter.accrued, 0, "the caller owns aggregate_accumulate")
        assertCode("XX000") { try count.foldStar() }
        assertCode("XX000") { try star.foldScalar(.null) }
        assertCode("XX000") { try star.foldCount(isNull: false) }
        try count.restore([.integer(Int64.max)])
        try count.foldCount(isNull: true); try count.foldScalar(.null)
        assertCode("22003") { try count.foldCount(isNull: false) }
        assertCode("22003") { try count.foldScalar(.bytea([])) }
        XCTAssertEqual(try encoded(count.packed()), try encoded([.integer(Int64.max)]))
        count.close(); star.close()
        assertCode("55000") { try count.foldScalar(.null) }
        assertCode("55000") { try count.foldCount(isNull: true) }
        assertCode("55000") { try star.foldStar() }
        assertCode("55000") { try star.fold([.null]) }
    }

    func testOverflowAndDecimalCostGuardPrecedeTheSameStateMutation() throws {
        let intPlan = try AggregatePlan.resolve(name: "sum", inputTypes: [TypeDescriptor(.scalar(.i32))])
        let (tuple, _) = state(intPlan), (single, _) = state(intPlan)
        defer { tuple.close(); single.close() }
        let maximum: [SQLValue] = [.integer(Int64.max),.boolean(true)]
        try tuple.restore(maximum); try single.restore(maximum)
        assertCode("22003") { try tuple.fold([.integer(1)]) }
        assertCode("22003") { try single.foldScalar(.integer(1)) }
        XCTAssertEqual(try encoded(tuple.packed()), try encoded(maximum))
        XCTAssertEqual(try encoded(single.packed()), try encoded(maximum))

        let decimalPlan = try AggregatePlan.resolve(name: "sum", inputTypes: [TypeDescriptor(.scalar(.decimal))])
        let value = SQLValue.decimal(try SQLDecimal.parse("1234567890123456789012345678901234567890"))
        let (exactTuple, tupleMeter) = state(decimalPlan, limit: 1), (exactScalar, scalarMeter) = state(decimalPlan, limit: 1)
        defer { exactTuple.close(); exactScalar.close() }
        let previous: [SQLValue] = [value,.boolean(true)]
        try exactTuple.restore(previous); try exactScalar.restore(previous)
        assertCode("54P01") { try exactTuple.fold([value]) }
        assertCode("54P01") { try exactScalar.foldScalar(value) }
        XCTAssertGreaterThan(tupleMeter.accrued, 0); XCTAssertEqual(tupleMeter.accrued, scalarMeter.accrued)
        XCTAssertEqual(try encoded(exactTuple.packed()), try encoded(previous))
        XCTAssertEqual(try encoded(exactScalar.packed()), try encoded(previous))
    }

    func testScalarRetainedAdaptersKeepNullsAndReleaseTheirOwners() throws {
        let type = TypeDescriptor(.scalar(.i32))
        let collection = try AggregatePlan.resolve(name: "array_agg", inputTypes: [type])
        let ordered = try AggregatePlan.resolve(name: "mode", inputTypes: [type], withinGroup: [AggregateOrder(type: type)])
        for plan in [collection, ordered] {
            let tupleDiagnostics = MemoryDiagnostics(), scalarDiagnostics = MemoryDiagnostics()
            let tupleMeter = CostMeter(resources: StatementResources(queryLimit: 1024, diagnostics: tupleDiagnostics))
            let scalarMeter = CostMeter(resources: StatementResources(queryLimit: 1024, diagnostics: scalarDiagnostics))
            let tuple = plan.makeState(context: ExpressionContext(meter: tupleMeter))
            let single = plan.makeState(context: ExpressionContext(meter: scalarMeter))
            defer { tuple.close(); single.close() }
            for value in [SQLValue.integer(3),.null,.integer(3),.integer(7)] {
                try tuple.fold([value]); try single.foldScalar(value)
                XCTAssertEqual(tupleMeter.resources.query.used, scalarMeter.resources.query.used)
            }
            XCTAssertGreaterThan(tupleMeter.resources.query.used, 0)
            XCTAssertEqual(try encoded([tuple.finish()]), try encoded([single.finish()]))
            XCTAssertEqual(tupleMeter.accrued, scalarMeter.accrued)
            tuple.close(); single.close(); tuple.close(); single.close()
            XCTAssertEqual(tupleMeter.resources.query.used, 0); XCTAssertEqual(scalarMeter.resources.query.used, 0)
            XCTAssertEqual(tupleDiagnostics.underflows, 0); XCTAssertEqual(scalarDiagnostics.underflows, 0)
        }
    }
}
