#if os(Linux)
import Foundation
import XCTest
@testable import Jed

final class PageServiceOwnerTests: XCTestCase {
    private func setup(limits: PageServiceOwnerLimits = .init()) throws -> (DatabaseCore, PageServiceOwner, String) {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-service-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        addTeardownBlock { try FileManager.default.removeItem(at: directory) }
        let endpoint = directory.appendingPathComponent("s").path
        let storage = try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: 1031, pageSize: 1024)
        let core = DatabaseCore(storage: storage)
        try core.startService(incarnation: [UInt8](repeating: 8, count: 16))
        let random = SeededRandomSource(seed: 107)
        let owner = PageServiceOwner(core: core, listener: try UnixServiceSocket.listen(path: endpoint),
            limits: limits, entropy: { try random.bytes(count: $0) })
        owner.start(); addTeardownBlock { owner.stop() }
        return (core, owner, endpoint)
    }
    private func connect(_ endpoint: String, hello: ServiceHello = .init()) throws -> PageServiceConnection {
        let deadline = ContinuousClock.now.advanced(by: .seconds(2))
        return try PageServiceConnection.open(socket: UnixServiceSocket.connect(path: endpoint, deadline: deadline), hello: hello, deadline: deadline)
    }
    func testRealOwnerMultiplexesReadWhileWaitingForLocalWriterAndAcceptsDelta() throws {
        let (core, _, endpoint) = try setup(), client = try connect(endpoint)
        defer { client.close() }
        guard case .readBegun(let read) = try client.request(.beginRead(.cold)).wait() else { return XCTFail("missing read reply") }
        let local = try core.acquireWriter()
        let pending = try client.request(.beginWrite(read.point, timeout: 5000, flags: 0))
        let page = try client.request(.readPages(txid: read.point.txid, indices: [2])).wait()
        guard case .pages(let bytes) = page else { return XCTFail("reader stalled behind writer") }
        XCTAssertEqual(bytes.count, 1); XCTAssertEqual(bytes[0].count, 1024)
        local.close()
        guard case .writeBegun(let write) = try pending.wait() else { return XCTFail("missing writer reply") }
        var snapshot = core.storage.committed, catalog = snapshot.catalog
        catalog.sequences.append(CatalogSequence(name: try SQLText("remote")))
        try snapshot.updateCatalog(catalog, schemaChanged: true)
        let prepared = try XCTUnwrap(core.storage.prepare(snapshot, policy: DomainCommitPolicy(reclaimAllowed: false)))
        let begin = ServiceCommitBegin(txid: prepared.snapshot.txid, meta: prepared.pages.metaBytes,
            manifest: prepared.pages.manifest.pages.map { StoredPage(index: $0.pageIndex, bytes: $0.bytes) })
        var endSent = false
        guard case .committed(let txid) = try client.sendCommit(begin, body: prepared.pages.body, endSent: &endSent).wait() else {
            return XCTFail("commit not acknowledged")
        }
        XCTAssertTrue(endSent); XCTAssertEqual(txid, write.read.point.txid + 1)
        XCTAssertEqual(core.version, txid)
        try client.notify(.endRead(read.point.txid))
        guard case .commitStatusReply(let status) = try client.request(.commitStatus(txid: txid,
            digest: ServiceIncomingDelta.resolutionDigest(prepared.pages.metaBytes))).wait() else { return XCTFail("missing status") }
        XCTAssertEqual(status, 0)
        let current = try core.beginRead(); defer { current.close() }
        XCTAssertEqual(current.snapshot.catalog.sequences.first?.name.description, "remote")
    }
    func testHelloLimitsReadOnlyAndMalformedPinFailures() throws {
        let (_, _, endpoint) = try setup(limits: PageServiceOwnerLimits(maxConnections: 1))
        let client = try connect(endpoint, hello: ServiceHello(flags: 1)); defer { client.close() }
        XCTAssertThrowsError(try connect(endpoint)) { XCTAssertEqual(($0 as? JedError)?.code, "53300") }
        XCTAssertThrowsError(try client.request(.beginWrite(.cold, timeout: 0, flags: 0)).wait()) {
            XCTAssertEqual(($0 as? JedError)?.code, "25006")
        }
        XCTAssertThrowsError(try client.request(.readPages(txid: 1, indices: [2])).wait()) {
            XCTAssertEqual(($0 as? JedError)?.code, "08P01")
        }
    }
    func testGateTimeoutIsNonfatalAndIdleWriteRevocationReleasesLocalWriter() throws {
        let (core, _, endpoint) = try setup(limits: PageServiceOwnerLimits(idleWriteMilliseconds: 20))
        let client = try connect(endpoint); defer { client.close() }
        let held = try core.acquireWriter()
        XCTAssertThrowsError(try client.request(.beginWrite(.cold, timeout: 10, flags: 0)).wait()) {
            XCTAssertEqual(($0 as? JedError)?.code, "55P03")
        }
        held.close()
        guard case .writeBegun(let write) = try client.request(.beginWrite(.cold, timeout: 5000, flags: 0)).wait() else {
            return XCTFail("missing writer")
        }
        // Native host timers are deliberately outside deterministic SQL cost.
        Thread.sleep(forTimeInterval: 0.08)
        XCTAssertThrowsError(try client.request(.readPages(txid: write.read.point.txid, indices: [2])).wait()) {
            XCTAssertEqual(($0 as? JedError)?.code, "25P03")
        }
        let local = try core.acquireWriter(wait: false); local.close()
        guard case .readBegun(let read) = try client.request(.beginRead(.cold)).wait() else { return XCTFail("connection was lost") }
        try client.notify(.endRead(read.point.txid))
    }
}
#endif
