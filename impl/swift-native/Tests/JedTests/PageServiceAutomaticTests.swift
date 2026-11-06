#if os(Linux)
import Foundation
import Glibc
import XCTest
import Synchronization
@testable import Jed

final class PageServiceAutomaticTests: XCTestCase {
    private func path() throws -> String {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-auto-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
        addTeardownBlock { try FileManager.default.removeItem(at: root) }
        return root.appendingPathComponent("db.jed").path
    }
    private func sequence(_ database: Database, _ name: String) throws {
        let writer = try database.core.acquireWriter(); defer { writer.close() }
        var snapshot = writer.snapshot, image = snapshot.catalog
        image.sequences.append(CatalogSequence(name: try SQLText(name)))
        try snapshot.updateCatalog(image, schemaChanged: true)
        try writer.persist(XCTUnwrap(writer.prepare(snapshot, entropy: database.keyEntropy)))
    }
    private func names(_ database: Database) throws -> [String] {
        let pin = try database.core.beginRead(); defer { pin.close() }
        return pin.snapshot.catalog.sequences.map { $0.name.description }.sorted()
    }
    func testAutomaticOwnerPreservesClaimAndCloseRefusesLocalWriter() throws {
        let path = try path(), database = try Database.create(options: CreateOptions(path: path, pageSize: 1024,
            pageService: PageServiceOptions(shutdownGraceMilliseconds: 0)))
        let host = try XCTUnwrap(database.host as? ServiceDatabaseHost)
        XCTAssertNotNil(host.localHost); XCTAssertTrue(try XCTUnwrap(FileLockBundle.readOwner(path)).serving)
        XCTAssertThrowsError(try Database.open(path)) { XCTAssertEqual(($0 as? JedError)?.code, "55006") }
        let writer = try database.core.acquireWriter()
        XCTAssertThrowsError(try database.close()) { XCTAssertEqual(($0 as? JedError)?.code, "55006") }
        XCTAssertFalse(database.isClosed); writer.close()
        try sequence(database, "saved")
        try database.close(); XCTAssertFalse(FileManager.default.fileExists(atPath: path + ".lock/owner"))
        let reopened = try Database.open(path, options: OpenOptions(pageService: PageServiceOptions(serve: false)))
        XCTAssertEqual(try names(reopened), ["saved"])
        XCTAssertFalse(try XCTUnwrap(FileLockBundle.readOwner(path)).serving)
        try reopened.close()
    }
    func testClientPromotesAfterGracefulOwnerCloseAndKeepsCommittedBytes() throws {
        let path = try path(), owner = try Database.create(options: CreateOptions(path: path, pageSize: 1024,
            pageService: PageServiceOptions(shutdownGraceMilliseconds: 0)))
        let child = try Peer(path: path, preference: "prefer"); defer { child.stop() }
        XCTAssertEqual(try child.command("add first"), "ok")
        XCTAssertEqual(try names(owner), ["first"])
        XCTAssertEqual(try child.command("pin"), "ok")
        try owner.close()
        // Owner shutdown and the child's unrelated command pipe do not order
        // the child's socket reader. Observe EOF on the captured connection
        // before asserting that its retained pin has ended (§8.1).
        XCTAssertEqual(try child.command("old-connection"), "08006")
        XCTAssertEqual(try child.command("old-pin"), "08006")
        XCTAssertEqual(try child.command("add second"), "ok")
        XCTAssertEqual(try child.command("names"), "first,second")
        XCTAssertEqual(try XCTUnwrap(FileLockBundle.readOwner(path)).pid, UInt32(child.process.processIdentifier))
        child.stop()
        let reopened = try Database.open(path, options: OpenOptions(locking: .exclusive))
        XCTAssertEqual(try names(reopened), ["first", "second"]); try reopened.close()
    }
    func testYieldInvalidatesExistingPinsAndRequesterBecomesOwner() throws {
        let path = try path(), owner = try Database.create(options: CreateOptions(path: path, pageSize: 1024,
            pageService: PageServiceOptions(ownership: .yield, yieldGraceMilliseconds: 25, shutdownGraceMilliseconds: 0)))
        let old = try owner.core.beginRead()
        let child = try Peer(path: path, preference: "prefer"); defer { child.stop() }
        XCTAssertEqual(try child.command("add handed_off"), "ok")
        XCTAssertEqual(try XCTUnwrap(FileLockBundle.readOwner(path)).pid, UInt32(child.process.processIdentifier))
        XCTAssertThrowsError(try old.checkRole()) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }; old.close()
        XCTAssertEqual(try names(owner), ["handed_off"])
        try owner.close()
    }
    func testAcceptedYieldParksBeginsBeforeWorkerRuns() throws {
        let path = try path()
        let created = try Database.create(options: CreateOptions(path: path, pageSize: 1024, locking: .exclusive))
        try created.close()
        let platform = HeldServiceWorkPlatform()
        let owner = try ServiceDatabaseHost.openResolved(path: path,
            options: OpenOptions(pageService: PageServiceOptions(ownership: .yield,
                yieldGraceMilliseconds: 0, shutdownGraceMilliseconds: 0)),
            catalogID: 1121, platform: platform)
        defer { try? owner.close(); platform.discardWork() }
        let host = try XCTUnwrap(owner.host as? ServiceDatabaseHost)
        let localAdmission = HeldLocalAdmission(base: host)
        owner.core.coordinateRoles(with: localAdmission)
        let cancellation = CancellationToken(), completed = DispatchSemaphore(value: 0)
        let outcome = Mutex<String?>(nil)
        Thread.detachNewThread {
            do {
                let writer = try owner.core.acquireWriter(cancellation: cancellation)
                writer.close(); outcome.withLock { $0 = "acquired" }
            } catch { outcome.withLock { $0 = (error as? JedError)?.code ?? "unexpected" } }
            completed.signal()
        }
        defer { cancellation.cancel(); localAdmission.proceed.signal(); owner.core.coordinateRoles(with: host) }
        XCTAssertEqual(localAdmission.admitted.wait(timeout: .now() + 2), .success)
        let endpoint = try XCTUnwrap(FileLockBundle.readOwner(path)).endpoint
        let deadline = ContinuousClock.now.advanced(by: .seconds(2))
        let client = try PageServiceConnection.open(socket: UnixServiceSocket.connect(path: endpoint, deadline: deadline),
            hello: ServiceHello(), deadline: deadline)
        defer { client.close() }
        guard case .ownershipReply(let status) = try client.request(.requestOwnership).wait(deadline: deadline) else {
            return XCTFail("missing ownership reply")
        }
        XCTAssertEqual(status, 0)
        // This caller passed coordinator admission before the handoff request.
        // The registry gate must still stop it from becoming a local writer.
        localAdmission.proceed.signal()
        XCTAssertEqual(localAdmission.retried.wait(timeout: .now() + 2), .success)
        XCTAssertFalse(owner.core.hasLocalWriter)
        cancellation.cancel()
        XCTAssertEqual(completed.wait(timeout: .now() + 2), .success)
        XCTAssertEqual(outcome.withLock { $0 }, "57014")
        owner.core.coordinateRoles(with: host)
        XCTAssertEqual(platform.pendingWork, 1, "yield worker remains deliberately suspended")
        let server = try XCTUnwrap((owner.host as? ServiceDatabaseHost)?.serviceOwner)
        XCTAssertTrue(server.isDraining, "accepted ownership must stop admission before dispatch")
        let pendingRead = try client.request(.beginRead(.cold))
        let pendingWrite = try client.request(.beginWrite(.cold, timeout: 0, flags: 0))
        // This reply orders both earlier begin requests without letting the
        // deliberately suspended worker run or relying on a timing sleep.
        _ = try client.request(.commitStatus(txid: UInt64.max, digest: 0)).wait(deadline: deadline)
        XCTAssertEqual(owner.core.livePinCount, 0, "a read must remain parked")
        XCTAssertEqual(platform.pendingWork, 1, "a writer must not dispatch before handoff")
        client.close()
        XCTAssertThrowsError(try pendingRead.wait(deadline: deadline)) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
        XCTAssertThrowsError(try pendingWrite.wait(deadline: deadline)) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
        while server.isDraining, ContinuousClock.now < deadline { Thread.sleep(forTimeInterval: 0.001) }
        XCTAssertFalse(server.isDraining, "disconnect cancels the first reservation")
        let replacement = try PageServiceConnection.open(socket: UnixServiceSocket.connect(path: endpoint, deadline: deadline),
            hello: ServiceHello(), deadline: deadline)
        defer { replacement.close() }
        guard case .ownershipReply(let nextStatus) = try replacement.request(.requestOwnership).wait(deadline: deadline) else {
            return XCTFail("missing replacement ownership reply")
        }
        XCTAssertEqual(nextStatus, 0)
        XCTAssertEqual(platform.pendingWork, 2)
        // A canceled request's delayed worker must not reopen admissions for
        // its replacement or overwrite the replacement's reservation.
        platform.runFirstWork()
        XCTAssertTrue(server.isDraining)
        let replacementRead = try replacement.request(.beginRead(.cold))
        let replacementWrite = try replacement.request(.beginWrite(.cold, timeout: 0, flags: 0))
        _ = try replacement.request(.commitStatus(txid: UInt64.max, digest: 0)).wait(deadline: deadline)
        XCTAssertEqual(owner.core.livePinCount, 0)
        XCTAssertEqual(platform.pendingWork, 1)
        replacement.close()
        XCTAssertThrowsError(try replacementRead.wait(deadline: deadline)) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
        XCTAssertThrowsError(try replacementWrite.wait(deadline: deadline)) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
    }
    func testWriterAdmissionTimeoutAndCancellationDuringYield() throws {
        let path = try path(), owner = try Database.create(options: CreateOptions(path: path, pageSize: 1024,
            pageService: PageServiceOptions(ownership: .yield, yieldGraceMilliseconds: 500, shutdownGraceMilliseconds: 0)))
        let pin = try owner.core.beginRead()
        let server = try XCTUnwrap((owner.host as? ServiceDatabaseHost)?.serviceOwner)
        let child = try Peer(path: path, preference: "prefer"); defer { child.stop() }
        let deadline = ContinuousClock.now.advanced(by: .seconds(2))
        while !server.isDraining, ContinuousClock.now < deadline { Thread.sleep(forTimeInterval: 0.001) }
        XCTAssertTrue(server.isDraining)
        let start = ContinuousClock.now
        XCTAssertThrowsError(try owner.core.acquireWriter(timeoutMilliseconds: 1)) { XCTAssertEqual(($0 as? JedError)?.code, "55P03") }
        XCTAssertLessThan(start.duration(to: .now), .milliseconds(200))
        let cancellation = CancellationToken()
        Thread.detachNewThread { Thread.sleep(forTimeInterval: 0.01); cancellation.cancel() }
        let cancelStart = ContinuousClock.now
        XCTAssertThrowsError(try owner.core.acquireWriter(cancellation: cancellation)) { XCTAssertEqual(($0 as? JedError)?.code, "57014") }
        XCTAssertLessThan(cancelStart.duration(to: .now), .milliseconds(200))
        pin.close(); XCTAssertEqual(try child.command("names"), "")
        _ = try names(owner); try owner.close()
    }
    func testReadOnlyOwnerYieldsOnlyWhenClientBeginsWrite() throws {
        let path = try path(), created = try Database.create(options: CreateOptions(path: path, pageSize: 1024, locking: .exclusive))
        try created.close()
        let owner = try Database.open(path, options: OpenOptions(readOnly: true,
            pageService: PageServiceOptions(yieldGraceMilliseconds: 0, shutdownGraceMilliseconds: 0)))
        let child = try Peer(path: path, preference: "prefer"); defer { child.stop() }
        XCTAssertEqual(try child.command("names"), "")
        XCTAssertEqual(try XCTUnwrap(FileLockBundle.readOwner(path)).pid, UInt32(getpid()))
        XCTAssertEqual(try child.command("add writable"), "ok")
        XCTAssertEqual(try names(owner), ["writable"]); try owner.close()
    }
    func testLostAcknowledgementResolvesThroughReconnectedOwner() throws {
        let path = try path(), owner = try Database.create(options: CreateOptions(path: path, pageSize: 1024,
            pageService: PageServiceOptions(shutdownGraceMilliseconds: 0)))
        let child = try Peer(path: path, preference: "prefer"); defer { child.stop() }
        let host = try XCTUnwrap(owner.host as? ServiceDatabaseHost)
        try XCTUnwrap(host.serviceOwner).dropNextCommitAcknowledgement()
        XCTAssertEqual(try child.command("add acknowledged_after_reconnect"), "ok")
        XCTAssertEqual(try names(owner), ["acknowledged_after_reconnect"])
        try owner.close()
    }

    /// Spawn the same compiled test binary, not another engine or a SwiftPM
    /// build. This host-only peer keeps process-local path identity meaningful.
    func testChildPeer() throws {
        let environment = ProcessInfo.processInfo.environment
        guard let path = environment["JED_SWIFT_HOST_PEER"] else { return }
        let preference: OwnershipPreference = environment["JED_SWIFT_HOST_PREFERENCE"] == "yield" ? .yield : .prefer
        let database = try Database.open(path, options: OpenOptions(pageService: PageServiceOptions(ownership: preference,
            yieldGraceMilliseconds: 0, shutdownGraceMilliseconds: 0)))
        var pin: SnapshotPin?
        var pinnedConnection: PageServiceConnection?
        func reply(_ value: String) { FileHandle.standardOutput.write(Data(("PEER:" + value + "\n").utf8)) }
        reply("ready")
        while let line = readLine() {
            do {
                if line == "quit" { pin?.close(); try database.close(); reply("bye"); return }
                if line == "pin" {
                    pin = try database.core.beginRead()
                    pinnedConnection = (database.host as? ServiceDatabaseHost)?.serviceClient?.connection
                    reply("ok")
                }
                else if line == "old-connection" {
                    guard let pinnedConnection else { throw JedError("XX000", "test pin has no captured client connection") }
                    _ = try pinnedConnection.request(.commitStatus(txid: UInt64.max, digest: 0))
                        .wait(deadline: .now.advanced(by: .seconds(2)), timeoutCode: "XX000")
                    reply("ok")
                }
                else if line == "old-pin" { try pin?.checkRole(); reply("ok") }
                else if line == "names" { reply(try names(database).joined(separator: ",")) }
                else if line.hasPrefix("add ") { try sequence(database, String(line.dropFirst(4))); reply("ok") }
                else { reply("bad-command") }
            } catch { reply((error as? JedError)?.code ?? "unexpected-error") }
        }
        pin?.close(); try database.close()
    }
    private final class HeldLocalAdmission: DatabaseRoleCoordinator, Sendable {
        let base: any DatabaseRoleCoordinator
        let admitted = DispatchSemaphore(value: 0), proceed = DispatchSemaphore(value: 0)
        let retried = DispatchSemaphore(value: 0)
        private let first = Mutex(true)
        init(base: any DatabaseRoleCoordinator) { self.base = base }
        func awaitAdmission() throws { try base.awaitAdmission() }
        func awaitWriteAdmission(timeoutMilliseconds: UInt64, cancellation: CancellationToken?) throws {
            let initial = first.withLock { value in let old = value; value = false; return old }
            if !initial { retried.signal() }
            try base.awaitWriteAdmission(timeoutMilliseconds: timeoutMilliseconds, cancellation: cancellation)
            if initial {
                admitted.signal()
                guard proceed.wait(timeout: .now() + 2) == .success else { throw JedError("XX000", "test admission wait timed out") }
            }
        }
    }
    /// Real file/lock/socket devices with background work held by the test.
    /// Only worker scheduling is controlled; protocol reads still run normally.
    private final class HeldServiceWorkPlatform: ServiceHostPlatform, Sendable {
        private let base = NativeServiceHostPlatform()
        private let work = Mutex<[@Sendable () -> Void]>([])
        var operations: any FileOperations { base.operations }
        var uid: UInt32 { base.uid }
        var pid: UInt32 { base.pid }
        var backgroundElection: Bool { base.backgroundElection }
        var ownerTimers: Bool { base.ownerTimers }
        var pendingWork: Int { work.withLock { $0.count } }
        func discardWork() { work.withLock { $0.removeAll() } }
        func runFirstWork() {
            let body = work.withLock { $0.isEmpty ? nil : $0.removeFirst() }
            body?()
        }
        func incarnation() throws -> [UInt8] { try base.incarnation() }
        func dispatch(_ body: @escaping @Sendable () -> Void) { work.withLock { $0.append(body) } }
        func claim(_ path: String) throws -> any ServicePathClaim { try base.claim(path) }
        func prepare(path: String, timeout: Int64, claim: any ServicePathClaim) throws -> any ServiceOwnership {
            try base.prepare(path: path, timeout: timeout, claim: claim)
        }
        func openFile(path: String, options: OpenOptions, catalogID: UInt64, bundle: any ServiceOwnership,
                      unlockedCodec: EncryptedPageCodec?, structural: Bool, random: any RandomSource) throws -> Database {
            try base.openFile(path: path, options: options, catalogID: catalogID, bundle: bundle,
                unlockedCodec: unlockedCodec, structural: structural, random: random)
        }
        func owner(_ path: String) -> ServiceOwnerRecord? { base.owner(path) }
        func endpoint(path: String, incarnation: [UInt8]) -> String? { base.endpoint(path: path, incarnation: incarnation) }
        func listen(_ endpoint: String) throws -> any ServiceListener { try base.listen(endpoint) }
        func connect(_ endpoint: String, deadline: ContinuousClock.Instant) throws -> any ServiceFrameTransport {
            try base.connect(endpoint, deadline: deadline)
        }
        func remove(_ endpoint: String) { base.remove(endpoint) }
        func probeWritable(_ path: String) throws { try base.probeWritable(path) }
        func now() -> Int64 { base.now() }
        func pause() { base.pause() }
        func scratch(sealed: Bool, random: any RandomSource) -> any ScratchFileHost { base.scratch(sealed: sealed, random: random) }
    }
    private final class Peer {
        let process = Process()
        private let input = Pipe(), output = Pipe()
        private var buffered: [UInt8] = []
        private var stopped = false
        init(path: String, preference: String) throws {
            process.executableURL = URL(fileURLWithPath: CommandLine.arguments[0])
            process.arguments = ["JedTests.PageServiceAutomaticTests/testChildPeer"]
            var environment = ProcessInfo.processInfo.environment
            environment["JED_SWIFT_HOST_PEER"] = path; environment["JED_SWIFT_HOST_PREFERENCE"] = preference
            process.environment = environment; process.standardInput = input; process.standardOutput = output; process.standardError = output
            try process.run()
            do { guard try receive() == "ready" else { throw JedError("XX000", "test peer did not initialize") } }
            catch { stop(); throw error }
        }
        func command(_ value: String) throws -> String {
            try input.fileHandleForWriting.write(contentsOf: Data((value + "\n").utf8)); return try receive()
        }
        private func receive() throws -> String {
            let deadline = ContinuousClock.now.advanced(by: .seconds(8))
            while true {
                if let end = buffered.firstIndex(of: 10) {
                    let line = String(decoding: buffered[..<end], as: UTF8.self); buffered.removeFirst(end + 1)
                    if line.hasPrefix("PEER:") { return String(line.dropFirst(5)) }; continue
                }
                guard ContinuousClock.now < deadline else { throw JedError("XX000", "test peer response timed out") }
                var descriptor = pollfd(fd: output.fileHandleForReading.fileDescriptor, events: Int16(POLLIN), revents: 0)
                if poll(&descriptor, 1, 20) < 0 { if errno == EINTR { continue }; throw JedError("XX000", "test peer poll failed") }
                if descriptor.revents != 0 {
                    var bytes = [UInt8](repeating: 0, count: 4096)
                    let count = bytes.withUnsafeMutableBytes { Glibc.read(descriptor.fd, $0.baseAddress, $0.count) }
                    if count < 0, errno == EINTR { continue }
                    guard count > 0 else { throw JedError("XX000", "test peer ended before replying") }
                    buffered += bytes.prefix(count)
                }
            }
        }
        func stop() {
            guard !stopped else { return }; stopped = true
            if process.isRunning { _ = try? command("quit") }
            if process.isRunning { process.terminate() }
            process.waitUntilExit()
        }
        deinit { stop() }
    }
}
#endif
