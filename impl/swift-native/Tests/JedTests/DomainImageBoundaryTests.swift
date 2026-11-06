import Foundation
import XCTest
@testable import Jed

final class DomainImageBoundaryTests: XCTestCase {
    private func paths() -> [String] {
        GeneratedSpec.sourceHashes.keys.filter {
            $0.hasPrefix("spec/fileformat/fixtures/") && !$0.contains("/cow_") && !$0.contains("/malformed_")
        }.sorted()
    }
    private func fixture(_ name: String) throws -> [UInt8] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures"))
        return Array(try Data(contentsOf: url))
    }
    private func expect(_ code: String, _ operation: () throws -> Void) {
        XCTAssertThrowsError(try operation()) { XCTAssertEqual(($0 as? JedError)?.code, code) }
    }

    func testFrozenImagesStreamCanonicalBytesAndRepairTornMeta() throws {
        XCTAssertEqual(paths().count, 87)
        var encryptedCount = 0
        for path in paths() {
            let name = String(path.split(separator: "/").last!.dropLast(4))
            do {
                let original = try fixture(name), source = MemoryBlockStore(), encrypted = name.hasPrefix("encrypted_")
                try source.write(at: 0, bytes: original)
                let credential = encrypted ? try Credential.hostKey(Array(repeating: 0x4a, count: 32)) : nil
                let domain = try StorageDomain.open(store: source, catalogID: 50, cacheBytes: 1024,
                    credential: credential, validateCatalog: { _ in })
                var target: (any PageCodec)?, block: KeyBlock?, entropy: KeyEntropy?
                if encrypted {
                    encryptedCount += 1
                    let random = SeededRandomSource(seed: 0x4a454442)
                    entropy = { try random.bytes(count: $0) }
                    let created = try KeyBlock.create(credential: credential!, entropy: entropy!)
                    block = created.block
                    target = try EncryptedPageCodec(pageSize: domain.pager.pageSize, dataKey: created.dataKey)
                    XCTAssertEqual(block, domain.pager.selectedKeyBlock, name)
                }
                var emitted: Set<UInt32> = [], order: [UInt32] = []
                let info = try DomainImage.write(domain.committed, txid: domain.committed.txid,
                    codec: target, keyBlock: block, entropy: entropy) { page in
                    XCTAssertTrue(emitted.insert(page.index).inserted, name)
                    order.append(page.index)
                    let start = Int(page.index) * domain.pager.pageSize
                    guard start + page.bytes.count <= original.count else { throw JedError("XX001", "export exceeds golden extent") }
                    // These two recovery fixtures deliberately damage one
                    // meta CRC. A fresh image restores both slots from the
                    // structurally selected valid generation.
                    let expected = name.hasPrefix("torn_meta_") && page.index < 2
                        ? domain.pager.selected.bytes : Array(original[start..<(start + page.bytes.count)])
                    XCTAssertEqual(page.bytes, expected, "\(name):\(page.index)")
                }
                XCTAssertEqual(info.pageCount, domain.pager.selected.meta.pageCount, name)
                XCTAssertEqual(info.livePages, domain.livePages, name)
                XCTAssertEqual(info.root, domain.pager.selected.meta.root, name)
                XCTAssertEqual(emitted, Set(0..<info.pageCount), name)
                XCTAssertEqual(Array(order.suffix(2)), [0, 1], name)
                XCTAssertLessThanOrEqual(domain.pool.residentLeaves, max(1, 1024 / domain.pager.pageSize), name)
            } catch { XCTFail("\(name): \(error)") }
        }
        XCTAssertEqual(encryptedCount, 6)
    }

    func testSinkFailureLeavesSourceUsableAndCanRetryWithoutRootChanges() throws {
        let source = MemoryBlockStore(), original = try fixture("encrypted_overflow_table")
        try source.write(at: 0, bytes: original)
        let domain = try StorageDomain.open(store: source, catalogID: 51, cacheBytes: 1024,
            credential: .hostKey(Array(repeating: 0x4a, count: 32)), validateCatalog: { _ in })
        let snapshot = domain.committed, block = try XCTUnwrap(domain.pager.selectedKeyBlock)
        let random = SeededRandomSource(seed: 123)
        var writes = 0
        expect("58030") {
            _ = try DomainImage.write(snapshot, txid: 17, keyBlock: block, entropy: { try random.bytes(count: $0) }) { _ in
                writes += 1
                if writes == 4 { throw JedError("58030", "destination failure") }
            }
        }
        XCTAssertEqual(writes, 4); XCTAssertFalse(domain.pager.isPoisoned)
        XCTAssertEqual(domain.committed.txid, snapshot.txid)
        XCTAssertEqual(try source.read(at: 0, count: original.count), original)
        let destination = MemoryBlockStore()
        let info = try DomainImage.write(snapshot, txid: 18, keyBlock: block, entropy: { try random.bytes(count: $0) }) { page in
            try destination.write(at: UInt64(page.index) * UInt64(domain.pager.pageSize), bytes: page.bytes)
        }
        let reopened = try StorageDomain.open(store: destination, catalogID: 51,
            credential: .hostKey(Array(repeating: 0x4a, count: 32)), validateCatalog: { _ in })
        XCTAssertEqual(reopened.committed.txid, 18); XCTAssertEqual(reopened.livePages, info.livePages)
        XCTAssertEqual(reopened.committed.catalog.tables.map(\.rowCount), snapshot.catalog.tables.map(\.rowCount))
    }

    func testMetadataAndLayoutRefusalsPrecedeSinkAndEntropy() throws {
        let domain = try StorageDomain.create(store: MemoryBlockStore(), catalogID: 52, pageSize: 256)
        var snapshot = domain.committed, catalog = snapshot.catalog
        let longName = try SQLText(String(repeating: "t", count: 65_536))
        catalog.tables.append(CatalogTable(name: longName,
            columns: [CatalogColumn(name: try SQLText("id"), type: CatalogTypeUse(.scalar(.i64)))],
            primaryKey: [0], root: PageReference(0)))
        try snapshot.updateCatalog(catalog, schemaChanged: true)
        snapshot.setTableStore(longName, snapshot.emptyStore(types: [TypeDescriptor(.scalar(.i64))]))
        var writes = 0, draws = 0
        XCTAssertThrowsError(try DomainImage.write(snapshot, txid: 1, entropy: { count in
            draws += 1; return Array(repeating: 0, count: count)
        }, sink: { _ in writes += 1 }))
        XCTAssertEqual(writes, 0); XCTAssertEqual(draws, 0)
        expect("0A000") {
            _ = try DomainImage.write(domain.committed, txid: 1, codec: PlainPageCodec(pageSize: 512)) { _ in writes += 1 }
        }
        XCTAssertEqual(writes, 0)
    }
}
