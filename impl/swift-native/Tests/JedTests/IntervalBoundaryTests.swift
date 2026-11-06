import XCTest
@testable import Jed

final class IntervalBoundaryTests: XCTestCase {
    private let integer = TypeDescriptor(.scalar(.i32))
    private func points(_ values: [Int64], column: Int) -> ResolvedExpression {
        let elements = values.map { ResolvedExpression(type: integer, node: .constant(.integer($0)), position: 0) }
        let array = ResolvedExpression(type: TypeDescriptor(.array(integer.type)), node: .array(elements, nested: false), position: 0)
        return ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)), node: .quantified("=", all: false,
            ResolvedExpression(type: integer, node: .column(column), position: 0), array, integer), position: 0)
    }
    private func key(_ value: Int64) throws -> [UInt8] { try KeyEncoding.encode(.integer(value), type: integer) }

    func testCanonicalByteAdjacencyAndOpenBoundary() throws {
        let first = ScanInterval(lower: TreeBound([0, 252]), upper: TreeBound([0, 253]))
        let second = ScanInterval(lower: TreeBound([0, 254]), upper: TreeBound([0, 255]))
        let fixed = IntervalPlan.canonical([second, first, first], adjacent: true)
        XCTAssertEqual(fixed.count, 1)
        XCTAssertEqual(fixed[0].lower?.key, [0, 252]); XCTAssertEqual(fixed[0].upper?.key, [0, 255])
        XCTAssertEqual(IntervalPlan.canonical([second, first], adjacent: false).count, 2)
        // Canonical adjacency uses prefix-successor, which truncates carry
        // bytes. It is deliberately not a same-width integer successor.
        let carry = ScanInterval(lower: TreeBound([1, 0]), upper: TreeBound([1, 2]))
        XCTAssertEqual(OrderedIndexPlan.successor([0, 255]), [1])
        XCTAssertEqual(IntervalPlan.canonical([second, carry], adjacent: true).count, 2)
        let openLeft = ScanInterval(lower: nil, upper: TreeBound([8], inclusive: false))
        let openRight = ScanInterval(lower: TreeBound([8], inclusive: false), upper: nil)
        XCTAssertEqual(IntervalPlan.canonical([openRight, openLeft], adjacent: true).count, 2)
        let touching = ScanInterval(lower: TreeBound([8]), upper: nil)
        XCTAssertEqual(IntervalPlan.canonical([touching, openLeft], adjacent: false).count, 1)
    }

    func testArrayIndexSuffixRetainsValidWireDimensions() throws {
        let type = TypeDescriptor(.array(integer.type))
        let value = SQLValue.array(try SQLArray(dimensions: Array(repeating: 1, count: 7),
            lowerBounds: Array(repeating: 1, count: 7), elements: [.integer(1)]))
        let suffix = try key(42)
        for option: UInt8 in 0...3 {
            let slot = try KeyEncoding.encodeSlot(value, type: type,
                descending: option & 1 != 0, nullsFirst: option & 2 != 0)
            XCTAssertEqual(try IndexKeyBoundary.suffix(slot + suffix, types: [type], options: [option]), suffix)
            XCTAssertThrowsError(try IndexKeyBoundary.suffix(Array(slot.dropLast()), types: [type], options: [option])) {
                XCTAssertEqual(($0 as? JedError)?.code, "XX001")
            }
        }
    }

    func testTupleProductCapWidensBeforeFollowingMember() throws {
        let a = points((0..<257).map { Int64($0 * 2) }, column: 0)
        let b = points((0..<256).map(Int64.init), column: 1)
        let predicate = try XCTUnwrap(QueryEstimate.and([a, b]))
        let plan = try XCTUnwrap(IntervalPlan.primary(predicate: predicate, keys: [0, 1], types: [integer, integer]))
        let access = try plan.bind(context: ExpressionContext(meter: CostMeter()))
        let intervals = try XCTUnwrap(access.intervals)
        XCTAssertEqual(intervals.count, 257)
        for (ordinal, interval) in intervals.enumerated() {
            let prefix = try key(Int64(ordinal * 2))
            XCTAssertEqual(interval.lower?.key, prefix)
            XCTAssertEqual(interval.upper?.key, OrderedIndexPlan.successor(prefix))
            XCTAssertEqual(interval.upper?.inclusive, false)
        }
    }

    func testIntervalStartCostAndCloseAreRetained() throws {
        let codec = try PlainPageCodec(pageSize: 4096)
        var store = TableStore(tree: BPlusTree(codec: codec, types: []))
        for value in [Int64(1), 3, 5] { try store.put(PackedRecord(key: key(value), cells: []), netStaging: true) }
        let plan = try XCTUnwrap(IntervalPlan.primary(predicate: points([5, 1, 3, 3], column: 0), keys: [0], types: [integer]))
        let meter = CostMeter(), context = ExpressionContext(meter: meter)
        let access = try plan.bind(context: context)
        let cursor = try IntervalCursor(plan: plan, table: store, index: nil, context: context, touched: [], access: access, incremental: true)
        XCTAssertEqual(meter.accrued, 0)
        XCTAssertEqual(try cursor.next()?.key, try key(1)); XCTAssertEqual(meter.accrued, 1)
        cursor.close(); XCTAssertNil(try cursor.next()); XCTAssertEqual(meter.accrued, 1)
        let eagerMeter = CostMeter(), eager = try IntervalCursor(plan: plan, table: store, index: nil,
            context: ExpressionContext(meter: eagerMeter), touched: [], access: access)
        XCTAssertEqual(eagerMeter.accrued, 3)
        eager.close(); XCTAssertEqual(eagerMeter.accrued, 3)
    }
}
