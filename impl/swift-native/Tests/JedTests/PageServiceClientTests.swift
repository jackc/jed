#if os(Linux)
import Foundation
import XCTest
import Synchronization
@testable import Jed

private final class RekeyRecordedEntropy: RandomSource {
    private let source = SeededRandomSource(seed: 311)
    private let counts = Mutex<[Int]>([])
    func bytes(count: Int) throws -> [UInt8] {
        counts.withLock { $0.append(count) }
        return try source.bytes(count: count)
    }
    var requests: [Int] { counts.withLock { $0 } }
}

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

    func testTransactionOwnedCursorBorrowsWriterPinAndExpiresWithItsToken() throws {
        let pair = try setup(), writer = try pair.client.acquireWriter()
        let borrowed = try writer.pin(writer.snapshot, borrowingWriter: true)
        let retained = try borrowed.retain()
        try barrier(pair.role)
        XCTAssertEqual(pair.owner.livePinCount, 1, "borrowing must not create a separate owner read pin")
        borrowed.close(); try barrier(pair.role)
        XCTAssertEqual(pair.owner.livePinCount, 1)
        writer.close(); try barrier(pair.role)
        XCTAssertEqual(pair.owner.livePinCount, 0)
        XCTAssertThrowsError(try retained.checkRole()) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
        retained.close(); try barrier(pair.role)
        XCTAssertEqual(pair.owner.livePinCount, 0)
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

    func testRejectedClientRekeyConsumesKeyPreparationBeforeOwnerAdmission() throws {
        let pair = try setup(encrypted: true), random = RekeyRecordedEntropy()
        let database = Database(core: pair.client, environment: Environment.builder().build(), keySource: random)
        defer { try? database.close() }
        let before = pair.client.storage.pager.selectedKeyBlock, version = database.version
        let credential = try Credential.hostKey([UInt8](repeating: 3, count: 32))
        XCTAssertThrowsError(try database.rekey(credential: credential)) { XCTAssertEqual(($0 as? JedError)?.code, "55006") }
        XCTAssertEqual(random.requests, [32, 32, 12])
        XCTAssertEqual(pair.client.storage.pager.selectedKeyBlock, before)
        XCTAssertEqual(database.version, version)
        XCTAssertThrowsError(try database.rekey(credential: nil)) { XCTAssertEqual(($0 as? JedError)?.code, "55006") }
        XCTAssertEqual(random.requests, [32, 32, 12], "a plaintext target draws no key material")
        let reader = try database.readSession(); reader.close()
        XCTAssertFalse(pair.client.storage.pager.isPoisoned)
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

    func testPublicIntegrityUsesRemoteSnapshotAndReleasesOwnerPin() throws {
        for encrypted in [false, true] {
            let pair = try setup(encrypted: encrypted)
            let owner = Database(core: pair.owner, environment: Environment.builder().build())
            let client = Database(core: pair.client, environment: Environment.builder().build())
            defer { try? client.close(); try? owner.close() }
            try owner.execute("CREATE TABLE checked (id i32 PRIMARY KEY, value text)")
            try owner.execute("INSERT INTO checked VALUES (1, repeat('x', 20000))")
            for level in [IntegrityLevel.quick, .full] {
                let report = try client.integrityCheck(options: .init(level: level, constraints: true, maxFindings: 0))
                XCTAssertTrue(report.ok, "\(encrypted) \(level): \(report.findings)")
                XCTAssertEqual(report.rows, 1)
                try barrier(pair.role)
                XCTAssertEqual(pair.owner.livePinCount, 0)
            }
            let writer = try owner.writeSession()
            try writer.execute("INSERT INTO checked VALUES (2, 'pending')")
            XCTAssertEqual(try client.integrityCheck().rows, 1)
            try writer.commit(); writer.close()
            XCTAssertEqual(try client.integrityCheck().rows, 2)
            try barrier(pair.role)
            XCTAssertEqual(pair.owner.livePinCount, 0)
            pair.role.close()
            XCTAssertThrowsError(try client.integrityCheck()) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
            XCTAssertEqual(pair.client.livePinCount, 0)
        }
    }
}
#endif
