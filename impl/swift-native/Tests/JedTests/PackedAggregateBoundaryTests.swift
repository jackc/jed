import Synchronization
import XCTest
@testable import Jed

private final class PackedAggregateDevice: BlockStore {
    private struct State {
        var reads = 0
        var reject = false
        var cancellation: CancellationToken?
    }
    private let state = Mutex(State())
    private let bytes = MemoryBlockStore()
    var reads: Int { state.withLock { $0.reads } }
    func arm(reject: Bool = false, cancellation: CancellationToken? = nil) {
        state.withLock { $0 = State(reject: reject, cancellation: cancellation) }
    }
    func read(at offset: UInt64, count: Int) throws -> [UInt8] {
        let action = state.withLock { value in
            value.reads += 1
            let result = (value.reject, value.cancellation)
            value.cancellation = nil
            return result
        }
        action.1?.cancel()
        if action.0 { throw JedError("58030", "injected packed aggregate read failure") }
        return try bytes.read(at: offset, count: count)
    }
    func write(at offset: UInt64, bytes: [UInt8]) throws { try self.bytes.write(at: offset, bytes: bytes) }
    func size() throws -> UInt64 { try bytes.size() }
    func setSize(_ bytes: UInt64) throws { try self.bytes.setSize(bytes) }
    func sync() throws { try bytes.sync() }
}

/// Representation and ownership probes. Shared SQL owns aggregate semantics;
/// these compare the packed and scalar lanes over the same pinned tree.
final class PackedAggregateBoundaryTests: XCTestCase {
    private struct Result {
        let rows: [[UInt8]]
        let cost: Int64
        let peak: Int64
        let error: String?
    }

    private func fixture() throws -> (Database, PackedAggregateDevice) {
        let device = PackedAggregateDevice(), environment = Environment.builder().build()
        let storage = try StorageDomain.create(store: device, kind: .file,
            catalogID: Database.allocateCatalogID(), pageSize: 512)
        let database = Database(core: DatabaseCore(storage: storage), environment: environment)
        let extra = (0..<24).map { "c\($0) i32" }.joined(separator: ",")
        try database.execute("CREATE TABLE t(id i32 PRIMARY KEY,a i16,b i32,n i32,r f32,d f64,pad bytea," + extra + ")")
        let values = Array(repeating: "g", count: 24).joined(separator: ",")
        try database.execute("INSERT INTO t SELECT g,CASE WHEN g%3=0 THEN NULL ELSE g::i16 END," +
            "CASE WHEN g%5=0 THEN NULL ELSE 2000000000::i32 END,NULL," +
            "CASE WHEN g%7=0 THEN NULL ELSE g::f32 END,g::f64/10::f64,NULL," +
            values + " FROM generate_series(1,96) s(g)")
        try database.execute("CREATE TABLE e(id i32 PRIMARY KEY,a i16,b i32,n i32,r f32,d f64)")
        XCTAssertGreaterThan(try XCTUnwrap(database.core.routingSnapshot.tables[SQLText("t")]).tree.height, 1)
        return (database, device)
    }

    private func reopen(_ database: Database, device: PackedAggregateDevice) throws -> Database {
        let environment = database.environment
        try database.close()
        let storage = try StorageDomain.open(store: device, kind: .file,
            catalogID: Database.allocateCatalogID(), cacheBytes: 1,
            validateCatalog: { try CatalogEnvironment.validate($0, environment: environment) })
        return Database(core: DatabaseCore(storage: storage), environment: environment)
    }

    private func run(_ sql: String, snapshot: DomainSnapshot, environment: Environment,
                     costLimit: Int64 = 0, memoryLimit: Int64 = 1_000_000,
                     cancellation: CancellationToken? = nil, expectPacked: Bool = true) throws -> Result {
        let diagnostics = MemoryDiagnostics()
        let resources = StatementResources(queryLimit: memoryLimit, diagnostics: diagnostics)
        let meter = CostMeter(limit: costLimit, resources: resources, cancellation: cancellation)
        var settings = SessionSettings(environment: environment, options: SessionOptions())
        settings.options.workMemoryBytes = 0
        let context = StatementServices(settings: settings, meter: meter).context()
        guard case .query(let syntax) = try SQLParser(sql).parse() else {
            throw JedError("XX000", "packed aggregate probe expected SELECT")
        }
        let resolved = try SelectPlan.resolve(syntax, snapshot: snapshot, settings: settings, scalar: context.scalar)
        let plan = try resolved.selectAccess(parameters: context.parameters, context: context)
        if let aggregate = plan.aggregation {
            XCTAssertEqual(ColumnarExecution.packedAggregation(plan, aggregate, context), expectPacked, sql)
        } else {
            XCTAssertEqual(ColumnarExecution.projection(plan, context), expectPacked, sql)
        }
        let execution = SelectExecution(plan: plan, context: context)
        defer { execution.close() }
        var output: [[UInt8]] = [], failure: String?
        do {
            while let row = try execution.next() { output.append(try SpillRowCodec.encode(row)) }
        } catch { failure = (error as? JedError)?.code ?? String(describing: error) }
        execution.close(); execution.close()
        XCTAssertEqual(resources.query.used, 0, sql)
        XCTAssertEqual(diagnostics.underflows, 0, sql)
        return Result(rows: output, cost: meter.accrued, peak: diagnostics.peak, error: failure)
    }

    private func compare(_ sql: String, snapshot: DomainSnapshot, environment: Environment,
                         expectPacked: Bool = true) throws {
        let direct = try run(sql, snapshot: snapshot, environment: environment, expectPacked: expectPacked)
        let scalar = try run(sql, snapshot: snapshot, environment: environment,
            costLimit: 1_000_000, expectPacked: false)
        XCTAssertNil(direct.error, sql); XCTAssertNil(scalar.error, sql)
        XCTAssertEqual(direct.rows, scalar.rows, sql)
        XCTAssertEqual(direct.cost, scalar.cost, sql)
    }

    func testWideMultilevelCleanReopenedAndDirtyTreesMatchScalarFold() throws {
        let (initial, device) = try fixture()
        var database = initial
        defer { try? database.close() }
        let probes = [
            "SELECT count(*),count(a),count(b),sum(a),sum(b) FROM t",
            "SELECT min(a),max(b),sum(r),avg(d),count(r),min(r),max(d) FROM t",
            "SELECT sum(a)+1,count(*) FROM t",
            "SELECT count(n),sum(n),min(n),max(n) FROM t",
            "SELECT count(*) FROM t LIMIT 0",
            "SELECT sum(b) FROM t OFFSET 1",
            "SELECT count(*),count(a),sum(b),sum(r),avg(d) FROM e",
        ]
        func check(_ snapshot: DomainSnapshot) throws {
            for sql in probes { try compare(sql, snapshot: snapshot, environment: database.environment) }
        }
        try check(database.core.routingSnapshot)
        database = try reopen(database, device: device)
        try check(database.core.routingSnapshot)
        let writer = try database.writeSession(); defer { writer.close() }
        try writer.execute("UPDATE t SET a=NULL,b=-7 WHERE id<=11")
        var state: UInt64 = 19, payload: [UInt8] = []
        for _ in 0..<1200 { state = state &* 6364136223846793005 &+ 1; payload.append(UInt8(truncatingIfNeeded: state >> 32)) }
        try writer.execute("UPDATE t SET pad=$1 WHERE id=12", parameters: [.bytea(payload)])
        let dirty = try XCTUnwrap(writer.transaction?.working.snapshots[DomainNames.main])
        let table = try XCTUnwrap(dirty.tables[SQLText("t")])
        var cursor = table.cursor(), sawPending = false
        while let position = try cursor.nextPosition() {
            if position.node.pending[position.row] != nil { sawPending = true }
        }
        cursor.close()
        XCTAssertTrue(sawPending, "dirty probe must retain an untouched overflow payload")
        try check(dirty)
        try compare("SELECT count(pad) FROM t", snapshot: dirty, environment: database.environment)
    }

    func testGroupEntryAndOutputHaveSeparateMemoryAdmissions() throws {
        let (database, device) = try fixture(); defer { try? database.close() }
        let snapshot = database.core.routingSnapshot, sql = "SELECT count(*) FROM t"
        let direct = try run(sql, snapshot: snapshot, environment: database.environment, memoryLimit: 64)
        XCTAssertNil(direct.error); XCTAssertEqual(direct.peak, 64)
        let scalar = try run(sql, snapshot: snapshot, environment: database.environment,
            costLimit: 1_000_000, expectPacked: false)
        XCTAssertNil(scalar.error); XCTAssertGreaterThan(scalar.peak, direct.peak)
        XCTAssertEqual(direct.rows, scalar.rows); XCTAssertEqual(direct.cost, scalar.cost)
        database.core.storage.pool.invalidateAll(); device.arm()
        let entryFailure = try run(sql, snapshot: snapshot, environment: database.environment, memoryLimit: 31)
        XCTAssertEqual(entryFailure.error, "54P05"); XCTAssertEqual(entryFailure.peak, 0)
        XCTAssertEqual(entryFailure.cost, 0); XCTAssertEqual(device.reads, 0)
        let outputFailure = try run(sql, snapshot: snapshot, environment: database.environment, memoryLimit: 63)
        XCTAssertEqual(outputFailure.error, "54P05"); XCTAssertEqual(outputFailure.peak, 32)
        XCTAssertEqual(outputFailure.cost, direct.cost - 1, "output admission precedes row production")
        XCTAssertTrue(outputFailure.rows.isEmpty)
    }

    func testFilteredBoundsGroupsAndProjectionShareThePackedReader() throws {
        let (initial, device) = try fixture()
        var database = initial
        defer { try? database.close() }
        try database.execute("UPDATE t SET pad=CASE WHEN id%3=0 THEN NULL ELSE ('p'||id::text)::bytea END")
        let probes = [
            "SELECT count(pad),min(pad),max(pad),sum(b) FROM t WHERE id>=11 AND id<73 AND b IS NOT NULL",
            "SELECT a,count(pad),sum(b) FROM t WHERE id<=40 GROUP BY a",
            "SELECT count(pad) FROM t WHERE id>200",
            "SELECT id,pad,id FROM t WHERE id<=40 AND b IS NOT NULL",
            "SELECT pad,id FROM t",
        ]
        func check(_ snapshot: DomainSnapshot) throws {
            for sql in probes { try compare(sql, snapshot: snapshot, environment: database.environment) }
        }
        try check(database.core.routingSnapshot)
        database = try reopen(database, device: device)
        try check(database.core.routingSnapshot)
        let pinned = database.core.routingSnapshot
        let writer = try database.writeSession(); defer { writer.close() }
        try writer.execute("UPDATE t SET pad='changed'::bytea,a=7 WHERE id<=20")
        let dirty = try XCTUnwrap(writer.transaction?.working.snapshots[DomainNames.main])
        try check(dirty)
        try check(pinned)
        let old = try run("SELECT count(pad),min(pad) FROM t", snapshot: pinned, environment: database.environment)
        let new = try run("SELECT count(pad),min(pad) FROM t", snapshot: dirty, environment: database.environment)
        XCTAssertNotEqual(old.rows, new.rows, "the pinned and dirty snapshots must stay independent")
    }

    func testVariableCountReadsNullnessWithoutDecodingBodiesOrFollowingOverflow() throws {
        let (database, device) = try fixture(); defer { try? database.close() }
        try database.execute("CREATE TABLE nullness(id i32 PRIMARY KEY,v bytea)")
        try database.execute("INSERT INTO nullness VALUES(1,'value'::bytea)")
        let name = try SQLText("nullness"), base = database.core.routingSnapshot
        let original = try XCTUnwrap(base.tables[name])
        var cursor = original.cursor()
        let record = try XCTUnwrap(cursor.next()); cursor.close()
        var external = ByteWriter()
        external.u8(2); external.u32(UInt32.max); external.u32(8)
        // Both cells have valid page-level tags. The first has an invalid
        // inline length; the second refers to a nonexistent overflow page.
        let bodies: [[UInt8]] = [[0,0,2,41], external.bytes]
        for body in bodies {
            var cells = record.cells
            cells[1] = StoredCell(bytes: body[...], width: 0)
            var table = original
            try table.put(PackedRecord(key: record.key, cells: cells), netStaging: false)
            var snapshot = base; snapshot.setTableStore(name, table)
            device.arm(reject: true)
            let counted = try run("SELECT count(v) FROM nullness", snapshot: snapshot, environment: database.environment)
            XCTAssertNil(counted.error)
            XCTAssertEqual(counted.rows, [try SpillRowCodec.encode([.integer(1)])])
            XCTAssertEqual(device.reads, 0, "nullness must not follow an overflow pointer")
            device.arm()
            let decoded = try run("SELECT min(v) FROM nullness", snapshot: snapshot, environment: database.environment)
            XCTAssertEqual(decoded.error, "XX001", "value consumers must still validate the stored body")
        }
    }

    func testCanceledAndFailedPackedReadsReleaseGroupOwnership() throws {
        let (initial, device) = try fixture()
        let database = try reopen(initial, device: device); defer { try? database.close() }
        let sql = "SELECT count(*),sum(b) FROM t", snapshot = database.core.routingSnapshot
        let token = CancellationToken()
        database.core.storage.pool.invalidateAll(); device.arm(cancellation: token)
        let canceled = try run(sql, snapshot: snapshot, environment: database.environment, cancellation: token)
        XCTAssertEqual(canceled.error, "57014"); XCTAssertTrue(canceled.rows.isEmpty)
        XCTAssertGreaterThan(device.reads, 0); XCTAssertGreaterThan(canceled.cost, 0)
        database.core.storage.pool.invalidateAll(); device.arm(reject: true)
        let failed = try run(sql, snapshot: snapshot, environment: database.environment)
        XCTAssertEqual(failed.error, "58030"); XCTAssertTrue(failed.rows.isEmpty)
        XCTAssertGreaterThan(device.reads, 0)
        device.arm()
        try compare(sql, snapshot: snapshot, environment: database.environment)
    }

    func testRetainedPositionKeepsPendingValuesAfterCursorAndTreeRelease() throws {
        let codec = try PlainPageCodec(pageSize: 256)
        let integer = TypeDescriptor(.scalar(.i32)), bytes = TypeDescriptor(.scalar(.bytea))
        let types = [integer, bytes, bytes]
        for compressed in [false, true] {
            let raw = (0..<4096).map { UInt8(truncatingIfNeeded: $0 % 71) }
            let pending = PendingOverflow(bytes: compressed ? LZ4.compress(raw) : raw,
                rawLength: raw.count, compressed: compressed)
            var tree = BPlusTree(codec: codec, types: types)
            for ordinal in 0..<40 {
                var key = ByteWriter(); key.u32(UInt32(ordinal))
                let cells = [
                    StoredCell(bytes: try ValueCodec.encodeBody(.integer(Int64(ordinal)), type: integer)[...], type: integer),
                    StoredCell(bytes: try pending.pointer(PageReference(0))[...], type: bytes),
                    StoredCell(bytes: nil, type: bytes),
                ]
                try tree.insert(PackedRecord(key: key.bytes, cells: cells, overflows: [1: pending]))
            }
            XCTAssertGreaterThan(tree.height, 1)
            var cursor = TreeCursor(tree)
            let retained = try XCTUnwrap(cursor.nextPosition())
            while try cursor.nextPosition() != nil {}
            cursor.close(); tree = BPlusTree(codec: codec, types: types)
            XCTAssertFalse(retained.cell(1).isNull); XCTAssertTrue(retained.cell(2).isNull)
            guard case .integer(0) = try retained.materialize(0, pager: nil),
                  case .bytea(let actual) = try retained.materialize(1, pager: nil) else {
                return XCTFail("retained packed position lost its values")
            }
            XCTAssertEqual(actual, raw)
            XCTAssertTrue(try retained.materialize(2, pager: nil).isNull)
            let frame = PackedLaneRow(width: types.count, touched: [0,1,2], pager: nil)
            frame.begin(retained)
            let saved = try frame.value(1)
            frame.close(); frame.close()
            guard case .bytea(let owned) = saved else { return XCTFail("packed row lost its returned value") }
            XCTAssertEqual(owned, raw, "returned values must outlive the frame and its leaf position")
        }
    }
}
