import XCTest
@testable import Jed

final class RecordDispositionTests: XCTestCase {
    private let text = TypeDescriptor(.scalar(.text))
    private let bytea = TypeDescriptor(.scalar(.bytea))

    func testKeptResidentOverflowAcceptsItsSealedReferenceAndValidatesCacheFacts() throws {
        let codec = try PlainPageCodec(pageSize: 256), payload = noise(601)
        let row = try RecordDisposition.plan(key: [1], values: [.value(.bytea(payload))], types: [bytea], codec: codec)
        var tree = BPlusTree(codec: codec, types: [bytea]); try tree.insert(row)
        var allocator = try PageAllocator(highWater: 2, free: [], firstBody: 2)
        let write = try tree.serialize(allocator: &allocator)
        let resident = try XCTUnwrap(write.tree.get(row.key))
        let pending = try XCTUnwrap(resident.overflows[0])
        let head = try XCTUnwrap(resident.cells[0].largeFacts(encrypted: false)?.external)
        XCTAssertGreaterThanOrEqual(head.index, codec.firstBodyPage)
        let kept = try RecordDisposition.plan(key: [2], values: [.kept(resident.cells[0], pending: pending)], types: [bytea], codec: codec)
        XCTAssertEqual(kept.cells[0].bytes, resident.cells[0].bytes)
        XCTAssertEqual(kept.overflows[0]?.bytes, pending.bytes)
        if case .bytea(let actual) = try kept.materialize(column: 0, type: bytea) { XCTAssertEqual(actual, payload) }
        else { XCTFail("wrong materialized value") }
        let wrong = PendingOverflow(bytes: pending.bytes, rawLength: pending.rawLength + 1, compressed: pending.compressed)
        XCTAssertThrowsError(try RecordDisposition.plan(key: [3], values: [.kept(resident.cells[0], pending: wrong)], types: [bytea], codec: codec)) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX001")
        }
        let invalid = StoredCell(bytes: try pending.pointer(PageReference(1))[...], type: bytea)
        XCTAssertThrowsError(try RecordDisposition.plan(key: [3], values: [.kept(invalid, pending: pending)], types: [bytea], codec: codec)) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX001")
        }
    }

    func testIrreducibleWideRowUsesExactLeafFitAndSplitsAtTwoRecords() throws {
        let codec = try PlainPageCodec(pageSize: 8192)
        let types = [TypeDescriptor](repeating: TypeDescriptor(.scalar(.i32)), count: 600)
        let values = [RecordInput](repeating: .value(.null), count: types.count)
        let first = try RecordDisposition.plan(key: [UInt8](repeating: 0, count: 8), values: values, types: types, codec: codec)
        var nextKey = first.key; nextKey[7] = 1
        let second = try RecordDisposition.plan(key: nextKey, values: values, types: types, codec: codec)
        XCTAssertEqual(RecordDisposition.recordMaximum(capacity: codec.capacity, columns: types.count), 0)
        XCTAssertEqual(first.weight, 2408)
        XCTAssertEqual(PackedLeaf.overhead(records: 1, types: types) + first.weight, 6016)
        var tree = BPlusTree(codec: codec, types: types)
        try tree.insert(first); try tree.insert(second)
        XCTAssertEqual(tree.height, 2); XCTAssertEqual(tree.nodeCount, 3)
        var allocator = try PageAllocator(highWater: 2, free: [], firstBody: 2)
        let output = try tree.serialize(allocator: &allocator, fromScratch: true)
        let pages = Dictionary(uniqueKeysWithValues: output.pages.map { ($0.index, $0.bytes) })
        let pool = try NodePool(codec: codec, cacheBytes: 8192) { reference in
            guard let bytes = pages[reference.index] else { throw JedError.corruption("missing test page") }
            return try codec.openBody(pageIndex: reference.index, sealed: bytes, expectedTag: reference.tag)
        }
        let reopened = try BPlusTree.open(root: output.root, rowCount: 2, pageCount: allocator.highWater, types: types, pool: pool)
        XCTAssertEqual(try reopened.get(first.key)?.weight, first.weight)
        XCTAssertEqual(try reopened.get(second.key)?.cells.count, 600)
        XCTAssertTrue(try reopened.get(second.key)!.cells.allSatisfy(\.isNull))
    }

    func testIrreducibleRowDefersActualLeafOverflowUntilSerialization() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        let types = [TypeDescriptor](repeating: TypeDescriptor(.scalar(.i32)), count: 20)
        let values = [RecordInput](repeating: .value(.null), count: types.count)
        let keyBytes = codec.capacity - PackedLeaf.overhead(records: 1, types: types) - 4 * types.count
        let exact = try RecordDisposition.plan(key: [UInt8](repeating: 1, count: keyBytes), values: values, types: types, codec: codec)
        let oversized = try RecordDisposition.plan(key: exact.key + [0], values: values, types: types, codec: codec)
        var tree = BPlusTree(codec: codec, types: types)
        try tree.insert(exact)
        let committed = tree
        try tree.insert(oversized)
        XCTAssertEqual(tree.count, 2)
        XCTAssertNotNil(try tree.get(oversized.key))
        var allocation = try PageAllocator(highWater: 2, free: [], firstBody: 2)
        XCTAssertThrowsError(try tree.serialize(allocator: &allocation)) { XCTAssertEqual(($0 as? JedError)?.code, "0A000") }
        XCTAssertEqual(allocation.highWater, 2)
        XCTAssertEqual(committed.count, 1)
        XCTAssertNotNil(try committed.get(exact.key)); XCTAssertNil(try committed.get(oversized.key))
    }

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
        if case .text(let result) = try row.cells[0].materialize(type: text) { XCTAssertEqual(result, try SQLText(String(repeating: "a", count: 70))) }
        else { XCTFail("wrong value") }
    }

    func testFreshOverflowSealsInColumnOrderAndKeptPointersNeedNoRead() throws {
        let codec = try PlainPageCodec(pageSize: 256), payload = noise(601), meter = CostMeter()
        let row = try RecordDisposition.plan(key: [1], values: [.value(.bytea(payload)), .value(.bytea(payload))],
            types: [bytea, bytea], codec: codec, meter: meter)
        XCTAssertEqual(row.cells.map { $0.bytes?.first }, [2, 2])
        XCTAssertEqual(meter.accrued, 6)
        if case .bytea(let materialized) = try row.materialize(column: 1, type: bytea) { XCTAssertEqual(materialized, payload) }
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
