import XCTest
@testable import Jed

final class PageServiceDeltaTests: XCTestCase {
    private func domain(encrypted: Bool) throws -> StorageDomain {
        let source = SeededRandomSource(seed: 88)
        return try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: 901, pageSize: 1024,
            credential: encrypted ? Credential.hostKey([UInt8](repeating: 9, count: 32)) : nil,
            entropy: { try source.bytes(count: $0) })
    }
    private func prepare(_ pager: Pager) throws -> PreparedPageCommit {
        let index = pager.selected.meta.pageCount, random = SeededRandomSource(seed: UInt64(index))
        let entropy: KeyEntropy = { try random.bytes(count: $0) }
        let body = try PageSealer.seal(codec: pager.codec, index: index, type: 1, count: 0, payload: [], entropy: entropy)
        return try pager.prepare(body: [body.page], root: body.reference, pageCount: index + 1,
            freeList: PageSealer.nullReference(pager.codec), livePages: 1, freePages: [], manifestPages: [],
            reuseAllowed: false, entropy: entropy)
    }
    private func base(_ pager: Pager) -> ServiceDeltaBase {
        ServiceDeltaBase(txid: pager.selected.meta.txid, pageCount: pager.selected.meta.pageCount, codec: pager.codec,
            granted: [], pending: Dictionary(uniqueKeysWithValues: pager.pendingEntries.map { ($0.pageIndex, $0.digest) }))
    }
    private func begin(_ plan: PreparedPageCommit) -> ServiceCommitBegin {
        ServiceCommitBegin(txid: plan.manifest.meta.txid, meta: plan.metaBytes,
            manifest: plan.manifest.pages.map { StoredPage(index: $0.pageIndex, bytes: $0.bytes) })
    }
    private func fails(_ action: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try action(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, "08P01", file: file, line: line) }
    }
    func testStreamingDeltaRejectsUnsafeMissingDuplicateAndChangedPages() throws {
        let storage = try domain(encrypted: false), pager = storage.pager
        let plan = try prepare(pager), fixed = base(pager)
        var delta = try ServiceIncomingDelta(base: fixed, begin: begin(plan))
        fails { try delta.complete() }
        var damaged = plan.body[0].bytes; damaged[20] ^= 1
        fails { try delta.receive(StoredPage(index: plan.body[0].index, bytes: damaged)) }
        fails { try delta.receive(StoredPage(index: 2, bytes: plan.body[0].bytes)) }
        try delta.receive(plan.body[0]); try delta.complete()
        fails { try delta.receive(plan.body[0]) }
        XCTAssertEqual(delta.written, plan.body.map(\.index))
        var premature = begin(plan); premature.txid += 1
        fails { _ = try ServiceIncomingDelta(base: fixed, begin: premature) }
        let manifest = try CommitManifest.build(pageSize: 1024, encrypted: false, txid: fixed.txid + 1,
            root: PageReference(2), pageCount: fixed.pageCount, freeList: PageReference(0), livePages: 1,
            entries: [ManifestEntry(pageIndex: 2, digest: Checksums.crc64(plan.body[0].bytes))], chain: [])
        let unsafe = try ServiceCommitBegin(txid: fixed.txid + 1, meta: manifest.meta.encode(inline: manifest.inline), manifest: [])
        fails { _ = try ServiceIncomingDelta(base: fixed, begin: unsafe) }
    }
    func testRelaxedDeltaCarriesPendingIdentitiesWithoutReceivingThemAgain() throws {
        let storage = try domain(encrypted: false), pager = storage.pager, first = try prepare(pager)
        try pager.commit(first, synchronous: false)
        let fixed = base(pager), second = try prepare(pager)
        XCTAssertEqual(fixed.pending.count, 1)
        var delta = try ServiceIncomingDelta(base: fixed, begin: begin(second))
        fails { try delta.receive(first.body[0]) }
        try delta.receive(second.body[0]); try delta.complete()
        XCTAssertEqual(delta.ownEntries.map(\.pageIndex), second.body.map(\.index))
        let forged = ServiceDeltaBase(txid: fixed.txid, pageCount: fixed.pageCount, codec: fixed.codec,
            granted: [], pending: fixed.pending.mapValues { $0 ^ 1 })
        fails { _ = try ServiceIncomingDelta(base: forged, begin: begin(second)) }
    }
    func testEncryptedDeltaAuthenticatesEitherSlotAndResolutionIgnoresResealing() throws {
        let storage = try domain(encrypted: true), pager = storage.pager, plan = try prepare(pager)
        let codec = try XCTUnwrap(pager.codec as? EncryptedPageCodec)
        var delta = try ServiceIncomingDelta(base: base(pager), begin: begin(plan))
        try delta.receive(plan.body[0]); try delta.complete()
        let first = try codec.sealMeta(plan.metaBytes, slot: 0, manifest: plan.manifest.pages, nonce: [UInt8](repeating: 3, count: 24))
        let second = try codec.sealMeta(plan.metaBytes, slot: 1, manifest: plan.manifest.pages, nonce: [UInt8](repeating: 4, count: 24))
        XCTAssertNotEqual(first, second)
        XCTAssertEqual(try ServiceIncomingDelta.resolutionDigest(first), try ServiceIncomingDelta.resolutionDigest(second))
        for bytes in [first, second] {
            var submitted = begin(plan); submitted.meta = bytes
            _ = try ServiceIncomingDelta(base: base(pager), begin: submitted)
        }
        var corrupted = begin(plan)
        corrupted.meta[corrupted.meta.count - 1] ^= 1
        try PageEnvelope.putU32(PageEnvelope.checksum(corrupted.meta, at: 32), into: &corrupted.meta, at: 32)
        fails { _ = try ServiceIncomingDelta(base: base(pager), begin: corrupted) }
    }
}
