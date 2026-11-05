import Foundation
import XCTest
@testable import Jed

final class MetaTests: XCTestCase {
    private func body(_ index: UInt32, marker: UInt8 = 0) throws -> [UInt8] {
        var page = [UInt8](repeating: 0, count: 256)
        page[0] = 1; page[16] = marker
        return try PlainPageCodec(pageSize: 256).sealBody(pageIndex: index, plaintext: page, salt: []).bytes
    }

    private func manifest(txid: UInt64, root: UInt32, pageCount: UInt32,
                          entries: [ManifestEntry] = [], chain: [UInt32] = []) throws -> CommitManifest {
        try CommitManifest.build(pageSize: 256, encrypted: false, txid: txid,
            root: PageReference(root), pageCount: pageCount, freeList: PageReference(0),
            livePages: 1, entries: entries, chain: chain)
    }

    private func expect(_ code: String, _ action: () throws -> Void,
                        file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try action(), file: file, line: line) { error in
            XCTAssertEqual((error as? JedError)?.code, code, "\(error)", file: file, line: line)
        }
    }

    func testBootstrapUsesNoBodyReadsAndParityFloorOnTie() throws {
        let image = try manifest(txid: 1, root: 2, pageCount: 3)
        let encoded = try image.meta.encode(inline: image.inline)
        var reads: [UInt32] = []
        let selected = try MetaRecovery.select(pageSize: 256, fileBytes: 3 * 256) { index in
            reads.append(index)
            guard index <= 1 else { throw JedError("58030", "must not read inherited page") }
            return encoded
        }
        XCTAssertEqual(selected.meta.txid, 1)
        XCTAssertEqual(selected.slot, 0)
        XCTAssertEqual(selected.floorSlot, 1)
        XCTAssertEqual(reads, [0, 1])
        XCTAssertTrue(selected.protectedPages.isEmpty)
        XCTAssertEqual(try MetaPage.decode(encoded).pageCount, 3)
    }

    func testIncompleteLatestFallsBackButHostIOAlwaysPropagates() throws {
        let old = try manifest(txid: 1, root: 2, pageCount: 3)
        let page = try body(3, marker: 99)
        let entry = ManifestEntry(pageIndex: 3, digest: Checksums.crc64(page))
        let new = try manifest(txid: 2, root: 3, pageCount: 4, entries: [entry])
        let oldMeta = try old.meta.encode(inline: old.inline), newMeta = try new.meta.encode(inline: new.inline)
        var pages = [newMeta, oldMeta, try body(2), page]
        let selected = try MetaRecovery.select(pageSize: 256, fileBytes: 4 * 256) { pages[Int($0)] }
        XCTAssertEqual(selected.meta.txid, 2)
        XCTAssertEqual(selected.protectedPages, [3])
        // An older, self-checksummed page at the same address is still incomplete.
        pages[3] = try body(3, marker: 98)
        XCTAssertEqual(try MetaRecovery.select(pageSize: 256, fileBytes: 4 * 256) { pages[Int($0)] }.meta.txid, 1)
        for code in ["58030", "XX001"] {
            var attemptedOld = false
            expect(code) {
                _ = try MetaRecovery.select(pageSize: 256, fileBytes: 4 * 256) { index in
                    if index == 3 { throw JedError(code, "host failure") }
                    if index == 2 { attemptedOld = true }
                    return pages[Int(index)]
                }
            }
            XCTAssertFalse(attemptedOld)
        }
        // Physical preallocation beyond page_count is permitted.
        XCTAssertEqual(try MetaRecovery.select(pageSize: 256, fileBytes: 100 * 256) { pages[Int($0)] }.meta.txid, 1)
    }

    func testOverflowManifestBindsOrderAndRetainsEmptyReservations() throws {
        var bodies: [UInt32: [UInt8]] = [:], entries: [ManifestEntry] = []
        for index: UInt32 in 2...18 {
            let page = try body(index, marker: UInt8(index))
            bodies[index] = page
            entries.append(ManifestEntry(pageIndex: index, digest: Checksums.crc64(page)))
        }
        let built = try manifest(txid: 17, root: 2, pageCount: 22, entries: entries, chain: [20, 21])
        XCTAssertEqual(built.inline.count, 16)
        XCTAssertEqual(built.pages.count, 2)
        XCTAssertEqual(try PageEnvelope.readU32(built.pages[0].bytes, at: 4), 1)
        XCTAssertEqual(try PageEnvelope.readU32(built.pages[1].bytes, at: 4), 0)
        for page in built.pages { bodies[page.pageIndex] = page.bytes }
        let encoded = try built.meta.encode(inline: built.inline)
        let decoded = try MetaPage.decode(encoded)
        let result = try MetaRecovery.validateManifest(decoded, bytes: encoded) { try XCTUnwrap(bodies[$0]) }
        XCTAssertEqual(result.entries, entries)
        XCTAssertEqual(result.chain, [20, 21])
        var corrupt = try XCTUnwrap(bodies[21]); corrupt[255] = 1
        try PageEnvelope.putU32(PageEnvelope.checksum(corrupt, at: 12), into: &corrupt, at: 12)
        bodies[21] = corrupt
        expect("XX001") { _ = try MetaRecovery.validateManifest(decoded, bytes: encoded) { try XCTUnwrap(bodies[$0]) } }
        expect("XX001") { _ = try manifest(txid: 17, root: 2, pageCount: 22, entries: entries, chain: [18]) }
        expect("XX001") { _ = try manifest(txid: 17, root: 2, pageCount: 22, entries: entries.reversed(), chain: [20]) }
    }

    func testIncompleteEqualTxidSlotCannotBecomeDurabilityFloor() throws {
        let valid = try manifest(txid: 1, root: 2, pageCount: 3).meta.encode(inline: [])
        let missing = ManifestEntry(pageIndex: 3, digest: 99)
        let broken = try manifest(txid: 1, root: 3, pageCount: 4, entries: [missing])
        let bytes = try broken.meta.encode(inline: [missing])
        let pages = [valid, bytes, try body(2), try body(3)]
        let selected = try MetaRecovery.select(pageSize: 256, fileBytes: 4 * 256) { pages[Int($0)] }
        XCTAssertEqual(selected.slot, 0)
        XCTAssertEqual(selected.floorSlot, 0)
    }

    func testMetaBoundsAreCheckedBeforeManifestReadsOrAllocation() throws {
        let built = try manifest(txid: 1, root: 2, pageCount: 3)
        let valid = try built.meta.encode(inline: [])
        for offset in [0, 4, 6, 7, 8, 20, 28, 36, 40, 44, 48, 56, 60, 255] {
            var bad = valid; bad[offset] ^= 0x80
            try PageEnvelope.putU32(PageEnvelope.checksum(bad, at: 32), into: &bad, at: 32)
            expect("XX001") { _ = try MetaPage.decode(bad) }
        }
        var bad = valid
        try PageEnvelope.putU32(UInt32.max, into: &bad, at: 40)
        try PageEnvelope.putU32(PageEnvelope.checksum(bad, at: 32), into: &bad, at: 32)
        expect("XX001") { _ = try MetaRecovery.select(pageSize: 256, fileBytes: 3 * 256) { _ in bad } }
        expect("XX001") { _ = try MetaRecovery.select(pageSize: 256, fileBytes: 2 * 256) { _ in valid } }
        var beyondFile = valid
        try PageEnvelope.putU32(UInt32.max, into: &beyondFile, at: 24)
        try PageEnvelope.putU32(PageEnvelope.checksum(beyondFile, at: 32), into: &beyondFile, at: 32)
        // A high-water is bounded by the byte device during selection, not by
        // the independently decoded meta page's length.
        expect("XX001") { _ = try MetaRecovery.select(pageSize: 256, fileBytes: 3 * 256) { _ in beyondFile } }
    }

    func testStoreOpenAndEncryptedGoldenKeylessSelection() throws {
        for name in ["encrypted_empty_db", "encrypted_pk_table"] {
            let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures"))
            let bytes = Array(try Data(contentsOf: url))
            let store = MemoryBlockStore(); try store.write(at: 0, bytes: bytes)
            let selected = try MetaRecovery.open(store)
            XCTAssertTrue(selected.meta.encrypted)
            XCTAssertEqual(selected.meta.txid, 1)
            XCTAssertEqual(selected.slot, 0)
            let encoded = try selected.meta.encode(inline: [])
            XCTAssertEqual(encoded.prefix(32), selected.bytes.prefix(32))
            XCTAssertEqual(encoded[36..<96], selected.bytes[36..<96])
            // The stored checksum also covers the nonce/tag; encode has a zero
            // reserve until EncryptedPageCodec.sealMeta authenticates it.
        }
    }

    func testCRCValidSlotsCannotDeclareDifferentEncryptionVariants() throws {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "encrypted_empty_db", withExtension: "jed", subdirectory: "Fixtures"))
        let store = MemoryBlockStore(); try store.write(at: 0, bytes: Array(Data(contentsOf: url)))
        var other = try store.read(at: 1024, count: 1024)
        other[6] = 0; other[7] = 0
        try PageEnvelope.putU32(PageEnvelope.checksum(other, at: 32), into: &other, at: 32)
        try store.write(at: 1024, bytes: other)
        expect("XX001") { _ = try MetaRecovery.open(store) }
        // A torn slot makes no variant declaration.
        other[32] ^= 1
        try store.write(at: 1024, bytes: other)
        XCTAssertTrue(try MetaRecovery.open(store).meta.encrypted)
    }
}
