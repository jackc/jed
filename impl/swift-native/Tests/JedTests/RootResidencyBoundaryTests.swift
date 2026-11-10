import Synchronization
import XCTest
@testable import Jed

private final class RootRefaultDevice: BlockStore {
    let memory = MemoryBlockStore()
    private let blocked = Mutex<UInt64?>(nil)
    func failRead(at offset: UInt64?) { blocked.withLock { $0 = offset } }
    func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        if blocked.withLock({ $0 == offset }) { throw JedError("58030", "injected root refault failure") }
        return try memory.read(at: offset, count: count)
    }
    func write(at offset: UInt64, bytes: [UInt8]) throws { try memory.write(at: offset, bytes: bytes) }
    func size() throws -> UInt64 { try memory.size() }
    func setSize(_ bytes: UInt64) throws { try memory.setSize(bytes) }
    func sync() throws {}
}

final class RootResidencyBoundaryTests: XCTestCase {
    private let types = [TypeDescriptor(.scalar(.i64)), TypeDescriptor(.scalar(.bytea))]
    private var tableName: SQLText { SQLText(validatedUTF8: Array("t".utf8)) }
    private func key(_ id: Int64) throws -> [UInt8] { try KeyEncoding.encode(.integer(id), type: types[0]) }
    private func payload(_ seed: UInt32) -> [UInt8] {
        var state = seed
        return (0..<601).map { _ in
            state ^= state << 13; state ^= state >> 17; state ^= state << 5
            return UInt8(truncatingIfNeeded: state)
        }
    }
    private func add(_ id: Int64, to snapshot: inout DomainSnapshot) throws {
        var store = try XCTUnwrap(snapshot.tables[tableName])
        let record = try RecordDisposition.plan(key: key(id), values: [.value(.integer(id)), .value(.bytea(payload(UInt32(id))))],
            types: types, codec: snapshot.pool.codec)
        try store.put(record, netStaging: true); snapshot.setTableStore(tableName, store)
    }
    private func seeded(_ device: RootRefaultDevice, kind: StorageDomainKind = .file) throws -> StorageDomain {
        let domain = try StorageDomain.create(store: device, kind: kind, catalogID: 117, pageSize: 256)
        var working = domain.committed, catalog = working.catalog
        let columns = [CatalogColumn(name: SQLText(validatedUTF8: Array("id".utf8)), type: CatalogTypeUse(.scalar(.i64)), notNull: true),
            CatalogColumn(name: SQLText(validatedUTF8: Array("b".utf8)), type: CatalogTypeUse(.scalar(.bytea)))]
        catalog.tables.append(CatalogTable(name: tableName, columns: columns, primaryKey: [0], root: PageSealer.nullReference(domain.pager.codec)))
        try working.updateCatalog(catalog, schemaChanged: true)
        working.setTableStore(tableName, working.emptyStore(types: types)); try add(1, to: &working)
        _ = try domain.persist(XCTUnwrap(domain.prepare(working, verifyLiveCount: true)))
        return domain
    }
    private func mixed(_ device: RootRefaultDevice, kind: StorageDomainKind = .file) throws -> (StorageDomain, PreparedDomainCommit) {
        // A memory domain opens a durable image, as deserialize does. Its own
        // internal ephemeral commits do not persist live-page accounting.
        _ = try seeded(device)
        let reopened = try StorageDomain.open(store: device, kind: kind, catalogID: 118, validateCatalog: { _ in })
        var working = reopened.committed; try add(2, to: &working)
        return (reopened, try XCTUnwrap(reopened.prepare(working, verifyLiveCount: true)))
    }

    func testMixedRootDropsEveryPayloadCacheAfterPersistenceButFreshRootRetainsIt() throws {
        let freshDevice = RootRefaultDevice(), fresh = try seeded(freshDevice)
        XCTAssertFalse(try XCTUnwrap(fresh.committed.tables[tableName]?.get(key(1))).overflows.isEmpty)
        let device = RootRefaultDevice(), (domain, plan) = try mixed(device)
        XCTAssertFalse(try XCTUnwrap(plan.snapshot.tables[tableName]?.get(key(2))).overflows.isEmpty)
        let committed = try domain.persist(plan)
        XCTAssertEqual(committed.tables[tableName]?.tree.height, 1)
        for id in [Int64(1), Int64(2)] {
            let record = try XCTUnwrap(committed.tables[tableName]?.get(key(id)))
            XCTAssertTrue(record.overflows.isEmpty)
            if case .bytea(let value) = try record.materialize(column: 1, type: types[1], pager: domain.pager) { XCTAssertEqual(value, payload(UInt32(id))) }
            else { XCTFail("wrong stored value") }
        }
        var working = committed, store = try XCTUnwrap(working.tables[tableName])
        let old = try XCTUnwrap(store.get(key(2)))
        let replacement = try RecordDisposition.plan(key: old.key, values: old.cells.map { .kept($0) }, types: types, codec: domain.pager.codec)
        try store.put(replacement, netStaging: true); working.setTableStore(tableName, store)
        let next = try XCTUnwrap(domain.prepare(working, verifyLiveCount: true))
        XCTAssertEqual(next.pages.body.filter { $0.bytes.first == 4 }.count, 0)
    }

    func testPostDurableRefaultFailurePoisonsWriterAndPreservesKnownCommit() throws {
        let device = RootRefaultDevice(), (domain, plan) = try mixed(device)
        let root = try XCTUnwrap(plan.snapshot.tables[tableName]?.tree.root?.reference)
        device.failRead(at: UInt64(root.index) * UInt64(domain.pager.pageSize))
        let committed = try domain.persist(plan)
        XCTAssertEqual(committed.txid, plan.snapshot.txid)
        XCTAssertEqual(domain.pager.selected.meta.txid, committed.txid)
        XCTAssertTrue(domain.pager.isPoisoned)
        XCTAssertThrowsError(try domain.pager.checkWritable()) { XCTAssertEqual(($0 as? JedError)?.code, "58030") }
        device.failRead(at: nil)
        let reopened = try StorageDomain.open(store: device, catalogID: 119, validateCatalog: { _ in })
        XCTAssertEqual(reopened.committed.tables[tableName]?.count, 2)
    }

    func testStagedMemoryRefaultFinishesBeforeFinalAdmission() throws {
        let device = RootRefaultDevice(), (domain, plan) = try mixed(device, kind: .memory)
        let staged = try domain.stageMemory(plan)
        XCTAssertTrue(try XCTUnwrap(staged.snapshot.tables[tableName]?.get(key(2))).overflows.isEmpty)
        let admitted = try domain.admitMemory(staged)
        let committed = domain.finishMemory(admitted)
        XCTAssertTrue(try XCTUnwrap(committed.tables[tableName]?.get(key(2))).overflows.isEmpty)
    }
}
