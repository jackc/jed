import XCTest
@testable import Jed

final class HostBatchIntervalBoundaryTests: XCTestCase {
    func testReadAheadEndsBeforeNextIntervalChargeAndCloseDropsFutureIntervals() throws {
        let integer = TypeDescriptor(.scalar(.i32))
        func key(_ n: Int64) throws -> [UInt8] { try KeyEncoding.encode(.integer(n), type: integer) }
        var store = TableStore(tree: BPlusTree(codec: try PlainPageCodec(pageSize: 4096), types: []))
        for n in [Int64(1), 3, 5] { try store.put(PackedRecord(key: key(n), cells: []), netStaging: true) }
        let values = [Int64(1), 3, 5].map { ResolvedExpression(type: integer, node: .constant(.integer($0)), position: 0) }
        let array = ResolvedExpression(type: TypeDescriptor(.array(integer.type)), node: .array(values, nested: false), position: 0)
        let predicate = ResolvedExpression(type: TypeDescriptor(.scalar(.boolean)), node: .quantified("=", all: false,
            ResolvedExpression(type: integer, node: .column(0), position: 0), array, integer), position: 0)
        let plan = try XCTUnwrap(IntervalPlan.primary(predicate: predicate, keys: [0], types: [integer]))
        let meter = CostMeter(), context = ExpressionContext(meter: meter)
        let cursor = try IntervalCursor(plan: plan, table: store, index: nil, context: context, touched: [],
            access: plan.bind(context: context), incremental: true)
        XCTAssertEqual(meter.accrued, 0)
        XCTAssertTrue(try cursor.beginHostBatchInterval()); XCTAssertEqual(meter.accrued, 1)
        XCTAssertTrue(try cursor.beginHostBatchInterval()); XCTAssertEqual(meter.accrued, 1)
        XCTAssertEqual(try cursor.nextHostBatchRecord()?.key, try key(1)); XCTAssertEqual(meter.accrued, 1)
        XCTAssertNil(try cursor.nextHostBatchRecord()); XCTAssertEqual(meter.accrued, 1)
        XCTAssertNil(try cursor.nextHostBatchRecord()); XCTAssertEqual(meter.accrued, 1)
        XCTAssertTrue(try cursor.beginHostBatchInterval()); XCTAssertEqual(meter.accrued, 2)
        XCTAssertEqual(try cursor.nextHostBatchRecord()?.key, try key(3)); XCTAssertEqual(meter.accrued, 2)
        cursor.close(); cursor.close()
        XCTAssertFalse(try cursor.beginHostBatchInterval()); XCTAssertNil(try cursor.nextHostBatchRecord())
        XCTAssertNil(try cursor.next()); XCTAssertEqual(meter.accrued, 2)
    }
}
