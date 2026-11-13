import XCTest
@testable import Jed

final class RecordLeafBoundaryTests: XCTestCase {
    private let integer = TypeDescriptor(.scalar(.i32))
    private let text = TypeDescriptor(.scalar(.text))

    private func cell(_ value: SQLValue, _ type: TypeDescriptor) throws -> StoredCell {
        let bytes: [UInt8]?
        if value.isNull { bytes = nil }
        else if ValueCodec.fixedWidth(type.type) != nil { bytes = try ValueCodec.encodeBody(value, type: type) }
        else { bytes = LeafValue.bare(type.type) ? [5] + (try ValueCodec.encodeRawBody(value, type: type)) : try ValueCodec.encode(value, type: type) }
        return StoredCell(bytes: bytes?[...], type: type)
    }

    private func check(_ actual: SQLValue, _ expected: SQLValue, file: StaticString = #filePath, line: UInt = #line) {
        switch (actual, expected) {
        case (.integer(let a), .integer(let b)): XCTAssertEqual(a, b, file: file, line: line)
        case (.text(let a), .text(let b)): XCTAssertEqual(a, b, file: file, line: line)
        case (.bytea(let a), .bytea(let b)): XCTAssertEqual(a, b, file: file, line: line)
        default: XCTFail("unexpected stored value type", file: file, line: line)
        }
    }

    func testRecordLeavesSerializeToTheSamePlainAndEncryptedPage() throws {
        let codecs: [any PageCodec] = [try PlainPageCodec(pageSize: 256),
            try EncryptedPageCodec(pageSize: 1024, dataKey: Array(repeating: 7, count: 32))]
        let types = [integer, text], empty: [UInt8] = []
        let records = [
            PackedRecord(key: [1], cells: [try cell(.integer(42), integer), try cell(.text(SQLText("abc")), text)]),
            PackedRecord(key: [2], cells: [try cell(.null, integer), StoredCell(bytes: empty[...], type: text)]),
            PackedRecord(key: [3], cells: [try cell(.integer(-8), integer), try cell(.text(SQLText("")), text)])
        ]
        for codec in codecs {
            var tree = BPlusTree(codec: codec, types: types)
            for record in records { try tree.insert(record) }
            let node = try tree.resolve(XCTUnwrap(tree.root))
            XCTAssertNil(node.leaf, "writer records must not build synthetic packed pages")
            XCTAssertTrue(try XCTUnwrap(tree.get([2])).cells[1].isNull)
            let entropy: KeyEntropy = { Array(repeating: 0xA5, count: $0) }
            let first = codec.firstBodyPage
            var allocator = try PageAllocator(highWater: first, free: [], firstBody: first)
            let written = try tree.serialize(allocator: &allocator, fromScratch: true, entropy: entropy)
            let payload = try PackedLeaf.encodePayload(records, types: types, capacity: codec.capacity)
            let expected = try PageSealer.seal(codec: codec, index: first, type: 2,
                count: UInt32(records.count), payload: payload, entropy: entropy)
            XCTAssertEqual(written.pages.count, 1)
            XCTAssertEqual(written.pages[0].bytes, expected.page.bytes)
            XCTAssertEqual(written.root, expected.reference)

            let device = MemoryBlockStore()
            try device.write(at: UInt64(first) * UInt64(codec.pageSize), bytes: written.pages[0].bytes)
            let pool = try NodePool(codec: codec, cacheBytes: codec.pageSize) { reference in
                try codec.openBody(pageIndex: reference.index,
                    sealed: device.read(at: UInt64(reference.index) * UInt64(codec.pageSize), count: codec.pageSize),
                    expectedTag: reference.tag)
            }
            var opened = try BPlusTree.open(root: written.root, pageCount: allocator.highWater, types: types, pool: pool)
            var packed = TreeCursor(opened)
            let position = try XCTUnwrap(packed.nextPosition())
            let replacement = PackedRecord(key: [1], cells: [try cell(.integer(99), integer), try cell(.null, text)])
            try opened.insert(replacement)
            packed.close(); pool.invalidateAll()
            check(try position.materialize(0, pager: nil), .integer(42))
            check(try XCTUnwrap(opened.get([1])).materialize(column: 0, type: types[0]), .integer(99))
        }
    }

    func testMalformedCellsDoNotPublishAReplacementLeaf() throws {
        let codec = try PlainPageCodec(pageSize: 256), types = [integer, text]
        let original = PackedRecord(key: [1], cells: [try cell(.integer(42), integer), try cell(.text(SQLText("old")), text)])
        var store = TableStore(tree: BPlusTree(codec: codec, types: types))
        try store.put(original, netStaging: true)
        let saved = store, revision = store.revision
        var cursor = store.cursor()
        let position = try XCTUnwrap(cursor.nextPosition())
        let badFixed: [UInt8] = [0], badTag: [UInt8] = [1]
        let malformed = [
            PackedRecord(key: [1], cells: [StoredCell(bytes: badFixed[...], type: integer), original.cells[1]]),
            PackedRecord(key: [1], cells: [original.cells[0], StoredCell(bytes: badTag[...], type: text)])
        ]
        for record in malformed {
            XCTAssertThrowsError(try store.put(record, netStaging: true)) {
                XCTAssertEqual(($0 as? JedError)?.code, "XX001")
            }
            XCTAssertTrue(store.revision === revision)
            XCTAssertEqual(store.count, saved.count)
            XCTAssertEqual(store.pendingBytes, saved.pendingBytes)
            check(try XCTUnwrap(store.get([1])).materialize(column: 0, type: types[0]), .integer(42))
        }
        cursor.close()
        check(try position.materialize(1, pager: nil), .text(try SQLText("old")))
    }

    func testNominalInputDescriptorsUseTheTreesCurrentFields() throws {
        let codec = try PlainPageCodec(pageSize: 1024)
        let declared = try CompositeType(catalogID: 1, id: 7, name: SQLText("current"),
            fields: [TypeField(name: SQLText("n"), descriptor: integer)])
        let stale = try CompositeType(catalogID: 1, id: 7, name: SQLText("stale"),
            fields: [TypeField(name: SQLText("n"), descriptor: text)])
        let value = SQLValue.composite([.integer(42)])
        let cases: [(SQLType, SQLType, SQLValue)] = [
            (.composite(declared), .composite(stale), value),
            (.array(.composite(declared)), .array(.composite(stale)),
             .array(try SQLArray(dimensions: [1], lowerBounds: [1], elements: [value])))
        ]
        for (current, previous, value) in cases {
            let type = TypeDescriptor(current), oldType = TypeDescriptor(previous)
            XCTAssertEqual(type, oldType, "nominal equality admits this descriptor")
            let bytes = try ValueCodec.encode(value, type: type)
            var tree = BPlusTree(codec: codec, types: [type])
            try tree.insert(PackedRecord(key: [1], cells: [StoredCell(bytes: bytes[...], type: oldType)]))
            let stored = try XCTUnwrap(tree.get([1])).cells[0]
            XCTAssertEqual(try ValueCodec.encode(stored.materialize(type: type), type: type), bytes)
        }
    }

    func testPendingOverflowOutlivesReplacementDeletionAndCursorClose() throws {
        let codec = try PlainPageCodec(pageSize: 256), type = TypeDescriptor(.scalar(.bytea))
        var state: UInt64 = 19, raw: [UInt8] = []
        for _ in 0..<600 { state = state &* 6364136223846793005 &+ 1; raw.append(UInt8(truncatingIfNeeded: state >> 32)) }
        let original = try RecordDisposition.plan(key: [1], values: [.value(.bytea(raw))], types: [type], codec: codec)
        let replacement = try RecordDisposition.plan(key: [1], values: [.value(.bytea(Array(raw.reversed())))], types: [type], codec: codec)
        XCTAssertFalse(original.overflows.isEmpty)
        var store = TableStore(tree: BPlusTree(codec: codec, types: [type]))
        try store.put(original, netStaging: true)
        let saved = store
        var cursor = store.cursor()
        let position = try XCTUnwrap(cursor.nextPosition())
        try store.put(replacement, netStaging: true)
        XCTAssertEqual(store.pendingBytes, try TableStore.footprint(replacement, encrypted: false))
        try store.remove([1]); cursor.close()
        XCTAssertEqual(store.pendingBytes, 0)
        check(try position.materialize(0, pager: nil), .bytea(raw))
        check(try XCTUnwrap(saved.get([1])).materialize(column: 0, type: type), .bytea(raw))
        XCTAssertEqual(saved.pendingBytes, try TableStore.footprint(original, encrypted: false))
    }
}
