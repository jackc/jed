import Foundation
import Synchronization
import XCTest
@testable import Jed

private final class GroupEvents: Sendable {
    let values = Mutex<[String]>([])
    func append(_ value: String) { values.withLock { $0.append(value) } }
}

private final class GroupDevice: BlockStore {
    let memory = MemoryBlockStore(), events: GroupEvents, name: String
    let failWrites = Mutex(false)
    init(_ name: String, _ events: GroupEvents) { self.name = name; self.events = events }
    func read(at offset: UInt64, count: Int) throws -> [UInt8] { try memory.read(at: offset, count: count) }
    func write(at offset: UInt64, bytes: [UInt8]) throws {
        events.append(name + ":write")
        if failWrites.withLock({ $0 }) { throw JedError("58030", "injected group memory write failure") }
        try memory.write(at: offset, bytes: bytes)
    }
    func sync() throws { events.append(name + ":sync") }
    func size() throws -> UInt64 { try memory.size() }
    func setSize(_ bytes: UInt64) throws { events.append(name + ":resize"); try memory.setSize(bytes) }
}

final class DomainCommitGroupBoundaryTests: XCTestCase {
    private let table = SQLText(validatedUTF8: [116])
    private func name(_ name: String) throws -> SQLText { try SQLText(name) }
    private func key(_ value: Int64) throws -> [UInt8] { try KeyEncoding.encode(.integer(value), type: TypeDescriptor(.scalar(.i64))) }
    private func edit(_ old: DomainSnapshot, value: Int64) throws -> DomainSnapshot {
        var working = old
        if working.tables[table] == nil {
            var image = working.catalog
            image.tables.append(CatalogTable(name: table,
                columns: [CatalogColumn(name: try name("v"), type: CatalogTypeUse(.scalar(.i64)), notNull: true)],
                primaryKey: [0], root: PageSealer.nullReference(working.pool.codec)))
            try working.updateCatalog(image, schemaChanged: true)
            working.setTableStore(table, working.emptyStore(types: [TypeDescriptor(.scalar(.i64))]))
        }
        var store = try XCTUnwrap(working.tables[table])
        let record = try RecordDisposition.plan(key: key(value), values: [.value(.integer(value))], types: store.tree.types, codec: store.tree.codec)
        try store.put(record, netStaging: true); working.setTableStore(table, store)
        return working
    }
    private func core(_ device: GroupDevice, kind: StorageDomainKind = .memory, id: UInt64) throws -> DatabaseCore {
        DatabaseCore(storage: try StorageDomain.create(store: device, kind: kind, catalogID: id, pageSize: 256))
    }
    private func item(_ name: String, _ lease: WriterLease, value: Int64) throws -> DomainCommitParticipant {
        DomainCommitParticipant(name: try self.name(name), lease: lease, working: try edit(lease.snapshot, value: value))
    }
    private func expect(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }

    func testEveryBudgetAdmittedBeforeStagingAndForcedCompactionSurvivesRefusal() throws {
        let events = GroupEvents(), aDevice = GroupDevice("a", events), bDevice = GroupDevice("b", events)
        let a = try core(aDevice, id: 1), b = try core(bDevice, id: 2)
        let seed = try a.acquireWriter()
        _ = try seed.persist(XCTUnwrap(seed.prepare(edit(seed.snapshot, value: 1)))); seed.close()
        let al = try a.acquireWriter(), bl = try b.acquireWriter(); defer { al.close(); bl.close() }
        var ai = try item("a", al, value: 2), bi = try item("b", bl, value: 3)
        ai.policy.storageLimit = 6 * 256; bi.policy.storageLimit = 1
        let free = try a.storage.pager.allocationSnapshot(reuseAllowed: true).persistedFree
        events.values.withLock { $0.removeAll() }
        expect("54P06") { _ = try DomainCommitGroup.prepare([bi, ai]) }
        XCTAssertTrue(events.values.withLock { $0.isEmpty })
        let reclaimed = try a.storage.pager.allocationSnapshot(reuseAllowed: true).persistedFree
        XCTAssertTrue(free.isEmpty); XCTAssertEqual(reclaimed, [2])
        XCTAssertEqual(a.storage.pager.selected.meta.pageCount, 5)
        XCTAssertNotNil(try a.storage.committed.tables[table]?.get(key(1)))
        XCTAssertNil(try a.storage.committed.tables[table]?.get(key(2)))
        XCTAssertEqual(a.version, 1); XCTAssertEqual(b.version, 0)
    }

    func testSecondChangedFileRejectedButUnchangedFileNotWritten() throws {
        let events = GroupEvents(), aDevice = GroupDevice("a", events), bDevice = GroupDevice("b", events), mDevice = GroupDevice("m", events)
        let a = try core(aDevice, kind: .file, id: 3), b = try core(bDevice, kind: .file, id: 4), m = try core(mDevice, id: 5)
        let al = try a.acquireWriter(), bl = try b.acquireWriter(), ml = try m.acquireWriter()
        defer { al.close(); bl.close(); ml.close() }
        events.values.withLock { $0.removeAll() }
        expect("0A000") { _ = try DomainCommitGroup.prepare([item("main", al, value: 1), item("other", bl, value: 2)]) }
        XCTAssertTrue(events.values.withLock { $0.isEmpty })
        let unchanged = DomainCommitParticipant(name: try name("main"), lease: al, working: al.snapshot)
        let plan = try DomainCommitGroup.prepare([unchanged, item("memory", ml, value: 3)])
        XCTAssertEqual(plan.changedNames, [try name("memory")])
        let roots = try plan.commit()
        XCTAssertEqual(roots.count, 1); XCTAssertEqual(m.version, 1); XCTAssertEqual(a.version, 1)
        XCTAssertTrue(events.values.withLock { !$0.contains(where: { $0.hasPrefix("a:") || $0.hasPrefix("b:") }) })
        expect("55000") { _ = try plan.commit() }
    }

    func testLaterMemoryStageFailureLeavesEarlierPublishedRootAndAccountingUnchanged() throws {
        let events = GroupEvents(), aDevice = GroupDevice("a", events), bDevice = GroupDevice("b", events)
        let a = try core(aDevice, id: 6), b = try core(bDevice, id: 7)
        let al = try a.acquireWriter(), bl = try b.acquireWriter(); defer { al.close(); bl.close() }
        let ai = try item("a", al, value: 10), bi = try item("b", bl, value: 20)
        let plan = try DomainCommitGroup.prepare([bi, ai])
        events.values.withLock { $0.removeAll() }; bDevice.failWrites.withLock { $0 = true }
        expect("58030") { _ = try plan.commit() }
        XCTAssertEqual(a.version, 0); XCTAssertEqual(b.version, 0)
        XCTAssertEqual(a.storage.pager.selected.meta.pageCount, 3)
        XCTAssertTrue(try a.storage.pager.allocationSnapshot(reuseAllowed: true).persistedFree.isEmpty)
        XCTAssertFalse(a.storage.pager.isPoisoned); XCTAssertTrue(b.storage.pager.isPoisoned)
        let log = events.values.withLock { $0 }
        XCTAssertLessThan(try XCTUnwrap(log.firstIndex(of: "a:write")), try XCTUnwrap(log.firstIndex(of: "b:write")))
        let retry = try DomainCommitGroup.prepare([ai]); _ = try retry.commit()
        XCTAssertEqual(a.version, 1)
        XCTAssertNotNil(try a.storage.committed.tables[table]?.get(key(10)))
    }

    func testFileFailureAfterMemoryStageDoesNotPublishMemory() throws {
        let events = GroupEvents(), mDevice = GroupDevice("memory", events), fDevice = GroupDevice("file", events)
        let memory = try core(mDevice, id: 8), file = try core(fDevice, kind: .file, id: 9)
        let ml = try memory.acquireWriter(), fl = try file.acquireWriter(); defer { ml.close(); fl.close() }
        let mi = try item("temp", ml, value: 1)
        let plan = try DomainCommitGroup.prepare([item("main", fl, value: 2), mi])
        file.storage.pager.armFault(.body(ordinal: 1, tearBytes: 0))
        events.values.withLock { $0.removeAll() }
        expect("58030") { _ = try plan.commit() }
        XCTAssertEqual(memory.version, 0); XCTAssertEqual(file.version, 1)
        XCTAssertFalse(memory.storage.pager.isPoisoned); XCTAssertTrue(file.storage.pager.isPoisoned)
        XCTAssertTrue(events.values.withLock { $0.contains("memory:write") })
        _ = try DomainCommitGroup.prepare([mi]).commit()
        XCTAssertEqual(memory.version, 1)
    }

    func testMemoryStagesPrecedeFileAndRetainedPinsKeepOldRoots() throws {
        let events = GroupEvents(), mDevice = GroupDevice("memory", events), fDevice = GroupDevice("file", events)
        let memory = try core(mDevice, id: 10), file = try core(fDevice, kind: .file, id: 11)
        let oldMemory = memory.pinLatest(), oldFile = file.pinLatest(); defer { oldMemory.close(); oldFile.close() }
        let ml = try memory.acquireWriter(), fl = try file.acquireWriter(); defer { ml.close(); fl.close() }
        let plan = try DomainCommitGroup.prepare([item("main", fl, value: 2), item("temp", ml, value: 1)])
        events.values.withLock { $0.removeAll() }
        let roots = try plan.commit(), log = events.values.withLock { $0 }
        XCTAssertEqual(roots.count, 2); XCTAssertEqual(memory.version, 1); XCTAssertEqual(file.version, 2)
        XCTAssertNil(oldMemory.snapshot.tables[table]); XCTAssertNil(oldFile.snapshot.tables[table])
        XCTAssertNotNil(try roots[name("temp")]?.tables[table]?.get(key(1)))
        XCTAssertLessThan(try XCTUnwrap(log.lastIndex(of: "memory:write")), try XCTUnwrap(log.firstIndex(of: "file:write")))
        XCTAssertFalse(log.contains("memory:sync"))
        XCTAssertTrue(log.contains("file:sync"))
    }

    func testTemporaryHostSpillFailurePrecedesFileWrites() throws {
        let events = GroupEvents(), mDevice = GroupDevice("memory", events), fDevice = GroupDevice("file", events)
        let memory = try core(mDevice, id: 12), file = try core(fDevice, kind: .file, id: 13)
        let ml = try memory.acquireWriter(), fl = try file.acquireWriter(); defer { ml.close(); fl.close() }
        memory.storage.installMemoryStageHook { pageCount in
            XCTAssertGreaterThan(pageCount, 3)
            events.append("spill")
            throw JedError("58030", "injected spill failure before staging")
        }
        let plan = try DomainCommitGroup.prepare([item("main", fl, value: 2), item("temp", ml, value: 1)])
        events.values.withLock { $0.removeAll() }
        expect("58030") { _ = try plan.commit() }
        XCTAssertEqual(events.values.withLock { $0 }, ["spill"])
        XCTAssertEqual(memory.version, 0); XCTAssertEqual(file.version, 1)
        XCTAssertFalse(memory.storage.pager.isPoisoned); XCTAssertFalse(file.storage.pager.isPoisoned)
    }
}
