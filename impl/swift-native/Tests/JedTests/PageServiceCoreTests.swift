import XCTest
@testable import Jed

final class PageServiceCoreTests: XCTestCase {
    private let incarnation = [UInt8](repeating: 7, count: 16)
    private func setup(encrypted: Bool = false, environment: Environment? = nil) throws -> (DatabaseCore, any RandomSource) {
        let random = SeededRandomSource(seed: 119)
        let storage = try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: 1001, pageSize: 1024,
            credential: encrypted ? Credential.hostKey([UInt8](repeating: 5, count: 32)) : nil,
            entropy: { try random.bytes(count: $0) })
        let core = DatabaseCore(storage: storage, environment: environment)
        try core.startService(incarnation: incarnation)
        return (core, random)
    }
    private func plan(_ core: DatabaseCore, entropy: KeyEntropy?, collation: Bool = false) throws -> PreparedDomainCommit {
        var snapshot = core.storage.committed, catalog = snapshot.catalog
        catalog.sequences.append(CatalogSequence(name: try SQLText("external_sequence")))
        if collation { catalog.collations.append(CatalogCollation(name: try SQLText("unavailable"), unicode: try SQLText("17.0.0"), cldr: try SQLText("48"), description: try SQLText("test"))) }
        try snapshot.updateCatalog(catalog, schemaChanged: true)
        return try XCTUnwrap(core.storage.prepare(snapshot,
            policy: DomainCommitPolicy(reclaimAllowed: false), entropy: entropy))
    }
    private func transmit(_ plan: PreparedDomainCommit, lease: ServiceWriterLease, entropy: KeyEntropy?) throws -> UInt64 {
        let begin = ServiceCommitBegin(txid: plan.snapshot.txid, meta: plan.pages.metaBytes,
            manifest: plan.pages.manifest.pages.map { StoredPage(index: $0.pageIndex, bytes: $0.bytes) })
        var incoming = try lease.begin(begin)
        for page in plan.pages.body {
            try incoming.receive(page)
            try lease.write(page)
        }
        return try lease.finish(incoming, entropy: entropy)
    }
    func testForeignCommitPublishesMetadataBeforeSQLAdoptionAndPreservesOldPin() throws {
        let (core, random) = try setup(), before = try core.beginRead()
        defer { before.close() }
        let plan = try plan(core, entropy: { try random.bytes(count: $0) })
        let writer = try core.serviceBeginWrite(.cold, relaxed: false, timeoutMilliseconds: 0, cancellation: CancellationToken())
        XCTAssertEqual(core.livePinCount, 2)
        let txid = try transmit(plan, lease: writer, entropy: nil)
        XCTAssertEqual(txid, 2); XCTAssertEqual(core.version, 2)
        XCTAssertEqual(core.storage.committed.txid, 1)
        let service = try core.serviceBeginRead(ServiceCachePoint(incarnation: incarnation, txid: 1))
        defer { service.close() }
        XCTAssertEqual(service.reply.invalidate, 0)
        XCTAssertEqual(service.reply.indices, Set(plan.pages.body.map(\.index)).union(plan.pages.manifest.pages.map(\.pageIndex)).sorted())
        XCTAssertEqual(before.snapshot.txid, 1)
        writer.close()
        let after = try core.beginRead(); defer { after.close() }
        XCTAssertEqual(after.snapshot.txid, 2)
        XCTAssertEqual(after.snapshot.catalog.sequences.map { $0.name.description }, ["external_sequence"])
        XCTAssertEqual(core.oldestLiveVersion, 1)
        let status = try core.serviceStatus(txid: 2, digest: ServiceIncomingDelta.resolutionDigest(plan.pages.metaBytes))
        XCTAssertEqual(status, 0)
    }
    func testEncryptedOwnerResealsMetaAndRelaxedBeginFlushesOnlyWhenRequested() throws {
        let (core, random) = try setup(encrypted: true)
        let entropy: KeyEntropy = { try random.bytes(count: $0) }
        let plan = try plan(core, entropy: entropy)
        let writer = try core.serviceBeginWrite(.cold, relaxed: true, timeoutMilliseconds: 0, cancellation: CancellationToken())
        _ = try transmit(plan, lease: writer, entropy: entropy)
        XCTAssertFalse(core.storage.pager.pendingEntries.isEmpty)
        XCTAssertNotEqual(core.storage.pager.selected.bytes, plan.pages.metaBytes)
        XCTAssertEqual(PageServiceHistory.digest(core.storage.pager.selected), try ServiceIncomingDelta.resolutionDigest(plan.pages.metaBytes))
        writer.close()
        let relaxed = try core.serviceBeginWrite(.cold, relaxed: true, timeoutMilliseconds: 0, cancellation: CancellationToken())
        XCTAssertFalse(relaxed.reply.pending.isEmpty); relaxed.close()
        let durable = try core.serviceBeginWrite(.cold, relaxed: false, timeoutMilliseconds: 0, cancellation: CancellationToken())
        XCTAssertTrue(durable.reply.pending.isEmpty)
        XCTAssertTrue(core.storage.pager.pendingEntries.isEmpty); durable.close()
    }
    func testOwnerEnvironmentCannotRejectDurableForeignMetadata() throws {
        let (core, random) = try setup(environment: Environment.builder().build())
        let plan = try plan(core, entropy: { try random.bytes(count: $0) }, collation: true)
        let writer = try core.serviceBeginWrite(.cold, relaxed: false, timeoutMilliseconds: 0, cancellation: CancellationToken())
        _ = try transmit(plan, lease: writer, entropy: nil); writer.close()
        XCTAssertThrowsError(try core.beginRead()) { XCTAssertEqual(($0 as? JedError)?.code, "XX002") }
        XCTAssertEqual(core.version, 2)
        let client = try core.serviceBeginRead(.cold); defer { client.close() }
        XCTAssertEqual(client.selected.meta.txid, 2)
        XCTAssertFalse(core.storage.pager.isPoisoned)
    }
    func testIncompleteOrAbandonedDeltaNeverChangesPublishedGeneration() throws {
        let (core, random) = try setup(), plan = try plan(core, entropy: { try random.bytes(count: $0) })
        let writer = try core.serviceBeginWrite(.cold, relaxed: false, timeoutMilliseconds: 0, cancellation: CancellationToken())
        let begin = ServiceCommitBegin(txid: plan.snapshot.txid, meta: plan.pages.metaBytes,
            manifest: plan.pages.manifest.pages.map { StoredPage(index: $0.pageIndex, bytes: $0.bytes) })
        let incomplete = try writer.begin(begin)
        XCTAssertThrowsError(try writer.finish(incomplete, entropy: nil)) { XCTAssertEqual(($0 as? JedError)?.code, "08P01") }
        writer.close()
        XCTAssertEqual(core.version, 1); XCTAssertFalse(core.storage.pager.isPoisoned)
        let local = try core.acquireWriter(wait: false); local.close()
    }
}
