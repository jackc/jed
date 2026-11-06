import Synchronization
import XCTest
@testable import Jed

private final class CursorReadDevice: BlockStore {
    private struct State { var reject = false; var reads = 0 }
    private let state = Mutex(State())
    private let bytes = MemoryBlockStore()
    var reads: Int { state.withLock { $0.reads } }
    func reset(reject: Bool = false) { state.withLock { $0 = State(reject: reject) } }
    func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        try state.withLock { state in
            state.reads += 1
            if state.reject { throw JedError("58030", "injected cursor leaf read") }
        }
        return try bytes.read(at: offset, count: count)
    }
    func write(at offset: UInt64, bytes: [UInt8]) throws { try self.bytes.write(at: offset, bytes: bytes) }
    func size() throws -> UInt64 { try bytes.size() }
    func setSize(_ bytes: UInt64) throws { try self.bytes.setSize(bytes) }
    func sync() throws { try bytes.sync() }
}

final class CursorAdmissionBoundaryTests: XCTestCase {
    private func fixture() throws -> (Database, CursorReadDevice) {
        let device = CursorReadDevice(), environment = Environment.builder().build()
        let storage = try StorageDomain.create(store: device, kind: .file, catalogID: Database.allocateCatalogID(), pageSize: 512)
        let seed = Database(core: DatabaseCore(storage: storage), environment: environment)
        try seed.execute("CREATE TABLE t(id i32 PRIMARY KEY, value text)")
        try seed.execute("INSERT INTO t SELECT i,'value' FROM generate_series(1,80) g(i)")
        try seed.close()
        let reopened = try StorageDomain.open(store: device, kind: .file, catalogID: Database.allocateCatalogID(), cacheBytes: 1,
            validateCatalog: { try CatalogEnvironment.validate($0, environment: environment) })
        let database = Database(core: DatabaseCore(storage: reopened), environment: environment)
        reopened.pool.invalidateAll(); device.reset()
        return (database, device)
    }
    private func fails<T>(_ code: String, _ operation: () throws -> T) {
        XCTAssertThrowsError(try operation()) { XCTAssertEqual(($0 as? JedError)?.code, code) }
    }
    private func tree(_ database: Database) throws -> BPlusTree {
        try XCTUnwrap(database.core.routingSnapshot.tables[SQLText("t")]).tree
    }

    func testFixedPointChargesAtOpenAndDefersLeafFaultAndRowCost() throws {
        let (database, device) = try fixture(); defer { try? database.close() }
        let height = Int64(try tree(database).height)
        XCTAssertGreaterThan(height, 0)
        device.reset(reject: true)
        let rows = try database.query("SELECT id FROM t WHERE id=40 ORDER BY id")
        XCTAssertEqual(rows.cost, height); XCTAssertEqual(device.reads, 0)
        fails("58030") { try rows.next() }
        XCTAssertTrue(rows.isClosed); XCTAssertEqual(rows.error?.code, "58030")
        XCTAssertEqual(database.core.livePinCount, 0)
        device.reset()
        let session = try database.session(); defer { session.close() }
        session.maxCost = height
        let atCeiling = try session.query("SELECT id FROM t WHERE id=40 ORDER BY id")
        XCTAssertEqual(atCeiling.cost, height); XCTAssertEqual(device.reads, 0)
        fails("54P01") { try atCeiling.next() }
        XCTAssertEqual(atCeiling.cost, height)
        session.maxCost = height + 1
        let limited = try session.query("SELECT id FROM t WHERE id=40 ORDER BY id")
        XCTAssertEqual(limited.cost, height)
        fails("54P01") { try limited.next() }
        XCTAssertEqual(limited.cost, height + 1)
        XCTAssertEqual(database.core.livePinCount, 0)
        session.maxCost = 1
        let missed = try session.query("SELECT id FROM t WHERE id=400 ORDER BY id")
        XCTAssertEqual(missed.cost, height); XCTAssertNil(try missed.next())
        let zero = try session.query("SELECT id FROM t ORDER BY id LIMIT 0")
        XCTAssertEqual(zero.cost, Int64(try tree(database).nodeCount)); XCTAssertNil(try zero.next())
    }

    func testSpillablePointFaultsAtOpenAndKeepsItsOneReconstructedRecord() throws {
        let (database, device) = try fixture(); defer { try? database.close() }
        device.reset(reject: true)
        fails("58030") { try database.query("SELECT value FROM t WHERE id=40 ORDER BY id") }
        XCTAssertGreaterThan(device.reads, 0)
        XCTAssertEqual(database.core.livePinCount, 0)
        device.reset()
        let rows = try database.query("SELECT value FROM t WHERE id=40 ORDER BY id")
        XCTAssertEqual(device.reads, 1, "resident interior skeleton leaves one fused point fault")
        XCTAssertEqual(rows.cost, Int64(try tree(database).height))
        database.core.storage.pool.invalidateAll(); device.reset(reject: true)
        XCTAssertEqual(try rows.nextRow()?.get(0, as: String.self), "value")
        XCTAssertNil(try rows.next()); XCTAssertEqual(device.reads, 0)
        device.reset()
        let missing = try database.query("SELECT value FROM t WHERE id=400 ORDER BY id")
        XCTAssertEqual(missing.cost, Int64(try tree(database).height))
        XCTAssertNil(try missing.next())
    }

    func testRangeDistinctAndZeroLimitHaveOpenBlockWhileBlockingBodiesStayDeferred() throws {
        let (database, device) = try fixture(); defer { try? database.close() }
        let nodes = Int64(try tree(database).nodeCount)
        for sql in ["SELECT id FROM t ORDER BY id", "SELECT DISTINCT id FROM t ORDER BY id", "SELECT id FROM t LIMIT 0"] {
            database.core.storage.pool.invalidateAll(); device.reset(reject: true)
            let rows = try database.query(sql)
            XCTAssertEqual(rows.cost, nodes, sql); XCTAssertEqual(device.reads, 0, sql)
            rows.close()
        }
        for sql in ["SELECT id FROM t ORDER BY id+0", "SELECT sum(id) FROM t",
                    "SELECT id FROM t UNION ALL SELECT id FROM t", "WITH q AS MATERIALIZED (SELECT id FROM t) SELECT id FROM q"] {
            database.core.storage.pool.invalidateAll(); device.reset(reject: true)
            let rows = try database.query(sql)
            XCTAssertEqual(rows.cost, 0, sql); XCTAssertEqual(device.reads, 0, sql)
            fails("58030") { try rows.next() }
        }
        database.core.storage.pool.invalidateAll(); device.reset(reject: true)
        let empty = try database.query("SELECT id FROM t WHERE false ORDER BY id")
        XCTAssertEqual(empty.cost, 0); XCTAssertNil(try empty.next()); XCTAssertEqual(device.reads, 0)
        XCTAssertEqual(database.core.livePinCount, 0)
        device.reset()
    }
}
