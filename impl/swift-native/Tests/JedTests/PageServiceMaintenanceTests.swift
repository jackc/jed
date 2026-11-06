import XCTest
@testable import Jed

final class PageServiceMaintenanceTests: XCTestCase {
    private func setup(encrypted: Bool = false, environment: Environment? = nil) throws -> (DatabaseCore, KeyEntropy) {
        let random = SeededRandomSource(seed: 909), entropy: KeyEntropy = { try random.bytes(count: $0) }
        let storage = try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: 1301, pageSize: 1024,
            credential: encrypted ? Credential.hostKey([UInt8](repeating: 12, count: 32)) : nil, entropy: entropy)
        let core = DatabaseCore(storage: storage, environment: environment)
        try core.startService(incarnation: [UInt8](repeating: 10, count: 16))
        return (core, entropy)
    }
    private func clientCommit(_ core: DatabaseCore, entropy: @escaping KeyEntropy, ordinal: Int,
                              relaxed: Bool = false, missingCollation: Bool = false) throws -> (UInt64, UInt64) {
        // This is a native page-delta fixture, independent of SQL execution.
        var snapshot = try core.storage.adoptServiceSnapshot(core.storage.pager.selected, environment: nil)
        var image = snapshot.catalog
        image.sequences.append(CatalogSequence(name: try SQLText("sequence_\(ordinal)")))
        if missingCollation && image.collations.isEmpty {
            image.collations.append(CatalogCollation(name: try SQLText("unavailable"), unicode: try SQLText("17.0.0"),
                cldr: try SQLText("48"), description: try SQLText("host-only maintenance fixture")))
        }
        try snapshot.updateCatalog(image, schemaChanged: true)
        let lease = try core.serviceBeginWrite(.cold, relaxed: relaxed, timeoutMilliseconds: 0, cancellation: CancellationToken())
        defer { lease.close() }
        let plan = try XCTUnwrap(core.storage.prepare(snapshot, policy: DomainCommitPolicy(reclaimAllowed: false), entropy: entropy))
        let begin = ServiceCommitBegin(txid: plan.snapshot.txid, meta: plan.pages.metaBytes,
            manifest: plan.pages.manifest.pages.map { StoredPage(index: $0.pageIndex, bytes: $0.bytes) })
        var delta = try lease.begin(begin)
        for page in plan.pages.body { try delta.receive(page); try lease.write(page) }
        let acknowledged = try lease.finish(delta, entropy: entropy)
        let digest = try ServiceIncomingDelta.resolutionDigest(plan.pages.metaBytes)
        try lease.afterAcknowledgement(entropy: entropy, synchronous: !relaxed)
        XCTAssertThrowsError(try core.acquireWriter(wait: false)) { XCTAssertEqual(($0 as? JedError)?.code, "55006") }
        XCTAssertEqual(try core.serviceStatus(txid: acknowledged, digest: digest), 0)
        XCTAssertTrue(core.version == acknowledged || core.version == acknowledged + 1)
        return (acknowledged, core.version)
    }
    func testReaderWatermarkBlocksMaintenanceAndGateRemainsHeldAfterAck() throws {
        let (core, entropy) = try setup(), pin = try core.serviceBeginRead(.cold)
        for ordinal in 0..<22 {
            let result = try clientCommit(core, entropy: entropy, ordinal: ordinal)
            XCTAssertEqual(result.0, result.1)
        }
        XCTAssertTrue(try core.storage.serviceMaintenanceTrigger().needed)
        pin.close()
        let result = try clientCommit(core, entropy: entropy, ordinal: 22)
        XCTAssertEqual(result.1, result.0 + 1)
        XCTAssertEqual(core.livePinCount, 0)
        XCTAssertFalse(try core.storage.pager.allocationSnapshot(reuseAllowed: true).persistedFree.isEmpty)
        let local = try core.beginRead(); defer { local.close() }
        XCTAssertEqual(local.snapshot.catalog.sequences.count, 23)
    }
    func testEncryptedMaintenanceNeverUsesOwnerEnvironmentAndDoesNotBypassLaterValidation() throws {
        let (core, entropy) = try setup(encrypted: true, environment: Environment.builder().build())
        var maintained = false
        for ordinal in 0..<24 {
            let result = try clientCommit(core, entropy: entropy, ordinal: ordinal, missingCollation: true)
            maintained = maintained || result.1 > result.0
        }
        XCTAssertTrue(maintained)
        XCTAssertThrowsError(try core.beginRead()) { XCTAssertEqual(($0 as? JedError)?.code, "XX002") }
        XCTAssertFalse(core.storage.pager.isPoisoned)
        let read = try core.serviceBeginRead(.cold); defer { read.close() }
        XCTAssertEqual(read.selected.meta.txid, core.version)
    }
    func testOwnerRelaxedMaintenanceRetainsPendingBatch() throws {
        let (core, entropy) = try setup()
        var maintained = false
        for ordinal in 0..<24 {
            let result = try clientCommit(core, entropy: entropy, ordinal: ordinal, relaxed: true)
            if result.1 > result.0 { maintained = true; break }
        }
        XCTAssertTrue(maintained)
        XCTAssertFalse(core.storage.pager.pendingEntries.isEmpty)
        try core.storage.pager.flush(shrink: false)
        XCTAssertTrue(core.storage.pager.pendingEntries.isEmpty)
    }
}
