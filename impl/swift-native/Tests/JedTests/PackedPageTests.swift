import XCTest
@testable import Jed

final class PackedPageTests: XCTestCase {
    private let integer = TypeDescriptor(.scalar(.i32))
    private let text = TypeDescriptor(.scalar(.text))

    private func cell(_ value: SQLValue, _ type: TypeDescriptor) throws -> StoredCell {
        let bytes: [UInt8]?
        if value.isNull { bytes = nil }
        else if ValueCodec.fixedWidth(type.type) != nil { bytes = try ValueCodec.encodeBody(value, type: type) }
        else { bytes = try ValueCodec.encode(value, type: type) }
        return StoredCell(bytes: bytes?[...], type: type)
    }

    func testPackedRegionsPreserveBytesAndBorrowCells() throws {
        let codec = try PlainPageCodec(pageSize: 256), types = [integer, text]
        let rows = [
            PackedRecord(key: [1], cells: [try cell(.integer(42), integer), try cell(.text(SQLText("abc")), text)]),
            PackedRecord(key: [2], cells: [try cell(.null, integer), try cell(.null, text)]),
            PackedRecord(key: [3], cells: [try cell(.integer(-8), integer), try cell(.text(SQLText("")), text)])
        ]
        let payload = try PackedLeaf.encodePayload(rows, types: types, capacity: codec.capacity)
        XCTAssertEqual(payload.count, rows.reduce(PackedLeaf.overhead(records: 3, types: types)) { $0 + $1.weight })
        let page = try PageSealer.seal(codec: codec, index: 2, type: 2, count: 3, payload: payload)
        let leaf = try PackedLeaf(bytes: page.page.bytes, types: types, codec: codec)
        XCTAssertEqual(leaf.count, 3)
        XCTAssertEqual(leaf.payloadSize, payload.count)
        for row in rows.indices {
            XCTAssertEqual(Array(leaf.key(at: row)), rows[row].key)
            XCTAssertEqual(leaf.record(at: row).weight, rows[row].weight)
            for column in types.indices {
                XCTAssertEqual(leaf.cell(row: row, column: column).bytes, rows[row].cells[column].bytes)
            }
        }
        XCTAssertTrue(try leaf.cell(row: 1, column: 0).materialize().isNull)
        if case .text(let value) = try leaf.cell(row: 0, column: 1).materialize() {
            XCTAssertEqual(value.utf8, [97, 98, 99])
        } else { XCTFail("wrong decoded value") }
    }

    func testMalformedPackedDirectoriesAreRejectedBeforeValuesAreDecoded() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        let row = PackedRecord(key: [1], cells: [try cell(.text(SQLText("abc")), text)])
        let payload = try PackedLeaf.encodePayload([row], types: [text], capacity: codec.capacity)
        let valid = try PageSealer.seal(codec: codec, index: 2, type: 2, count: 1, payload: payload).page.bytes
        // Header count, key end, region start/end, flags, value end, and tail.
        for offset in [4, 16, 21, 25, 29, 30, 255] {
            var corrupt = valid; corrupt[offset] ^= 0x80
            try PageEnvelope.putU32(PageEnvelope.checksum(corrupt, at: 12), into: &corrupt, at: 12)
            XCTAssertThrowsError(try PackedLeaf(bytes: corrupt, types: [text], codec: codec), "offset \(offset)") {
                XCTAssertEqual(($0 as? JedError)?.code, "XX001")
            }
        }
    }

    func testBorrowedCellMaterializationKeepsFixedAndInlineExtents() throws {
        let integerBacking: [UInt8] = [0xff, 0x80, 0, 0, 42, 0xee]
        let textBacking: [UInt8] = [0xff, 0, 0, 3, 97, 98, 99, 0xee]
        guard case .integer(let number) = try StoredCell(bytes: integerBacking[1..<5], type: integer).materialize(),
              case .text(let value) = try StoredCell(bytes: textBacking[1..<7], type: text).materialize() else {
            return XCTFail("wrong decoded cell values")
        }
        XCTAssertEqual(number, 42)
        XCTAssertEqual(value.utf8, [97, 98, 99])
        for (type, backing) in [(integer, integerBacking), (text, textBacking)] {
            let end = backing.count - 1
            for cut in 1..<end {
                XCTAssertThrowsError(try StoredCell(bytes: backing[1..<cut], type: type).materialize()) {
                    XCTAssertEqual(($0 as? JedError)?.code, "XX001")
                }
            }
            XCTAssertThrowsError(try StoredCell(bytes: backing[1...], type: type).materialize()) {
                XCTAssertEqual(($0 as? JedError)?.code, "XX001")
            }
        }
        XCTAssertTrue(try StoredCell(bytes: nil, type: text).materialize().isNull)
        // Leaf NULLs have no bytes; the single-value NULL tag is not a leaf cell.
        let taggedNull: [UInt8] = [0xff, 1, 0xee]
        XCTAssertThrowsError(try StoredCell(bytes: taggedNull[1..<2], type: text).materialize()) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX001")
        }
    }

    func testOverflowAllocationChainOrderAndExactSlabValidation() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        var allocator = try PageAllocator(highWater: 10, free: [3, 7], firstBody: 2)
        let payload = (0..<601).map { UInt8(truncatingIfNeeded: $0) }
        let written = try OverflowChain.write(payload, allocator: &allocator, codec: codec)
        XCTAssertEqual(written.pages.map(\.index), [3, 7, 10])
        XCTAssertEqual(written.head.index, 3)
        XCTAssertEqual(allocator.highWater, 11)
        var pages = Dictionary(uniqueKeysWithValues: written.pages.map { ($0.index, $0.bytes) })
        let read: (PageReference) throws -> [UInt8] = { reference in
            try codec.openBody(pageIndex: reference.index, sealed: XCTUnwrap(pages[reference.index]), expectedTag: reference.tag)
        }
        XCTAssertEqual(try OverflowChain.read(written.head, length: payload.count, pageCount: 11, codec: codec, read: read), payload)
        XCTAssertThrowsError(try OverflowChain.read(written.head, length: payload.count - 1, pageCount: 11, codec: codec, read: read))
        var corrupt = try XCTUnwrap(pages[7])
        try PageEnvelope.putU32(3, into: &corrupt, at: 8)
        try PageEnvelope.putU32(PageEnvelope.checksum(corrupt, at: 12), into: &corrupt, at: 12)
        pages[7] = corrupt
        XCTAssertThrowsError(try OverflowChain.read(written.head, length: payload.count, pageCount: 11, codec: codec, read: read))
    }

    func testMonotoneManifestPlanningKeepsReservationsAndSkipsUnsafeFreePages() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        let persisted: [UInt32] = Array(2..<122)
        let safe: [UInt32] = [3, 7, 10]
        let planned = try CommitAllocation.plan(persist: persisted, safe: safe, highWater: 130,
            dirtyCount: 17, pendingCount: 0, truncate: false, codec: codec)
        XCTAssertEqual(planned.manifestIDs, [3])
        XCTAssertEqual(planned.freeList.pages.map(\.index), [7, 10])
        XCTAssertFalse(planned.freeList.entries.contains(3))
        XCTAssertFalse(planned.freeList.entries.contains(7))
        XCTAssertFalse(planned.freeList.entries.contains(10))
        XCTAssertEqual(planned.freeList.highWater, 130)
        let trimmed = PageAllocator.trimTrailing(safe: [4, 7, 8, 9], highWater: 10)
        XCTAssertEqual(trimmed.safe, [4]); XCTAssertEqual(trimmed.highWater, 7)
        var exhausted = try PageAllocator(highWater: .max, free: [], firstBody: 2)
        XCTAssertThrowsError(try exhausted.allocate()) { XCTAssertEqual(($0 as? JedError)?.code, "54000") }
    }
}
