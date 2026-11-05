import Foundation
import Synchronization
import XCTest
@testable import Jed

private final class RecordedStore: BlockStore {
    let memory = MemoryBlockStore()
    private let events = Mutex<[String]>([])
    func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        events.withLock { $0.append("read:\(offset):\(count)") }
        return try memory.read(at: offset, count: count)
    }
    func write(at offset: UInt64, bytes: [UInt8]) throws {
        events.withLock { $0.append("write:\(offset):\(bytes.count)") }
        try memory.write(at: offset, bytes: bytes)
    }
    func size() throws -> UInt64 { try memory.size() }
    func setSize(_ bytes: UInt64) throws {
        events.withLock { $0.append("resize:\(bytes)") }; try memory.setSize(bytes)
    }
    func sync() throws { events.withLock { $0.append("sync") } }
    func reset() { events.withLock { $0.removeAll() } }
    var log: [String] { events.withLock { $0 } }
}

final class PagerTests: XCTestCase {
    private func body(_ index: UInt32, marker: UInt8 = 0) throws -> StoredPage {
        var page = [UInt8](repeating: 0, count: 256); page[0] = 1; page[16] = marker
        let sealed = try PlainPageCodec(pageSize: 256).sealBody(pageIndex: index, plaintext: page, salt: [])
        return StoredPage(index: index, bytes: sealed.bytes)
    }

    private func opened() throws -> (RecordedStore, Pager) {
        let store = RecordedStore()
        let bootstrap = try CommitManifest.build(pageSize: 256, encrypted: false, txid: 1,
            root: PageReference(2), pageCount: 3, freeList: PageReference(0), livePages: 1, entries: [], chain: [])
        let bytes = try bootstrap.meta.encode(inline: [])
        try store.setSize(64 * 256)
        try store.write(at: 0, bytes: bytes); try store.write(at: 256, bytes: bytes)
        try store.write(at: 512, bytes: body(2).bytes)
        let pager = try Pager.open(store: store)
        store.reset()
        return (store, pager)
    }

    private func prepared(_ pager: Pager, count: UInt32 = 1) throws -> PreparedPageCommit {
        let first = pager.selected.meta.pageCount
        let pages = try (first..<(first + count)).map { try body($0, marker: UInt8(truncatingIfNeeded: $0)) }
        let total = pager.pendingEntries.count + Int(count)
        let chainCount = max(0, (max(0, total - 16) + 17) / 18)
        let chain = (0..<chainCount).map { first + count + UInt32($0) }
        return try pager.prepare(body: pages, root: PageReference(first),
            pageCount: first + count + UInt32(chainCount), freeList: PageReference(0), livePages: 1,
            freePages: [], manifestPages: chain, reuseAllowed: true)
    }

    private func expect(_ code: String, _ action: () throws -> Void,
                        file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try action(), file: file, line: line) { error in
            XCTAssertEqual((error as? JedError)?.code, code, "\(error)", file: file, line: line)
        }
    }

    func testRecoveryStabilizesBeforeWritesAndSteadyCommitUsesOneBarrier() throws {
        let (store, pager) = try opened()
        var directories = 0
        let plan = try prepared(pager)
        XCTAssertTrue(store.log.isEmpty)
        try pager.commit(plan, syncDirectory: { directories += 1 })
        XCTAssertEqual(store.log, ["sync", "write:768:256", "write:0:256", "sync"])
        XCTAssertEqual(directories, 1)
        XCTAssertEqual(pager.selected.meta.txid, 2)
        store.reset()
        try pager.commit(prepared(pager), syncDirectory: { directories += 1 })
        XCTAssertEqual(store.log, ["write:1024:256", "write:256:256", "sync"])
        XCTAssertEqual(directories, 1)
        XCTAssertEqual(try Pager.open(store: store).selected.meta.txid, 3)
    }

    func testSerializationFailureDoesNotPoisonAndStalePlanDoesNotWrite() throws {
        let (store, pager) = try opened()
        expect("XX001") {
            _ = try pager.prepare(body: [body(2)], root: PageReference(2), pageCount: 3,
                freeList: PageReference(0), livePages: 1, freePages: [], manifestPages: [], reuseAllowed: true)
        }
        XCTAssertFalse(pager.isPoisoned)
        XCTAssertTrue(store.log.isEmpty)
        let first = try prepared(pager), stale = try prepared(pager)
        try pager.commit(first)
        store.reset()
        expect("55006") { try pager.commit(stale) }
        XCTAssertFalse(pager.isPoisoned)
        XCTAssertTrue(store.log.isEmpty)
    }

    func testTornWritesRecoverAndFailedHandleCannotContinue() throws {
        for fault in [CommitFault.body(ordinal: 1, tearBytes: 18), .meta(tearBytes: 40), .sync(ordinal: 2)] {
            let (store, pager) = try opened()
            let plan = try prepared(pager)
            pager.armFault(fault)
            expect("58030") { try pager.commit(plan) }
            XCTAssertTrue(pager.isPoisoned)
            expect("58030") { try pager.checkWritable() }
            let recovered = try Pager.open(store: store)
            let expected: UInt64
            if case .sync = fault { expected = 2 } else { expected = 1 }
            XCTAssertEqual(recovered.selected.meta.txid, expected)
            store.reset()
            try recovered.commit(prepared(recovered))
            XCTAssertEqual(store.log.first, "sync")
            XCTAssertEqual(recovered.selected.meta.txid, expected + 1)
        }
    }

    func testRelaxedBatchKeepsFloorAndCumulativeDependencies() throws {
        let (store, pager) = try opened()
        let floor = try store.memory.read(at: 256, count: 256)
        for _ in 0..<3 { try pager.commit(prepared(pager), synchronous: false) }
        XCTAssertEqual(pager.selected.meta.txid, 4)
        XCTAssertEqual(pager.selected.slot, 0)
        XCTAssertEqual(pager.pendingEntries.map(\.pageIndex), [3, 4, 5])
        XCTAssertEqual(pager.selected.entries.map(\.pageIndex), [3, 4, 5])
        XCTAssertEqual(try store.memory.read(at: 256, count: 256), floor)
        XCTAssertEqual(store.log.filter { $0 == "sync" }.count, 1) // adopted-floor stabilization only
        XCTAssertEqual(try Pager.open(store: store).selected.meta.txid, 4)
        store.reset()
        try pager.flush()
        XCTAssertEqual(store.log, ["sync"])
        XCTAssertTrue(pager.pendingEntries.isEmpty)
        try pager.commit(prepared(pager), synchronous: false)
        XCTAssertEqual(pager.selected.slot, 1)
    }

    func testFailedCapFlushRevokesReadsAfterPublication() throws {
        let (_, pager) = try opened()
        let plan = try prepared(pager, count: 17)
        pager.armFault(.sync(ordinal: 2))
        try pager.commit(plan, synchronous: false)
        XCTAssertEqual(pager.selected.meta.txid, 2)
        XCTAssertTrue(pager.isPoisoned)
        expect("58030") { _ = try pager.readBody(PageReference(2)) }
        expect("58030") { try pager.flush() }
    }

    func testGrowthPolicyIsGeometricBoundedAndSaturating() {
        XCTAssertEqual(Pager.growthStep(pages: 3, pageSize: 256), 64)
        XCTAssertEqual(Pager.growthStep(pages: 80, pageSize: 256), 80)
        XCTAssertEqual(Pager.growthStep(pages: 10_000, pageSize: 256), 4096)
        XCTAssertEqual(Pager.growthStep(pages: 0, pageSize: 65_536), 1)
        XCTAssertEqual(Pager.growthStep(pages: .max, pageSize: 65_536), 16)
    }

    func testSelectedEncryptedMetaFailureDoesNotTryOlderSlot() throws {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "encrypted_empty_db", withExtension: "jed", subdirectory: "Fixtures"))
        let store = MemoryBlockStore(); try store.write(at: 0, bytes: Array(Data(contentsOf: url)))
        let snapshotURL = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        let snapshot = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: snapshotURL)) as? [String: Any])
        let docs = try XCTUnwrap(snapshot["documents"] as? [String: [String: Any]])
        let vector = try XCTUnwrap(docs["spec/fileformat/encryption_vectors.toml"]?["host_kek"] as? [String: Any])
        let keyText = Array(try XCTUnwrap(vector["key_hex"] as? String))
        let key = stride(from: 0, to: keyText.count, by: 2).map { UInt8(String(keyText[$0..<$0 + 2]), radix: 16)! }
        let credential = try Credential.hostKey(key)
        let pager = try Pager.open(store: store, credential: credential)
        XCTAssertEqual(try pager.readBody(pager.selected.meta.root).first, 1)
        var meta = try store.read(at: 0, count: 1024)
        meta[19] = 2 // structurally complete higher generation, unauthenticated change
        try PageEnvelope.putU32(PageEnvelope.checksum(meta, at: 32), into: &meta, at: 32)
        try store.write(at: 0, bytes: meta)
        XCTAssertEqual(try MetaRecovery.open(store).meta.txid, 2)
        expect("XX001") { _ = try Pager.open(store: store, credential: credential) }
    }
}
