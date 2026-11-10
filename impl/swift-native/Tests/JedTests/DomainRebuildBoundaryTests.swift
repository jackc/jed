import Foundation
import Synchronization
import XCTest
@testable import Jed

private final class RebuildDevice: BlockStore {
    let memory = MemoryBlockStore()
    private let events = Mutex<(writes: [UInt64], syncs: Int)>(([], 0))
    func read(at offset: UInt64, count: Int) throws -> [UInt8] { try memory.read(at: offset, count: count) }
    func write(at offset: UInt64, bytes: [UInt8]) throws {
        events.withLock { $0.writes.append(offset) }; try memory.write(at: offset, bytes: bytes)
    }
    func sync() throws { events.withLock { $0.syncs += 1 } }
    func size() throws -> UInt64 { try memory.size() }
    func setSize(_ bytes: UInt64) throws { try memory.setSize(bytes) }
    var writes: [UInt64] { events.withLock { $0.writes } }
    var syncs: Int { events.withLock { $0.syncs } }
}

final class DomainRebuildBoundaryTests: XCTestCase {
    private func fixture(_ name: String) throws -> [UInt8] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures"))
        return Array(try Data(contentsOf: url))
    }
    private func open(_ name: String) throws -> StorageDomain {
        let bytes = try fixture(name), device = MemoryBlockStore()
        try device.write(at: 0, bytes: bytes)
        return try StorageDomain.open(store: device, catalogID: 91, cacheBytes: 1024,
            credential: name.hasPrefix("encrypted_") ? .hostKey(Array(repeating: 0x4a, count: 32)) : nil,
            validateCatalog: { _ in })
    }
    private func scratch(_ codec: any PageCodec, device: any BlockStore = MemoryBlockStore()) throws -> ScratchDomain {
        let random = SeededRandomSource(seed: 93)
        return try ScratchDomain.create(store: device, codec: codec, cacheBytes: codec.pageSize,
            entropy: codec.reservedBytes == 0 ? nil : { try random.bytes(count: $0) })
    }
    private func compare(_ source: DomainSnapshot, _ target: DomainSnapshot,
                         file: StaticString = #filePath, line: UInt = #line) throws {
        XCTAssertEqual(Set(source.tables.keys), Set(target.tables.keys), file: file, line: line)
        XCTAssertEqual(Set(source.indexes.keys), Set(target.indexes.keys), file: file, line: line)
        for index in [false, true] {
            let a = index ? source.indexes : source.tables, b = index ? target.indexes : target.tables
            for name in a.keys.sorted() {
                XCTAssertEqual(a[name]!.nextRowID, b[name]!.nextRowID, file: file, line: line)
                var left = a[name]!.cursor(), right = b[name]!.cursor()
                while let l = try left.next() {
                    let r = try XCTUnwrap(right.next(), file: file, line: line)
                    XCTAssertEqual(l.key, r.key, file: file, line: line)
                    for column in l.cells.indices {
                        let type = a[name]!.tree.types[column]
                        let lv = try l.materialize(column: column, type: type, pager: source.pool.pager)
                        let rv = try r.materialize(column: column, type: b[name]!.tree.types[column], pager: target.pool.pager)
                        XCTAssertEqual(lv.isNull, rv.isNull, file: file, line: line)
                        if !lv.isNull {
                            XCTAssertEqual(try ValueCodec.encodeRawBody(lv, type: type),
                                try ValueCodec.encodeRawBody(rv, type: type), file: file, line: line)
                        }
                    }
                }
                XCTAssertNil(try right.next(), file: file, line: line)
            }
        }
    }

    func testRekeyGoldenIndependentOfSourceShapeAndScratchStages() throws {
        let original = try open("encrypted_index_table").committed
        var plainScratch = try scratch(PlainPageCodec(pageSize: 1024))
        var descending = try DomainRebuilder.rebuild(original, scratch: &plainScratch, stageBytes: 512)
        for index in [false, true] {
            let source = index ? descending.indexes : descending.tables
            for name in source.keys {
                let old = source[name]!
                var cursor = old.cursor(descending: true)
                var rebuilt = TableStore(tree: BPlusTree(codec: plainScratch.pool.codec, types: old.tree.types,
                    pool: plainScratch.pool), nextRowID: old.nextRowID)
                while let record = try cursor.next() {
                    let values = try record.cells.indices.map { RecordInput.value(try record.materialize(column: $0, type: old.tree.types[$0], pager: plainScratch.pager)) }
                    try rebuilt.put(RecordDisposition.plan(key: record.key, values: values,
                        types: old.tree.types, codec: plainScratch.pool.codec), netStaging: true)
                }
                if index { descending.setIndexStore(name, rebuilt) } else { descending.setTableStore(name, rebuilt) }
            }
        }
        let expected = try fixture("encrypted_rekey_index_table")
        for budget in [Int64(256), 65_536] {
            let device = RebuildDevice()
            let scratchCodec = try EncryptedPageCodec(pageSize: 1024, dataKey: Array(repeating: 0x37, count: 32))
            var build = try scratch(scratchCodec, device: device)
            let target = try DomainRebuilder.rebuild(descending, scratch: &build, stageBytes: budget)
            XCTAssertEqual(target.pendingBytes, 0)
            if budget == 256 { XCTAssertGreaterThan(build.stages, 20) }
            XCTAssertEqual(device.syncs, 0)
            XCTAssertTrue(device.writes.allSatisfy { $0 >= 4096 })
            XCTAssertLessThanOrEqual(build.pool.residentLeaves, 1)
            let random = SeededRandomSource(seed: 0x4a454442)
            let entropy: KeyEntropy = { try random.bytes(count: $0) }
            let created = try KeyBlock.create(credential: .hostKey(Array(repeating: 0x4a, count: 32)), entropy: entropy)
            let targetCodec = try EncryptedPageCodec(pageSize: 1024, dataKey: created.dataKey)
            let info = try DomainImage.write(target, txid: 155, codec: targetCodec,
                keyBlock: created.block, entropy: entropy) { page in
                let start = Int(page.index) * 1024
                XCTAssertLessThanOrEqual(start + 1024, expected.count)
                if start + 1024 <= expected.count {
                    XCTAssertEqual(page.bytes, Array(expected[start..<(start + 1024)]), "budget \(budget), page \(page.index)")
                }
            }
            XCTAssertEqual(Int(info.pageCount) * 1024, expected.count)
        }
    }

    func testLayoutMigrationReplansAllSharedValueFamiliesAndIndexes() throws {
        let names = ["overflow_table", "compressed_table", "nested_composite_table", "array_table", "range_table",
                     "gin_jsonb_table", "statistics_table", "encrypted_overflow_table", "encrypted_index_table"]
        for name in names {
            do {
                let source = try open(name).committed
                var plain = try scratch(PlainPageCodec(pageSize: 4096))
                let widened = try DomainRebuilder.rebuild(source, scratch: &plain, stageBytes: 384)
                try compare(source, widened)
                let encrypted = try EncryptedPageCodec(pageSize: 1024, dataKey: Array(repeating: 0x63, count: 32))
                var sealed = try scratch(encrypted)
                let narrowed = try DomainRebuilder.rebuild(widened, scratch: &sealed, stageBytes: 384)
                try compare(source, narrowed)
                XCTAssertEqual(narrowed.catalog.tables.map(\.rowCount), source.catalog.tables.map(\.rowCount))
                XCTAssertEqual(narrowed.pendingBytes, 0)
                XCTAssertLessThanOrEqual(sealed.pool.residentLeaves, 1)
            } catch { XCTFail("\(name): \(error)") }
        }
    }

    func testTargetRefusalLeavesSourceUnchangedAndMemoryEncryptionStillFails() throws {
        let device = MemoryBlockStore(), source = try StorageDomain.create(store: device, catalogID: 92, pageSize: 4096)
        var snapshot = source.committed, catalog = snapshot.catalog
        let name = try SQLText("t"), type = TypeDescriptor(.scalar(.text))
        catalog.tables.append(CatalogTable(name: name,
            columns: [CatalogColumn(name: try SQLText("k"), type: CatalogTypeUse(.scalar(.text)))],
            primaryKey: [0], root: PageReference(0)))
        try snapshot.updateCatalog(catalog, schemaChanged: true)
        var store = snapshot.emptyStore(types: [type])
        // RECORD_MAX is a disposition target, not a hard record limit. A
        // 600-byte key fits the target's actual leaf capacity; use a key that
        // fits the 4096-byte source but cannot fit even alone at 1024 bytes.
        let value = SQLValue.text(try SQLText(String(repeating: "x", count: 1200)))
        try store.put(RecordDisposition.plan(key: KeyEncoding.encode(value, type: type), values: [.value(value)],
            types: [type], codec: source.pager.codec), netStaging: true)
        snapshot.setTableStore(name, store)
        let saved = try source.persist(XCTUnwrap(source.prepare(snapshot)))
        let bytes = try device.read(at: 0, count: Int(device.size()))
        var build = try scratch(EncryptedPageCodec(pageSize: 1024, dataKey: Array(repeating: 0x48, count: 32)))
        XCTAssertThrowsError(try DomainRebuilder.rebuild(saved, scratch: &build, stageBytes: 256)) {
            XCTAssertEqual(($0 as? JedError)?.code, "0A000")
        }
        XCTAssertFalse(source.pager.isPoisoned)
        XCTAssertEqual(try device.read(at: 0, count: bytes.count), bytes)
        XCTAssertEqual(source.committed.txid, saved.txid)
        XCTAssertThrowsError(try StorageDomain.create(store: MemoryBlockStore(), catalogID: 93,
            credential: .hostKey(Array(repeating: 0x4a, count: 32)))) {
            XCTAssertEqual(($0 as? JedError)?.code, "55000")
        }
    }
}
