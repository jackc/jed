import Foundation
import XCTest
@testable import Jed

final class CatalogTests: XCTestCase {
    private func fixture(_ name: String) throws -> [UInt8] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures"))
        return [UInt8](try Data(contentsOf: url))
    }
    private func pager(_ bytes: [UInt8], encrypted: Bool = false) throws -> Pager {
        let store = MemoryBlockStore(); try store.write(at: 0, bytes: bytes)
        return try Pager.open(store: store, credential: encrypted ? Credential.hostKey([UInt8](repeating: 0x4a, count: 32)) : nil)
    }

    func testFrozenOrdinaryCatalogsRebuildEveryPageByteExactly() throws {
        // Recovery/damaged-image acceptance is a separate adapter contract.
        // This test covers ordinary catalog bytes, including all six encrypted
        // fixtures and the version-skew catalogs whose trees need later checks.
        let paths = GeneratedSpec.sourceHashes.keys.filter {
            $0.hasPrefix("spec/fileformat/fixtures/") && !$0.contains("/cow_") && !$0.contains("/malformed_")
        }.sorted()
        XCTAssertEqual(paths.count, 91)
        for path in paths {
            let name = String(path.split(separator: "/").last!.dropLast(4))
            do {
                let bytes = try fixture(name), encrypted = name.hasPrefix("encrypted_")
                let pager = try pager(bytes, encrypted: encrypted), codec = pager.codec
                let catalog = try CatalogChain.read(root: pager.selected.meta.root, pageCount: pager.selected.meta.pageCount,
                    codec: codec, read: pager.readBody)
                let image = try CatalogImage.collect(catalog.entries, catalogID: 1)
                let originalEntries = try catalog.entries.map { try CatalogCodec.encode($0, encrypted: encrypted) }
                let sortedEntries = try image.entries().map { try CatalogCodec.encode($0, encrypted: encrypted) }
                XCTAssertEqual(originalEntries, sortedEntries, name)
                var allocator = try PageAllocator(highWater: pager.selected.meta.pageCount, free: catalog.pages.sorted(), firstBody: codec.firstBodyPage)
                var nonces = catalog.pages.reversed().map { page in
                    let end = (Int(page) + 1) * codec.pageSize
                    return Array(bytes[(end - 40)..<(end - 16)])
                }.makeIterator()
                let rebuilt = try CatalogChain.write(image.entries(), allocator: &allocator, codec: codec,
                    entropy: encrypted ? { count in XCTAssertEqual(count, 24); return try XCTUnwrap(nonces.next()) } : nil)
                XCTAssertEqual(rebuilt.root, pager.selected.meta.root, name)
                XCTAssertEqual(rebuilt.pages.map(\.index), catalog.pages, name)
                for page in rebuilt.pages {
                    XCTAssertEqual(page.bytes, Array(bytes[(Int(page.index) * codec.pageSize)..<((Int(page.index) + 1) * codec.pageSize)]), name)
                }
            } catch { XCTFail("\(name): \(error)") }
        }
    }

    func testContinuedEntryFramingAndCorruption() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        let text = try SQLText(String(repeating: "select 1 ", count: 100))
        let view = CatalogView(name: try SQLText("v"), columns: [.init(name: try SQLText("x"))], definition: text, dependencies: [])
        var allocator = try PageAllocator(highWater: 2, free: [], firstBody: 2)
        let write = try CatalogChain.write([.view(view)], allocator: &allocator, codec: codec)
        XCTAssertGreaterThan(write.pages.count, 3)
        var pages = Dictionary(uniqueKeysWithValues: write.pages.map { ($0.index, $0.bytes) })
        let read: (PageReference) throws -> [UInt8] = { try XCTUnwrap(pages[$0.index]) }
        let decoded = try CatalogChain.read(root: write.root, pageCount: allocator.highWater, codec: codec, read: read)
        XCTAssertEqual(decoded.entries.count, 1)
        XCTAssertEqual(try CatalogCodec.encode(decoded.entries[0], encrypted: false), try CatalogCodec.encode(.view(view), encrypted: false))
        var middle = try XCTUnwrap(pages[3]); try PageEnvelope.putU32(1, into: &middle, at: 4)
        try PageEnvelope.putU32(PageEnvelope.checksum(middle, at: 12), into: &middle, at: 12); pages[3] = middle
        XCTAssertThrowsError(try CatalogChain.read(root: write.root, pageCount: allocator.highWater, codec: codec, read: read)) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX001")
        }
    }

    func testMalformedCatalogReferencesAndCardinality() throws {
        for name in ["malformed_missing_type", "malformed_fk_type"] {
            XCTAssertThrowsError(try {
                let pager = try pager(fixture(name))
                let raw = try CatalogChain.read(root: pager.selected.meta.root, pageCount: pager.selected.meta.pageCount, codec: pager.codec, read: pager.readBody)
                _ = try CatalogImage.collect(raw.entries, catalogID: 1)
            }(), name) { XCTAssertEqual(($0 as? JedError)?.code, "XX001", name) }
        }
        // The catalog trusts a nonzero cardinality without recounting leaves.
        // malformed_row_count's 25-over-20 mismatch is a write/integrity error,
        // not an open-time catalog error (shared malformed_row_count.test).
        let countPager = try pager(fixture("malformed_row_count"))
        let countCatalog = try CatalogChain.read(root: countPager.selected.meta.root, pageCount: countPager.selected.meta.pageCount,
            codec: countPager.codec, read: countPager.readBody)
        var counts = try CatalogImage.collect(countCatalog.entries, catalogID: 1)
        XCTAssertEqual(counts.tables[0].rowCount, 25)
        counts.tables[0].root = PageReference(0)
        var countReader = ByteReader(try CatalogCodec.encode(.table(counts.tables[0]), encrypted: false))
        XCTAssertThrowsError(try CatalogCodec.decode(&countReader, encrypted: false)) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX001")
        }
        let a = try SQLText("a"), b = try SQLText("b"), field = try SQLText("x")
        var image = CatalogImage()
        image.composites = [CatalogComposite(name: a, fields: [CatalogColumn(name: field, type: CatalogTypeUse(.array(.composite(b))))]),
            CatalogComposite(name: b, fields: [CatalogColumn(name: field, type: CatalogTypeUse(.composite(a)))])]
        XCTAssertThrowsError(try image.resolvedTypes(catalogID: 1)) { XCTAssertEqual(($0 as? JedError)?.code, "XX001") }
    }
}
