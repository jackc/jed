#if os(Linux)
import Foundation
import XCTest
@testable import Jed

final class PageServiceClientTests: XCTestCase {
    private func setup(encrypted: Bool = false, limits: PageServiceOwnerLimits = .init()) throws -> (owner: DatabaseCore, client: DatabaseCore, role: PageServiceClient) {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-client-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        addTeardownBlock { try FileManager.default.removeItem(at: directory) }
        let credential: Credential? = try encrypted ? Credential.hostKey([UInt8](repeating: 9, count: 32)) : nil
        let random = SeededRandomSource(seed: 140)
        let storage = try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: 1101, pageSize: 1024,
            credential: credential, entropy: { try random.bytes(count: $0) })
        let owner = DatabaseCore(storage: storage)
        try owner.startService(incarnation: [UInt8](repeating: 5, count: 16))
        let endpoint = directory.appendingPathComponent("s").path
        let server = PageServiceOwner(core: owner, listener: try UnixServiceSocket.listen(path: endpoint),
            limits: limits, entropy: { try random.bytes(count: $0) })
        server.start(); addTeardownBlock { server.stop() }
        let deadline = ContinuousClock.now.advanced(by: .seconds(2))
        let connection = try PageServiceConnection.open(socket: UnixServiceSocket.connect(path: endpoint, deadline: deadline),
            hello: ServiceHello(), deadline: deadline)
        let remote = try PageServiceClient.open(connection: connection, credential: credential,
            environment: Environment.builder().build(), catalogID: 1102, cacheBytes: 4096)
        addTeardownBlock { remote.client.close() }
        try barrier(remote.client)
        return (owner, remote.core, remote.client)
    }
    private func barrier(_ role: PageServiceClient) throws {
        _ = try role.connection.request(.commitStatus(txid: UInt64.max, digest: 0)).wait()
    }
    private func appendSequence(_ core: DatabaseCore, _ name: String, entropy: KeyEntropy? = nil) throws {
        let writer = try core.acquireWriter(); defer { writer.close() }
        var snapshot = writer.snapshot, image = snapshot.catalog
        image.sequences.append(CatalogSequence(name: try SQLText(name)))
        try snapshot.updateCatalog(image, schemaChanged: true)
        let prepared = try XCTUnwrap(writer.prepare(snapshot, entropy: entropy))
        try writer.persist(prepared)
    }
    func testIdleClientPinsNothingAndLocalReadersShareOneOwnerPin() throws {
        let pair = try setup()
        XCTAssertEqual(pair.owner.livePinCount, 0)
        let first = try pair.client.beginRead(), second = try pair.client.beginRead(), retained = try first.retain()
        try barrier(pair.role); XCTAssertEqual(pair.owner.livePinCount, 1)
        first.close(); second.close(); try barrier(pair.role); XCTAssertEqual(pair.owner.livePinCount, 1)
        retained.close(); try barrier(pair.role); XCTAssertEqual(pair.owner.livePinCount, 0)
        let writer = try pair.client.acquireWriter()
        let cursor = try writer.pin(writer.snapshot)
        try barrier(pair.role); XCTAssertEqual(pair.owner.livePinCount, 2)
        writer.close(); try barrier(pair.role); XCTAssertEqual(pair.owner.livePinCount, 1)
        cursor.close(); try barrier(pair.role); XCTAssertEqual(pair.owner.livePinCount, 0)
    }
    func testNativeClientPlansCommitsAndRefreshesOwnerWritesInBothPageVariants() throws {
        for encrypted in [false, true] {
            let pair = try setup(encrypted: encrypted)
            let random = SeededRandomSource(seed: 141), entropy: KeyEntropy = { try random.bytes(count: $0) }
            try appendSequence(pair.client, "from_client", entropy: entropy)
            XCTAssertEqual(pair.owner.version, 2); XCTAssertEqual(pair.client.version, 2)
            let ownerRead = try pair.owner.beginRead()
            XCTAssertEqual(ownerRead.snapshot.catalog.sequences.map { $0.name.description }, ["from_client"])
            ownerRead.close()
            try appendSequence(pair.owner, "from_owner", entropy: entropy)
            let refreshed = try pair.client.beginRead()
            XCTAssertEqual(refreshed.snapshot.txid, 3)
            XCTAssertEqual(refreshed.snapshot.catalog.sequences.map { $0.name.description }.sorted(), ["from_client", "from_owner"])
            refreshed.close(); try barrier(pair.role)
            XCTAssertEqual(pair.owner.livePinCount, 0)
        }
    }
    func testExpiredWriterCannotLeakNewerReadPinDuringCursorRetention() throws {
        let pair = try setup(limits: .init(idleWriteMilliseconds: 20))
        let writer = try pair.client.acquireWriter(); defer { writer.close() }
        Thread.sleep(forTimeInterval: 0.08)
        try appendSequence(pair.owner, "after_expiry")
        XCTAssertThrowsError(try writer.pin(writer.snapshot)) { XCTAssertEqual(($0 as? JedError)?.code, "25P03") }
        try barrier(pair.role)
        XCTAssertEqual(pair.owner.livePinCount, 0)
        XCTAssertFalse(pair.role.connection.isClosed)
        writer.close(); try barrier(pair.role)
        let current = try pair.client.beginRead(); current.close(); try barrier(pair.role)
        XCTAssertEqual(pair.owner.livePinCount, 0)
    }

    func testEndedClientRoleRefusesStatementAndCommitButAllowsRollback() throws {
        let pair = try setup(), database = Database(core: pair.client, environment: Environment.builder().build())
        let session = try database.session(), reader = try database.readSession()
        try session.begin()
        pair.role.close()
        XCTAssertThrowsError(try reader.query("SELECT 1")) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
        XCTAssertThrowsError(try session.commit()) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
        try session.rollback()
        XCTAssertEqual(session.status, .idle)
        try database.close()
    }
}
#endif
