import XCTest
@testable import Jed

final class CredentialWriterBoundaryTests: XCTestCase {
    private func setup() throws -> (DatabaseCore, SeededRandomSource) {
        let random = SeededRandomSource(seed: 241)
        let storage = try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: 241, pageSize: 1024,
            credential: Credential.hostKey([UInt8](repeating: 5, count: 32)), entropy: { try random.bytes(count: $0) })
        let core = DatabaseCore(storage: storage)
        try core.startService(incarnation: [UInt8](repeating: 1, count: 16))
        return (core, random)
    }
    private func replacement(_ core: DatabaseCore, random: SeededRandomSource) throws -> KeyBlock {
        let selected = try XCTUnwrap(core.storage.pager.selectedKeyBlock)
        return try XCTUnwrap(core.storage.pager.codec as? EncryptedPageCodec).rewrap(selected,
            credential: Credential.hostKey([UInt8](repeating: 7, count: 32)),
            parameters: .default, entropy: { try random.bytes(count: $0) })
    }
    func testCredentialAdmissionExcludesLocalWriterButPermitsIdleServiceGrant() throws {
        let (core, random) = try setup(), before = try XCTUnwrap(core.storage.pager.selectedKeyBlock)
        let local = try core.acquireWriter(wait: false)
        XCTAssertThrowsError(try core.credentialMaintenance {}) { XCTAssertEqual(($0 as? JedError)?.code, "55006") }
        local.close()
        let service = try core.serviceBeginWrite(.cold, relaxed: false, timeoutMilliseconds: 0, cancellation: CancellationToken())
        defer { service.close() }
        let block = try replacement(core, random: random)
        try core.credentialMaintenance {
            XCTAssertThrowsError(try core.acquireWriter(wait: false)) { XCTAssertEqual(($0 as? JedError)?.code, "55006") }
            try core.storage.pager.changeKeyBlock(block, replacing: before, syncDirectory: {})
        }
        XCTAssertEqual(core.storage.pager.selectedKeyBlock?.generation, before.generation + 1)
        XCTAssertEqual(core.version, service.selected.meta.txid)
        try core.serviceCheckWriter(service)
        XCTAssertFalse(core.storage.pager.isPoisoned)
    }

    func testStreamedDeltaRefusesKeyWritesUntilAbortWithoutPoisoning() throws {
        let (core, random) = try setup()
        var working = core.storage.committed, catalog = working.catalog
        catalog.sequences.append(CatalogSequence(name: try SQLText("pending")))
        try working.updateCatalog(catalog, schemaChanged: true)
        let entropy: KeyEntropy = { try random.bytes(count: $0) }
        let plan = try XCTUnwrap(core.storage.prepare(working, entropy: entropy))
        let service = try core.serviceBeginWrite(.cold, relaxed: false, timeoutMilliseconds: 0, cancellation: CancellationToken())
        defer { service.close() }
        let begin = ServiceCommitBegin(txid: plan.snapshot.txid, meta: plan.pages.metaBytes,
            manifest: plan.pages.manifest.pages.map { StoredPage(index: $0.pageIndex, bytes: $0.bytes) })
        _ = try service.begin(begin)
        let before = try XCTUnwrap(core.storage.pager.selectedKeyBlock), block = try replacement(core, random: random)
        XCTAssertThrowsError(try core.credentialMaintenance {
            try core.storage.pager.changeKeyBlock(block, replacing: before, syncDirectory: {})
        }) { XCTAssertEqual(($0 as? JedError)?.code, "58030") }
        XCTAssertFalse(core.storage.pager.isPoisoned)
        XCTAssertEqual(core.storage.pager.selectedKeyBlock, before)
        service.close()
        try core.credentialMaintenance { try core.storage.pager.changeKeyBlock(block, replacing: before, syncDirectory: {}) }
        XCTAssertEqual(core.storage.pager.selectedKeyBlock?.generation, before.generation + 1)
    }
}
