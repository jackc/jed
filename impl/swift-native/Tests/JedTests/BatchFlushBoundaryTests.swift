import Foundation
import Synchronization
import XCTest
@testable import Jed

/// Clock counts and commit-phase boundaries are host contracts, not SQL cases.
final class BatchFlushBoundaryTests: XCTestCase {
    private final class Clock: ClockSource {
        struct State { var now: Int64 = 10_000; var reads = 0 }
        private let state = Mutex(State())
        func microseconds() -> Int64 { state.withLock { $0.reads += 1; return $0.now } }
        var reads: Int { state.withLock { $0.reads } }
        func set(_ value: Int64) { state.withLock { $0.now = value } }
    }
    private func file(_ clock: Clock, interval: UInt64 = 10) throws -> (Database, SimFilesystem) {
        var builder = Environment.builder(); builder.setClockSource(clock)
        let filesystem = try SimFilesystem(seed: 93, pageSize: 4096)
        let database = try FileDatabaseHost.createResolved(path: "sim.jed",
            options: CreateOptions(environment: builder.build(), pageSize: 4096, locking: .none,
                synchronous: false, flushIntervalMilliseconds: interval),
            catalogID: Database.allocateCatalogID(), random: SeededRandomSource(seed: 1), operations: filesystem)
        return (database, filesystem)
    }
    private func working(_ lease: WriterLease, _ name: String) throws -> DomainSnapshot {
        var snapshot = lease.snapshot, catalog = snapshot.catalog
        catalog.sequences.append(CatalogSequence(name: try SQLText(name)))
        try snapshot.updateCatalog(catalog, schemaChanged: true); return snapshot
    }
    private func commit(_ database: Database, _ name: String, synchronous: Bool) throws {
        let lease = try database.core.acquireWriter(); defer { lease.close() }
        let snapshot = try working(lease, name)
        let plan = try XCTUnwrap(lease.prepare(snapshot, synchronous: synchronous))
        try lease.persist(plan, synchronous: synchronous)
    }
    func testOneReadingPerChangedFileCommitAndExpiryBeforePlanning() throws {
        let clock = Clock(), (database, filesystem) = try file(clock)
        defer { try? database.close() }
        try commit(database, "first", synchronous: false)
        XCTAssertEqual(clock.reads, 1); XCTAssertEqual(database.core.storage.pager.idleFlushState.deadline, 20_000)
        clock.set(19_999); try commit(database, "second", synchronous: false)
        XCTAssertEqual(clock.reads, 2); XCTAssertEqual(database.core.storage.pager.idleFlushState.deadline, 20_000)
        filesystem.beginStep(2); clock.set(20_000)
        let lease = try database.core.acquireWriter(); defer { lease.close() }
        let plan = try XCTUnwrap(lease.prepare(working(lease, "third"), synchronous: false))
        XCTAssertEqual(clock.reads, 3); XCTAssertEqual(filesystem.barriers, 1)
        XCTAssertTrue(database.core.storage.pager.pendingEntries.isEmpty)
        try lease.persist(plan, synchronous: false)
        XCTAssertEqual(clock.reads, 3); XCTAssertEqual(database.core.storage.pager.idleFlushState.deadline, 30_000)
        try database.core.storage.pager.flush()
        XCTAssertNil(database.core.storage.pager.idleFlushState.deadline); XCTAssertEqual(clock.reads, 3)
    }
    func testSynchronousAndMemoryCommitsReadOnlyWhenTimingAnExistingFileBatch() throws {
        let clock = Clock(), (database, _) = try file(clock)
        defer { try? database.close() }
        try commit(database, "sync", synchronous: true); XCTAssertEqual(clock.reads, 0)
        try commit(database, "relaxed", synchronous: false); XCTAssertEqual(clock.reads, 1)
        try commit(database, "sync_after", synchronous: true); XCTAssertEqual(clock.reads, 2)
        XCTAssertNil(database.core.storage.pager.idleFlushState.deadline)
        var builder = Environment.builder(); builder.setClockSource(clock)
        let memory = try Database(environment: builder.build()); defer { try? memory.close() }
        try commit(memory, "memory", synchronous: false); XCTAssertEqual(clock.reads, 2)
        let lease = try database.core.acquireWriter(); defer { lease.close() }
        XCTAssertNil(try lease.prepare(lease.snapshot, synchronous: false)); XCTAssertEqual(clock.reads, 2)
    }
    func testClientBeginEndsExpiredBatchAndWriterLeaseExcludesIdleFlush() throws {
        let clock = Clock(), (database, filesystem) = try file(clock)
        defer { try? database.close() }
        try commit(database, "first", synchronous: false)
        try database.core.startService(incarnation: Array(repeating: 11, count: 16))
        clock.set(20_000); filesystem.beginStep(2)
        let lease = try database.core.serviceBeginWrite(.cold, relaxed: true, timeoutMilliseconds: 0, cancellation: CancellationToken())
        XCTAssertEqual(clock.reads, 2); XCTAssertEqual(filesystem.barriers, 1)
        XCTAssertTrue(lease.reply.pending.isEmpty)
        XCTAssertThrowsError(try database.core.idleBatchFlush(expected: database.core.storage)) {
            XCTAssertEqual(($0 as? JedError)?.code, "55006")
        }
        XCTAssertEqual(clock.reads, 2); lease.close()
        let noBatch = try database.core.serviceBeginWrite(.cold, relaxed: true, timeoutMilliseconds: 0, cancellation: CancellationToken())
        noBatch.close(); XCTAssertEqual(clock.reads, 2)
    }
    func testFailedDeadlineBarrierPrecedesNewWritesAndPoisonsStorage() throws {
        let clock = Clock(), (database, filesystem) = try file(clock)
        defer { try? database.close() }
        try commit(database, "first", synchronous: false)
        clock.set(20_000); filesystem.beginStep(2, fault: SimFault(kind: .sync, nth: 1))
        let lease = try database.core.acquireWriter(); defer { lease.close() }
        XCTAssertThrowsError(try lease.prepare(working(lease, "second"), synchronous: false)) {
            XCTAssertEqual(($0 as? JedError)?.code, "58030")
        }
        XCTAssertEqual(clock.reads, 2); XCTAssertEqual(filesystem.writes, 0)
        XCTAssertTrue(database.core.storage.pager.isPoisoned); XCTAssertEqual(database.version, 2)
    }
    func testAllDomainBudgetPreflightPrecedesExpiredBatchFlush() throws {
        let clock = Clock(), (database, filesystem) = try file(clock)
        defer { try? database.close() }
        try commit(database, "first", synchronous: false)
        let memory = try Database(pageSize: 4096); defer { try? memory.close() }
        let fileLease = try database.core.acquireWriter(), memoryLease = try memory.core.acquireWriter()
        defer { fileLease.close(); memoryLease.close() }
        clock.set(20_000); filesystem.beginStep(2)
        var file = DomainCommitParticipant(name: try SQLText("main"), lease: fileLease, working: try working(fileLease, "second"))
        file.synchronous = false
        var ephemeral = DomainCommitParticipant(name: try SQLText("a"), lease: memoryLease, working: try working(memoryLease, "too_large"))
        ephemeral.policy.storageLimit = 1
        XCTAssertThrowsError(try DomainCommitGroup.prepare([file, ephemeral])) { XCTAssertEqual(($0 as? JedError)?.code, "54P06") }
        XCTAssertEqual(clock.reads, 1); XCTAssertEqual(filesystem.barriers, 0); XCTAssertEqual(filesystem.writes, 0)
    }
    func testThrowingClientClockAfterBodyWritesPoisonsBeforeMetaPublication() throws {
        final class FailingClock: ClockSource {
            func microseconds() throws -> Int64 { throw JedError("58030", "host clock failed") }
        }
        let storage = try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: 991, pageSize: 4096)
        let core = DatabaseCore(storage: storage)
        try core.startService(incarnation: Array(repeating: 19, count: 16))
        var snapshot = storage.committed, catalog = snapshot.catalog
        catalog.sequences.append(CatalogSequence(name: try SQLText("external")))
        try snapshot.updateCatalog(catalog, schemaChanged: true)
        let plan = try XCTUnwrap(storage.prepare(snapshot))
        storage.pager.configureBatchFlush(clock: FailingClock(), milliseconds: 10)
        let lease = try core.serviceBeginWrite(.cold, relaxed: true, timeoutMilliseconds: 0, cancellation: CancellationToken())
        defer { lease.close() }
        let begin = ServiceCommitBegin(txid: plan.snapshot.txid, meta: plan.pages.metaBytes,
            manifest: plan.pages.manifest.pages.map { StoredPage(index: $0.pageIndex, bytes: $0.bytes) })
        var delta = try lease.begin(begin)
        for page in plan.pages.body { try delta.receive(page); try lease.write(page) }
        XCTAssertFalse(plan.pages.body.isEmpty)
        XCTAssertThrowsError(try lease.finish(delta, entropy: nil)) { XCTAssertEqual(($0 as? JedError)?.code, "58030") }
        XCTAssertTrue(storage.pager.isPoisoned); XCTAssertEqual(core.version, 1)
    }
    func testNativeIdleWaiterStopsAtCloseAndReadOnlyOpenNeverReadsClock() throws {
        let clock = Clock(); var builder = Environment.builder(); builder.setClockSource(clock)
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-batch-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: directory) }
        let path = directory.appendingPathComponent("test.jed").path
        let database = try Database.create(options: CreateOptions(environment: builder.build(), path: path,
            pageSize: 4096, locking: .none, synchronous: false, flushIntervalMilliseconds: 10))
        defer { try? database.close() }
        try commit(database, "first", synchronous: false); XCTAssertEqual(clock.reads, 1)
        clock.set(20_000)
        let limit = Date(timeIntervalSinceNow: 2)
        while !database.core.storage.pager.pendingEntries.isEmpty && Date() < limit { Thread.sleep(forTimeInterval: 0.005) }
        XCTAssertTrue(database.core.storage.pager.pendingEntries.isEmpty)
        try database.close(); let reads = clock.reads
        let reader = try Database.open(path, options: OpenOptions(environment: builder.build(), readOnly: true,
            locking: .none, synchronous: false, flushIntervalMilliseconds: 1))
        let pin = try reader.core.beginRead(); pin.close(); try reader.close()
        Thread.sleep(forTimeInterval: 0.04); XCTAssertEqual(clock.reads, reads)
    }
    func testFrozenDeadlineRegressionUsesRealForegroundPolicy() throws {
        let path = try XCTUnwrap(Bundle.module.url(forResource: "relaxed-deadline-flush", withExtension: "sim", subdirectory: "Fixtures/Simulation/regress"))
        let result = try SimRunner(SimTrace(String(contentsOf: path, encoding: .utf8))).run()
        XCTAssertEqual(result.violation, "")
    }
}
