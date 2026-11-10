import Foundation
import Synchronization
import XCTest
@testable import Jed

final class DatabaseCoreBoundaryTests: XCTestCase {
    private var tableName: SQLText { SQLText(validatedUTF8: [116]) }
    private var rowKey: [UInt8] { [0x80, 0, 0, 0, 0, 0, 0, 0] }
    private func expect(_ code: String, _ operation: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try operation(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }
    private func edit(_ snapshot: DomainSnapshot, value: Int64) throws -> DomainSnapshot {
        var working = snapshot
        if working.tables[tableName] == nil {
            var catalog = working.catalog
            catalog.tables.append(CatalogTable(name: tableName,
                columns: [CatalogColumn(name: try SQLText("v"), type: CatalogTypeUse(.scalar(.i64)), notNull: true)],
                primaryKey: [0], root: PageSealer.nullReference(working.pool.codec)))
            try working.updateCatalog(catalog, schemaChanged: true)
            working.setTableStore(tableName, working.emptyStore(types: [TypeDescriptor(.scalar(.i64))]))
        }
        var store = working.tables[tableName]!
        let record = try RecordDisposition.plan(key: rowKey, values: [.value(.integer(value))], types: store.tree.types, codec: store.tree.codec)
        try store.put(record, netStaging: true); working.setTableStore(tableName, store)
        return working
    }
    private func stored(_ snapshot: DomainSnapshot, pager: Pager) throws -> Int64 {
        let record = try XCTUnwrap(snapshot.tables[tableName]?.get(rowKey))
        guard case .integer(let value) = try record.materialize(column: 0, type: TypeDescriptor(.scalar(.i64)), pager: pager) else {
            throw JedError("XX001", "expected integer in test record")
        }
        return value
    }
    @discardableResult private func write(_ core: DatabaseCore, value: Int64,
                                          policy: DomainCommitPolicy = .init()) throws -> DomainSnapshot {
        let lease = try core.acquireWriter(); defer { lease.close() }
        let working = try edit(lease.snapshot, value: value)
        guard let prepared = try lease.prepare(working, policy: policy) else { throw JedError("XX000", "test write did not change the domain") }
        return try lease.persist(prepared)
    }

    func testIndependentPinRetainCloseAndLeaseExclusion() throws {
        let core = DatabaseCore(storage: try StorageDomain.create(store: MemoryBlockStore(), catalogID: 20))
        let first = core.pinLatest(), second = try first.retain()
        XCTAssertEqual(core.livePinCount, 2)
        first.close(); first.close(); XCTAssertEqual(core.livePinCount, 1)
        expect("55000") { _ = try first.retain() }
        let lease = try core.acquireWriter()
        expect("55006") { _ = try core.acquireWriter(wait: false) }
        expect("55006") { try core.maintenance {} }
        let working = try edit(lease.snapshot, value: 5), cursorPin = try lease.pin(working)
        XCTAssertEqual(core.livePinCount, 2)
        let committed = try lease.persist(XCTUnwrap(lease.prepare(working)))
        XCTAssertEqual(core.oldestLiveVersion, 0)
        XCTAssertEqual(try stored(cursorPin.snapshot, pager: core.storage.pager), 5)
        second.close(); cursorPin.close()
        XCTAssertEqual(core.oldestLiveVersion, committed.txid); XCTAssertEqual(core.livePinCount, 0)
        lease.close(); lease.close(); expect("55000") { _ = try lease.prepare(committed) }
        XCTAssertEqual(try core.maintenance { 7 }, 7)
    }

    func testWriterWaitCancellationDoesNotReleaseAnotherLease() throws {
        let gate = WriterGate(), owner = try gate.acquire(), cancellation = CancellationToken()
        let started = DispatchSemaphore(value: 0), done = DispatchSemaphore(value: 0)
        let result = Mutex<String?>(nil)
        DispatchQueue.global().async {
            started.signal()
            do { let token = try gate.acquire(cancellation: cancellation); gate.release(token); result.withLock { $0 = "acquired" } }
            catch { result.withLock { $0 = (error as? JedError)?.code ?? "unknown" } }
            done.signal()
        }
        XCTAssertEqual(started.wait(timeout: .now() + 2), .success)
        cancellation.cancel()
        XCTAssertEqual(done.wait(timeout: .now() + 2), .success)
        XCTAssertEqual(result.withLock { $0 }, "57014"); XCTAssertTrue(gate.owns(owner))
        gate.release(StorageRevision()); XCTAssertTrue(gate.owns(owner))
        gate.release(owner)
        let replacement = try gate.acquire(wait: false); XCTAssertTrue(gate.owns(replacement)); gate.release(replacement)
    }

    func testFallbackReaderPinsOldRootBeforePublicationAndDefersActualReuse() throws {
        let domain = try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: 21, pageSize: 256, cacheBytes: 256)
        let core = DatabaseCore(storage: domain)
        _ = try write(core, value: 0)
        var fallback: SnapshotPin?, expected: Int64 = 0
        // Find a real reclamation commit: its free list contains the leaf that
        // the previously published root still references. No production knob
        // or simulated ownership state is used to manufacture the window.
        for value in 1...80 {
            let lease = try core.acquireWriter(), before = lease.snapshot
            let oldLeaf = try XCTUnwrap(before.tables[tableName]?.tree.root?.reference?.index)
            let prepared = try XCTUnwrap(lease.prepare(edit(before, value: Int64(value))))
            if prepared.pages.freePages.contains(oldLeaf) {
                expected = try stored(before, pager: domain.pager)
                _ = try lease.persist(prepared, afterPersist: {
                    XCTAssertEqual(domain.committed.txid, before.txid + 1)
                    XCTAssertEqual(core.version, before.txid)
                    fallback = core.pinLatest()
                })
                lease.close(); break
            }
            _ = try lease.persist(prepared); lease.close()
        }
        let pin = try XCTUnwrap(fallback), retained = try pin.retain()
        XCTAssertEqual(core.livePinCount, 2); pin.close()
        for value in 100..<180 {
            _ = try write(core, value: Int64(value))
            XCTAssertEqual(try stored(retained.snapshot, pager: domain.pager), expected)
        }
        XCTAssertEqual(core.oldestLiveVersion, retained.snapshot.txid)
        retained.close(); XCTAssertEqual(core.livePinCount, 0)
        _ = try write(core, value: 200)
        XCTAssertEqual(try stored(domain.committed, pager: domain.pager), 200)
    }

    func testMemoryCompactionAllowsCurrentPinsAndRejectsOlderPins() throws {
        let domain = try StorageDomain.create(store: MemoryBlockStore(), catalogID: 22, pageSize: 256)
        let core = DatabaseCore(storage: domain)
        _ = try write(core, value: 1)
        XCTAssertEqual(domain.pager.selected.meta.pageCount, 5)
        var policy = DomainCommitPolicy(); policy.storageLimit = 6 * 256
        let pin = core.pinLatest()
        _ = try write(core, value: 2, policy: policy)
        XCTAssertEqual(domain.pager.selected.meta.pageCount, 6)
        expect("54P06") { _ = try write(core, value: 3, policy: policy) }
        XCTAssertEqual(try stored(pin.snapshot, pager: domain.pager), 1)
        pin.close()
        let current = core.pinLatest()
        _ = try write(core, value: 3, policy: policy)
        XCTAssertEqual(try stored(current.snapshot, pager: domain.pager), 2)
        XCTAssertLessThanOrEqual(domain.pager.selected.meta.pageCount, 6)
        current.close()
    }

    func testSuccessfulPersistencePublishesEvenWhenObserverThrows() throws {
        let core = DatabaseCore(storage: try StorageDomain.create(store: MemoryBlockStore(), catalogID: 23))
        let lease = try core.acquireWriter(); defer { lease.close() }
        let prepared = try XCTUnwrap(lease.prepare(edit(lease.snapshot, value: 7)))
        expect("XX000") { _ = try lease.persist(prepared, afterPersist: { throw JedError("XX000", "observer error") }) }
        XCTAssertEqual(core.version, 1); XCTAssertEqual(core.storage.committed.txid, 1)
        let pin = core.pinLatest(); defer { pin.close() }
        XCTAssertEqual(try stored(pin.snapshot, pager: core.storage.pager), 7)
    }

    func testReadOnlyOpenAndNoopDoNotAcquireWriterCapability() throws {
        let device = MemoryBlockStore()
        _ = try StorageDomain.create(store: device, kind: .file, catalogID: 24)
        var validated = false
        let domain = try StorageDomain.open(store: device, catalogID: 24, readOnly: true, validateCatalog: { _ in validated = true })
        XCTAssertTrue(validated); XCTAssertNil(try domain.prepare(domain.committed))
        let core = DatabaseCore(storage: domain)
        expect("25006") { _ = try core.acquireWriter() }
        expect("25006") { _ = try domain.prepare(edit(domain.committed, value: 1)) }
        XCTAssertEqual(core.version, 1)
    }
}
