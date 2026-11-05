import Foundation
import XCTest
@testable import Jed

final class GistBoundaryTests: XCTestCase {
    private func key(_ number: Int) -> [UInt8] { var writer = ByteWriter(); writer.u32(UInt32(number)); return writer.bytes }
    private func span(_ lower: Int64, _ upper: Int64) -> SQLRange {
        SQLRange(lower: .integer(lower), upper: .integer(upper))
    }
    private func op(_ type: SQLType) throws -> GistOpclass { try GistOpclass(type: TypeDescriptor(type)) }
    private func assertCode(_ code: String = "XX001", _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }

    func testAllFrozenGistGoldensRebuildByteExactly() throws {
        var indexes = 0
        for name in ["gist_range_table", "gist_scalar_table", "gist_exclude_table", "exclude_followon_table", "encrypted_index_table"] {
            let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures"))
            let bytes = [UInt8](try Data(contentsOf: url)), store = MemoryBlockStore()
            try store.write(at: 0, bytes: bytes)
            let encrypted = name.hasPrefix("encrypted_")
            let pager = try Pager.open(store: store, credential: encrypted ? .hostKey([UInt8](repeating: 0x4a, count: 32)) : nil)
            let catalog = try CatalogChain.read(root: pager.selected.meta.root, pageCount: pager.selected.meta.pageCount,
                codec: pager.codec, read: pager.readBody)
            let image = try CatalogImage.collect(catalog.entries, catalogID: 1), types = try image.resolvedTypes(catalogID: 1)
            for table in image.tables {
                for index in table.indexes where index.kind == 2 {
                    indexes += 1
                    let ops = try index.keys.map { column -> GistOpclass in
                        guard case .column(let number) = column else { throw JedError.corruption("unexpected expression GiST golden") }
                        return try GistOpclass(type: table.columns[number].type.resolve(types))
                    }
                    let opened = try GistTree.open(root: index.root, pageCount: pager.selected.meta.pageCount,
                        opclasses: ops, codec: pager.codec, read: pager.readBody)
                    var allocator = try PageAllocator(highWater: pager.selected.meta.pageCount,
                        free: opened.pages.sorted(), firstBody: pager.codec.firstBodyPage)
                    var salts = opened.pages.map { page in
                        let end = (Int(page) + 1) * pager.codec.pageSize
                        return Array(bytes[(end - 40)..<(end - 16)])
                    }.makeIterator()
                    let rebuilt = try opened.tree.serialize(allocator: &allocator, codec: pager.codec,
                        entropy: encrypted ? { count in XCTAssertEqual(count, 24); return try XCTUnwrap(salts.next()) } : nil)
                    XCTAssertEqual(rebuilt.root, index.root, name)
                    XCTAssertEqual(rebuilt.pages.map(\.index), opened.pages, name)
                    for page in rebuilt.pages {
                        let start = Int(page.index) * pager.codec.pageSize
                        XCTAssertEqual(page.bytes, Array(bytes[start..<(start + pager.codec.pageSize)]), "\(name):\(page.index)")
                    }
                    let fromKeys = try GistTree.build(opclasses: ops, leafKeys: Array(opened.tree.leafKeys().reversed()))
                    XCTAssertEqual(fromKeys.count, opened.tree.count)
                    XCTAssertEqual(try fromKeys.leafKeys(), try opened.tree.leafKeys())
                }
            }
        }
        XCTAssertGreaterThanOrEqual(indexes, 4)
    }

    func testEveryAdmittedOpclassAndTupleFraming() throws {
        let scalars: [(ScalarType, SQLValue)] = [(.i16, .integer(-2)), (.i32, .integer(-3)), (.i64, .integer(.max)),
            (.boolean, .boolean(true)), (.uuid, .uuid(try SQLUUID(bytes: Array(0..<16)))), (.date, .date(-5)),
            (.timestamp, .timestamp(-100)), (.timestamptz, .timestamptz(200))]
        let ranges: [(ScalarType, SQLValue, SQLValue)] = [(.i32, .integer(1), .integer(5)), (.i64, .integer(2), .integer(9)),
            (.decimal, .decimal(try SQLDecimal.parse("1.20")), .decimal(try SQLDecimal.parse("9.90"))),
            (.timestamp, .timestamp(4), .timestamp(8)), (.timestamptz, .timestamptz(3), .timestamptz(7)),
            (.date, .date(2), .date(6))]
        var ops: [GistOpclass] = [], bounds: [GistBound] = []
        for (scalar, value) in scalars {
            let current = try op(.scalar(scalar)); ops.append(current)
            bounds.append(try XCTUnwrap(current.leafBound(value)))
            XCTAssertNil(try current.leafBound(.null))
        }
        for (scalar, lower, upper) in ranges {
            let current = try op(.range(.scalar(scalar))); ops.append(current)
            bounds.append(.range(SQLRange(lower: lower, upper: upper)))
        }
        let entry = GistEntry(bound: bounds, storageKey: [0, 1, 2, 0, 255])
        let bytes = try entry.leafKey(opclasses: ops), decoded = try GistEntry.decode(bytes, opclasses: ops)
        XCTAssertEqual(decoded.storageKey, entry.storageKey)
        XCTAssertEqual(try decoded.leafKey(opclasses: ops), bytes)
        for scalar in [ScalarType.text, .bytea, .decimal, .interval] {
            assertCode("0A000") { _ = try self.op(.scalar(scalar)) }
        }
        for type in [SQLType.scalar(.f32), .scalar(.f64), .scalar(.json), .scalar(.jsonb), .scalar(.jsonpath), .array(.scalar(.i32))] {
            assertCode("42704") { _ = try self.op(type) }
        }
        var truncated = bytes; truncated.removeLast(entry.storageKey.count + 1)
        assertCode { _ = try GistEntry.decode(truncated, opclasses: ops) }
    }

    func testCanonicalTupleDescentAndSnapshotRebuild() throws {
        let scalar = try op(.scalar(.i32)), range = try op(.range(.scalar(.i32))), ops = [scalar, range]
        let entries = try (0..<80).map { i in
            GistEntry(bound: [try XCTUnwrap(scalar.leafBound(.integer(Int64(i % 3)))), .range(span(Int64(i), Int64(i + 5)))], storageKey: key(i))
        }
        let tree = try GistTree.build(opclasses: ops, entries: entries)
        let reversed = try GistTree.build(opclasses: ops, entries: Array(entries.reversed()))
        XCTAssertEqual(try tree.leafKeys(), try reversed.leafKeys())
        let query: [GistQuery] = [try XCTUnwrap(scalar.query(.integer(1))), .range(span(20, 30))]
        let found = try tree.search(queries: query, strategies: [.equal, .overlaps])
        XCTAssertEqual(found.storageKeys, [16, 19, 22, 25, 28].map(key))
        let again = try reversed.search(queries: query, strategies: [.equal, .overlaps])
        XCTAssertEqual(found.nodesVisited, again.nodesVisited)
        XCTAssertEqual(found.interiorsVisited, again.interiorsVisited)
        XCTAssertGreaterThan(found.interiorsVisited, 0); XCTAssertLessThan(found.nodesVisited, tree.nodeCount)
        let contained = try tree.search(queries: [query[0], .range(span(20, 22))], strategies: [.equal, .contains])
        XCTAssertEqual(contained.storageKeys, [19].map(key))
        let removed = try GistTree.build(opclasses: ops, entries: entries.filter { $0.storageKey != key(19) })
        XCTAssertEqual(try removed.search(queries: query, strategies: [.equal, .overlaps]).storageKeys, [16, 22, 25, 28].map(key))
        XCTAssertEqual(try tree.search(queries: query, strategies: [.equal, .overlaps]).storageKeys, found.storageKeys)
        let codec = try PlainPageCodec(pageSize: 256)
        var a = try PageAllocator(highWater: 2, free: [], firstBody: 2), b = a
        let wa = try tree.serialize(allocator: &a, codec: codec), wb = try reversed.serialize(allocator: &b, codec: codec)
        XCTAssertEqual(wa.pages.map(\.bytes), wb.pages.map(\.bytes)); XCTAssertEqual(wa.root, wb.root)
    }

    func testEmptyRangesAndUnboundedUnionCannotPruneMatches() throws {
        let range = try op(.range(.scalar(.i32)))
        let rows = [SQLRange(empty: true), SQLRange(lower: nil, upper: .integer(5)),
                    SQLRange(lower: .integer(10), upper: nil)] + (0..<12).map { span(Int64($0), Int64($0 + 2)) }
        let tree = try GistTree.build(opclasses: [range], entries: rows.enumerated().map {
            GistEntry(bound: [.range($0.element)], storageKey: key($0.offset))
        })
        let all = try tree.search(queries: [.range(SQLRange())], strategies: [.overlaps])
        XCTAssertEqual(all.storageKeys, (1..<rows.count).map(key))
        XCTAssertEqual(all.nodesVisited, tree.nodeCount)
        let emptyOverlap = try tree.search(queries: [.range(SQLRange(empty: true))], strategies: [.overlaps])
        XCTAssertTrue(emptyOverlap.storageKeys.isEmpty); XCTAssertEqual(emptyOverlap.nodesVisited, 0)
        let emptyContains = try tree.search(queries: [.range(SQLRange(empty: true))], strategies: [.contains])
        XCTAssertTrue(emptyContains.requiresFullScan); XCTAssertEqual(emptyContains.nodesVisited, 0)
        let none = try GistTree.build(opclasses: [range], entries: [])
        XCTAssertTrue(try none.search(queries: [.range(SQLRange(empty: true))], strategies: [.contains]).requiresFullScan)
        XCTAssertEqual(try none.search(queries: [.range(span(1, 2))], strategies: [.overlaps]).nodesVisited, 1)
        assertCode("0A000") { _ = try tree.search(queries: [.range(self.span(1, 2))], strategies: [.equal]) }

        let numeric = try op(.range(.scalar(.decimal)))
        let a = GistBound.range(SQLRange(lower: .decimal(try SQLDecimal.parse("1.0")), upper: .decimal(try SQLDecimal.parse("2.0"))))
        let b = GistBound.range(SQLRange(lower: .decimal(try SQLDecimal.parse("1.000")), upper: .decimal(try SQLDecimal.parse("2.000"))))
        XCTAssertEqual(try numeric.encode(numeric.union(a, b)), try numeric.encode(b))
        XCTAssertEqual(try numeric.encode(numeric.union(b, a)), try numeric.encode(a))
    }

    func testEncryptedPageTagsAndFreshAllocation() throws {
        let scalar = try op(.scalar(.i64)), codec = try EncryptedPageCodec(pageSize: 1024, dataKey: [UInt8](repeating: 9, count: 32))
        let entries = try (0..<50).map { GistEntry(bound: [try XCTUnwrap(scalar.leafBound(.integer(Int64($0))))], storageKey: key($0)) }
        let tree = try GistTree.build(opclasses: [scalar], entries: entries)
        var allocator = try PageAllocator(highWater: 4, free: [], firstBody: 4), salt: UInt8 = 0
        let written = try tree.serialize(allocator: &allocator, codec: codec, entropy: { count in
            defer { salt += 1 }; return [UInt8](repeating: salt, count: count)
        })
        let pages = Dictionary(uniqueKeysWithValues: written.pages.map { ($0.index, $0.bytes) })
        let read: (PageReference) throws -> [UInt8] = { reference in
            try codec.openBody(pageIndex: reference.index, sealed: XCTUnwrap(pages[reference.index]), expectedTag: reference.tag)
        }
        let opened = try GistTree.open(root: written.root, pageCount: allocator.highWater, opclasses: [scalar], codec: codec, read: read)
        XCTAssertEqual(try opened.tree.leafKeys(), try tree.leafKeys())
        XCTAssertEqual(opened.pages.count, tree.nodeCount)
        var wrong = written.root.tag; wrong[0] ^= 1
        XCTAssertThrowsError(try GistTree.open(root: PageReference(written.root.index, tag: wrong), pageCount: allocator.highWater,
            opclasses: [scalar], codec: codec, read: read))
        let next = allocator.highWater
        let second = try opened.tree.serialize(allocator: &allocator, codec: codec, entropy: { [UInt8](repeating: 3, count: $0) })
        XCTAssertTrue(second.pages.allSatisfy { $0.index >= next })
    }

    func testMalformedPagesFailClosedAndFailedWritePreservesAllocator() throws {
        let scalar = try op(.scalar(.i32)), codec = try PlainPageCodec(pageSize: 256)
        let entries = try (0..<12).map { GistEntry(bound: [try XCTUnwrap(scalar.leafBound(.integer(Int64($0))))], storageKey: key($0)) }
        let tree = try GistTree.build(opclasses: [scalar], entries: entries)
        var allocator = try PageAllocator(highWater: 2, free: [], firstBody: 2)
        let written = try tree.serialize(allocator: &allocator, codec: codec)
        let good = Dictionary(uniqueKeysWithValues: written.pages.map { ($0.index, $0.bytes) })
        func reject(_ edit: (inout [UInt8]) throws -> Void) throws {
            var pages = good, root = try XCTUnwrap(pages[written.root.index])
            try edit(&root); try PageEnvelope.putU32(PageEnvelope.checksum(root, at: 12), into: &root, at: 12)
            pages[written.root.index] = root
            assertCode { _ = try GistTree.open(root: written.root, pageCount: allocator.highWater,
                opclasses: [scalar], codec: codec, read: { try XCTUnwrap(pages[$0.index]) }) }
        }
        try reject { $0[23] &+= 1 } // first scalar minimum narrows a child union
        try reject { try PageEnvelope.putU32(written.root.index, into: &$0, at: 30) } // cycle
        try reject { try PageEnvelope.putU32(5, into: &$0, at: 4) } // fanout
        try reject { $0[$0.count - 1] = 1 } // nonzero padding
        var badEntry = entries[0]
        badEntry = GistEntry(bound: badEntry.bound, storageKey: [UInt8](repeating: 1, count: 300))
        let oversized = try GistTree.build(opclasses: [scalar], entries: [badEntry]), before = allocator.highWater
        assertCode { _ = try oversized.serialize(allocator: &allocator, codec: codec) }
        XCTAssertEqual(allocator.highWater, before)
        assertCode { _ = try GistTree.build(opclasses: [scalar], entries: [entries[0], entries[0]]) }
    }
}
