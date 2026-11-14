import Foundation
import XCTest
@testable import Jed

final class Format53Tests: XCTestCase {
    private func integer(_ rows: [[SQLValue]]) throws -> Int64 {
        XCTAssertEqual(rows.count, 1)
        let row = try XCTUnwrap(rows.first)
        XCTAssertEqual(row.count, 1)
        guard case .integer(let value) = try XCTUnwrap(row.first) else { throw JedError.corruption("expected integer result") }
        return value
    }
    func testStampWindowWrapAndNarrowManifestCapacities() throws {
        for (size, encrypted, capacity) in [(256, false, 16), (4096, false, 336), (8192, false, 678), (1024, true, 75), (8192, true, 672)] {
            XCTAssertEqual(PageRemap.inlineCapacity(pageSize: size, encrypted: encrypted), capacity)
        }
        var bytes = [UInt8](repeating: 0, count: 16)
        for stamp: UInt16 in [65535, 0, 1] {
            bytes[2] = UInt8(stamp >> 8); bytes[3] = UInt8(truncatingIfNeeded: stamp)
            XCTAssertTrue(PageStamp.contains(bytes, floor: 65534, newest: 65537))
        }
        bytes[3] = 2
        XCTAssertFalse(PageStamp.contains(bytes, floor: 65534, newest: 65537))
        XCTAssertTrue(PageStamp.contains(bytes, floor: nil, newest: 65537))
        XCTAssertTrue(PageStamp.contains(bytes, floor: 0, newest: 65536))
    }

    func testRemapsAreSlotBoundAndRejectInvalidMetadata() throws {
        let entries = [PageRemap(parent: 8, slot: 0, target: PageReference(9)),
                       PageRemap(parent: 9, slot: 1, target: PageReference(10))]
        XCTAssertEqual(try PageRemap.resolve([PageReference(2), PageReference(3)], parent: 8, entries: entries),
                       [PageReference(9), PageReference(3)], "resolution must not chain through another parent's entry")
        XCTAssertThrowsError(try PageRemap.resolve([PageReference(2)], parent: 9, entries: entries))
        try PageRemap.validate(entries, pageSize: 256, encrypted: false, pageCount: 20)
        for bad in [entries.reversed().map { $0 }, [entries[0], entries[0]],
                    [PageRemap(parent: 8, slot: 0, target: PageReference(8))],
                    [PageRemap(parent: 8, slot: 0, target: PageReference(20))]] {
            XCTAssertThrowsError(try PageRemap.validate(bad, pageSize: 256, encrypted: false, pageCount: 20))
        }
    }

    func testRetainedInteriorsPreservePinnedReaderAndReopen() throws {
        let device = MemoryBlockStore()
        let storage = try StorageDomain.create(store: device, kind: .file, catalogID: Database.allocateCatalogID(), pageSize: 256)
        let database = Database(core: DatabaseCore(storage: storage), environment: Environment.builder().build())
        defer { try? database.close() }
        try database.execute("CREATE TABLE t(id i64 PRIMARY KEY, v i64)")
        try database.execute("INSERT INTO t SELECT g::i64, 0::i64 FROM generate_series(1,1200) g")
        let reader = try database.session(); defer { reader.close() }
        try reader.begin(writable: false)
        XCTAssertEqual(try integer(reader.query("SELECT sum(v)::i64 FROM t").collect()), 0)
        var retained = false, folded = false, priorEntries = 0
        for iteration in 1...80 {
            let id = (iteration * 37) % 1200 + 1
            try database.execute("UPDATE t SET v = v + 1 WHERE id = \(id)")
            let meta = storage.pager.selected.meta
            retained = retained || !meta.remaps.isEmpty
            folded = folded || priorEntries > 0 && meta.remaps.isEmpty
            priorEntries = meta.remaps.count
            XCTAssertEqual(try integer(reader.query("SELECT sum(v)::i64 FROM t").collect()), 0)
        }
        XCTAssertTrue(retained)
        XCTAssertTrue(folded)
        try reader.rollback()
        let reopened = try StorageDomain.open(store: device, kind: .file, catalogID: Database.allocateCatalogID(), validateCatalog: { _ in })
        let peer = Database(core: DatabaseCore(storage: reopened), environment: Environment.builder().build())
        defer { try? peer.close() }
        XCTAssertEqual(try integer(peer.query("SELECT sum(v)::i64 FROM t").collect()), 80)
    }
}
