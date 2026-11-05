import XCTest
@testable import Jed

final class RecordDispositionTests: XCTestCase {
    private let text = TypeDescriptor(.scalar(.text))
    private let bytea = TypeDescriptor(.scalar(.bytea))

    private func noise(_ count: Int) -> [UInt8] {
        var state: UInt32 = 42
        return (0..<count).map { _ in
            state ^= state << 13; state ^= state >> 17; state ^= state << 5
            return UInt8(truncatingIfNeeded: state)
        }
    }

    func testInlinePlainDoesNotCompressAndFixedNullStillHasAWeight() throws {
        let codec = try PlainPageCodec(pageSize: 256), meter = CostMeter()
        let row = try RecordDisposition.plan(key: [0], values: [.value(.text(SQLText(String(repeating: "a", count: 60)))), .value(.null)],
            types: [text, TypeDescriptor(.scalar(.i64))], codec: codec, meter: meter)
        XCTAssertEqual(row.cells[0].bytes?.first, 0)
        XCTAssertEqual(row.weight, 72)
        XCTAssertTrue(row.cells[1].isNull)
        XCTAssertEqual(meter.accrued, 0)
    }

    func testCompressionStopsAtFitAndUsesColumnOrderToBreakTies() throws {
        let codec = try PlainPageCodec(pageSize: 256), meter = CostMeter()
        let input = SQLValue.text(try SQLText(String(repeating: "a", count: 70)))
        let row = try RecordDisposition.plan(key: [0], values: [.value(input), .value(input)], types: [text, text], codec: codec, meter: meter)
        XCTAssertEqual(row.cells.map { $0.bytes?.first }, [3, 0])
        XCTAssertEqual(meter.accrued, 1)
        XCTAssertLessThanOrEqual(row.weight, RecordDisposition.recordMaximum(capacity: codec.capacity, columns: 2))
        if case .text(let result) = try row.cells[0].materialize() { XCTAssertEqual(result, try SQLText(String(repeating: "a", count: 70))) }
        else { XCTFail("wrong value") }
    }

    func testFreshOverflowSealsInColumnOrderAndKeptPointersNeedNoRead() throws {
        let codec = try PlainPageCodec(pageSize: 256), payload = noise(601), meter = CostMeter()
        let row = try RecordDisposition.plan(key: [1], values: [.value(.bytea(payload)), .value(.bytea(payload))],
            types: [bytea, bytea], codec: codec, meter: meter)
        XCTAssertEqual(row.cells.map { $0.bytes?.first }, [2, 2])
        XCTAssertEqual(meter.accrued, 6)
        if case .bytea(let materialized) = try row.materialize(column: 1) { XCTAssertEqual(materialized, payload) }
        else { XCTFail("wrong value") }
        var allocator = try PageAllocator(highWater: 10, free: [3, 7], firstBody: 2)
        let sealed = try row.sealOverflow(allocator: &allocator, codec: codec)
        XCTAssertEqual(sealed.pages.map(\.index), [3, 7, 10, 11, 12, 13])
        XCTAssertTrue(sealed.record.overflows.isEmpty)
        XCTAssertEqual(try sealed.record.cells[0].largeFacts(encrypted: false)?.external?.index, 3)
        XCTAssertEqual(try sealed.record.cells[1].largeFacts(encrypted: false)?.external?.index, 11)
        let keptMeter = CostMeter()
        let kept = try RecordDisposition.plan(key: [1], values: sealed.record.cells.map { .kept($0) },
            types: [bytea, bytea], codec: codec, meter: keptMeter)
        XCTAssertEqual(keptMeter.accrued, meter.accrued)
        XCTAssertEqual(kept.cells.map { $0.bytes }, sealed.record.cells.map { $0.bytes })
        XCTAssertTrue(kept.overflows.isEmpty)
    }

    func testCompressionGuardAndEncryptedQuantum() throws {
        let codec = try EncryptedPageCodec(pageSize: 1024, dataKey: [UInt8](repeating: 7, count: 32))
        let payload = noise(codec.capacity), meter = CostMeter()
        let row = try RecordDisposition.plan(key: [1], values: [.value(.bytea(payload))], types: [bytea], codec: codec, meter: meter)
        XCTAssertEqual(meter.accrued, 1) // C bytes, although an encrypted chain slab is C - 16.
        XCTAssertEqual(row.cells[0].storedSize, 25)
        let blocked = CostMeter(limit: 1)
        XCTAssertThrowsError(try RecordDisposition.plan(key: [1], values: [.value(.bytea(payload))], types: [bytea], codec: codec, meter: blocked)) {
            XCTAssertEqual(($0 as? JedError)?.code, "54P01")
        }
    }
}
