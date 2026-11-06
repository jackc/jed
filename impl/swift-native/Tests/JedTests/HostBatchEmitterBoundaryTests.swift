import Foundation
import Synchronization
import XCTest
@testable import Jed

/// Host callback shape, speculative failure replay and scratch ownership are
/// embedding contracts; SQL values and costs remain covered by shared records.
final class HostBatchEmitterBoundaryTests: XCTestCase {
    private final class Calls: Sendable {
        private let state = Mutex([[SQLValue]]())
        var rows: [[SQLValue]] { state.withLock { $0 } }
        func append(_ values: [SQLValue]) { state.withLock { $0.append(values) } }
        func reset() { state.withLock { $0 = [] } }
    }
    private final class ScratchHost: ScratchFileHost {
        private let inner: any ScratchFileHost
        private let state = Mutex((created: 0, active: 0))
        var sealed: Bool { inner.sealed }
        var created: Int { state.withLock { $0.created } }
        var active: Int { state.withLock { $0.active } }
        init(_ inner: any ScratchFileHost) { self.inner = inner }
        func create(kind: String) throws -> any ScratchStore {
            let store = try inner.create(kind: kind)
            state.withLock { $0.created += 1; $0.active += 1 }
            return ScratchHandle(store) { self.state.withLock { $0.active -= 1 } }
        }
    }
    private final class ScratchHandle: ScratchStore {
        private let inner: any ScratchStore
        private let released: @Sendable () -> Void
        private let closed = Mutex(false)
        init(_ inner: any ScratchStore, released: @escaping @Sendable () -> Void) {
            self.inner = inner; self.released = released
        }
        func read(at offset: UInt64, count: Int) throws -> [UInt8] { try inner.read(at: offset, count: count) }
        func write(at offset: UInt64, bytes: [UInt8]) throws { try inner.write(at: offset, bytes: bytes) }
        func size() throws -> UInt64 { try inner.size() }
        func setSize(_ bytes: UInt64) throws { try inner.setSize(bytes) }
        func sync() throws { try inner.sync() }
        func close() throws {
            let first = closed.withLock { value in
                if value { return false }; value = true; return true
            }
            guard first else { return }
            defer { released() }
            try inner.close()
        }
    }
    private struct Result {
        let rows: [[SQLValue]]
        let error: String?
        let cost: Int64
    }
    private func fixture(failingAt: Int64? = nil) throws -> (Database, Environment, Calls) {
        let calls = Calls()
        var builder = Environment.builder()
        try builder.register(HostFunction(name: SQLText("batch_value"), arguments: [.i64], result: .i64,
            cost: 3, volatility: .stable, batchKernel: { columns, output in
                calls.append(columns[0])
                for value in columns[0] {
                    if let failingAt, case .integer(let number) = value, number == failingAt {
                        throw JedError("22012", "injected host failure")
                    }
                    output.append(value)
                }
            }))
        try builder.register(HostFunction(name: SQLText("scalar_value"), arguments: [.i64], result: .i64,
            cost: 3, volatility: .stable, kernel: { values in
                if let failingAt, case .integer(let number) = values[0], number == failingAt {
                    throw JedError("22012", "injected host failure")
                }
                return values[0]
            }))
        let environment = builder.build(), database = try Database(environment: environment)
        try database.execute("CREATE TABLE t(id i64 PRIMARY KEY, k i64, v i64)")
        let values = (0..<131).map { "(\($0),\($0 % 17),\(130 - $0))" }.joined(separator: ",")
        try database.execute("INSERT INTO t VALUES " + values)
        return (database, environment, calls)
    }
    private func run(_ sql: String, snapshot: DomainSnapshot, environment: Environment,
                     host: (any ScratchFileHost)? = nil, limit: Int64 = 0,
                     stopAfter: Int? = nil, requireBoundedJoin: Bool = false) throws -> Result {
        let diagnostics = MemoryDiagnostics(), resources = StatementResources(queryLimit: 1_000_000, diagnostics: diagnostics)
        let meter = CostMeter(limit: limit, resources: resources)
        var settings = SessionSettings(environment: environment, options: SessionOptions())
        settings.options.workMemoryBytes = host == nil ? 0 : 128
        settings.scratchFileHost = host
        let context = StatementServices(settings: settings, meter: meter).context()
        guard case .query(let query) = try SQLParser(sql).parse() else { throw JedError("XX000", "expected SELECT") }
        var plan = try SelectPlan.resolve(query, snapshot: snapshot, settings: settings, scalar: context.scalar)
        if requireBoundedJoin {
            plan = try plan.selectAccess(parameters: context.parameters, context: context)
            XCTAssertTrue(BlockingJoinRows.eligible(plan), "probe must reach the bounded join emitter")
        }
        let execution = SelectExecution(plan: plan, context: context)
        var rows: [[SQLValue]] = [], failure: String?
        do {
            while stopAfter == nil || rows.count < stopAfter! {
                guard let row = try execution.next() else { break }
                rows.append(row)
            }
        } catch { failure = (error as? JedError)?.code ?? String(describing: error) }
        execution.close(); execution.close()
        XCTAssertEqual(resources.query.used, 0, sql)
        XCTAssertEqual(diagnostics.underflows, 0, sql)
        if let tracked = host as? ScratchHost { XCTAssertEqual(tracked.active, 0, sql) }
        return Result(rows: rows, error: failure, cost: meter.accrued)
    }
    private func compare(_ query: String, snapshot: DomainSnapshot, environment: Environment, calls: Calls,
                         host: (any ScratchFileHost)? = nil, maximum: Int,
                         stopAfter: Int? = nil, requireBoundedJoin: Bool = false) throws -> (Result, Result) {
        calls.reset()
        let batch = try run(query.replacingOccurrences(of: "FN", with: "batch_value"), snapshot: snapshot,
            environment: environment, host: host, stopAfter: stopAfter, requireBoundedJoin: requireBoundedJoin)
        let lengths = calls.rows.map(\.count)
        XCTAssertTrue(lengths.contains { $0 > 1 }, "eligible projection should use multi-row calls: \(query)")
        XCTAssertTrue(lengths.allSatisfy { $0 <= maximum }, query)
        let scalar = try run(query.replacingOccurrences(of: "FN", with: "scalar_value"), snapshot: snapshot,
            environment: environment, host: host, stopAfter: stopAfter, requireBoundedJoin: requireBoundedJoin)
        XCTAssertEqual(try batch.rows.map(SpillRowCodec.encode), try scalar.rows.map(SpillRowCodec.encode), query)
        XCTAssertEqual(batch.error, scalar.error, query)
        XCTAssertEqual(batch.cost, scalar.cost, query)
        return (batch, scalar)
    }

    func testInHandOrderedGroupedAndWindowProjectionReplayScalarResultsAndCost() throws {
        let (database, environment, calls) = try fixture(); defer { try? database.close() }
        let pin = database.core.pinLatest(); defer { pin.close() }
        for query in [
            "SELECT FN(id) FROM t ORDER BY v+0 OFFSET 2 LIMIT 125",
            "SELECT FN(id),count(*) FROM t GROUP BY id ORDER BY id",
            "SELECT FN(row_number() OVER(ORDER BY id)) FROM t ORDER BY id",
        ] {
            let (result, _) = try compare(query, snapshot: pin.snapshot, environment: environment, calls: calls, maximum: 1024)
            XCTAssertNil(result.error)
        }
    }

    func testInHandFailureAndCostAbortRemainOnTheScalarReplayRow() throws {
        let (database, environment, calls) = try fixture(failingAt: 17); defer { try? database.close() }
        let pin = database.core.pinLatest(); defer { pin.close() }
        let query = "SELECT FN(id) FROM t ORDER BY id+0"
        let (failure, _) = try compare(query, snapshot: pin.snapshot, environment: environment, calls: calls, maximum: 1024)
        XCTAssertEqual(failure.error, "22012")
        // Determine the phase boundary from the scalar execution itself; do not
        // duplicate the engine's scan, sort or host-cost formulas in the test.
        let prefix = try run(query.replacingOccurrences(of: "FN", with: "scalar_value"), snapshot: pin.snapshot,
                             environment: environment, stopAfter: 3)
        calls.reset()
        let batch = try run(query.replacingOccurrences(of: "FN", with: "batch_value"), snapshot: pin.snapshot,
                            environment: environment, limit: prefix.cost)
        let scalar = try run(query.replacingOccurrences(of: "FN", with: "scalar_value"), snapshot: pin.snapshot,
                             environment: environment, limit: prefix.cost)
        XCTAssertEqual(batch.error, "54P01")
        XCTAssertEqual(try batch.rows.map(SpillRowCodec.encode), try scalar.rows.map(SpillRowCodec.encode)); XCTAssertEqual(batch.cost, scalar.cost)
        XCTAssertTrue(calls.rows.allSatisfy { $0.count <= 4 })
    }

    func testSpoolReplayPullRetainsChargeUntilTheWindowOwnerCloses() throws {
        let diagnostics = MemoryDiagnostics(), resources = StatementResources(queryLimit: 1_000_000, diagnostics: diagnostics)
        let meter = CostMeter(resources: resources)
        let spool = RowSpool(config: SpillConfig(workMemoryBytes: 0, host: nil, meter: meter))
        try spool.append([.integer(1)]); try spool.append([.integer(2)])
        let held = resources.query.used, cursor = try spool.takeOutput()
        XCTAssertNotNil(try cursor.pullForReplay())
        XCTAssertNotNil(try cursor.pullForReplay())
        XCTAssertNil(try cursor.pullForReplay())
        XCTAssertEqual(resources.query.used, held, "EOF must not release rows retained by read-ahead")
        cursor.close(); cursor.close()
        XCTAssertEqual(resources.query.used, 0); XCTAssertEqual(diagnostics.underflows, 0)
    }

    #if os(Linux) || canImport(Darwin)
    private func scratch(_ body: (any ScratchFileHost) throws -> Void) throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-batch-emitter-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let host = ScratchHost(NativeScratchFileHost(directory: directory.path, sealed: true,
            names: SeededRandomSource(seed: 191), keys: SeededRandomSource(seed: 193)))
        try body(host)
        XCTAssertGreaterThan(host.created, 0)
        XCTAssertEqual(host.active, 0)
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [])
    }
    func testPulledSortAndSpoolEmittersBatchAndCloseAfterPartialDrain() throws {
        let (database, environment, calls) = try fixture(); defer { try? database.close() }
        let pin = database.core.pinLatest(); defer { pin.close() }
        try scratch { host in
            let probes: [(String, Bool)] = [
                ("SELECT FN(id) FROM t ORDER BY v", false),
                ("SELECT FN(id),count(*) FROM t GROUP BY id ORDER BY id", false),
                ("SELECT FN(row_number() OVER(ORDER BY id)),count(*) FROM t GROUP BY id ORDER BY id", false),
                ("SELECT DISTINCT ON(k) FN(id) FROM t ORDER BY k,id", false),
                ("SELECT FN(a.id) FROM t a LEFT JOIN t b ON a.k=b.k ORDER BY a.id+0,b.id", true),
            ]
            for (query, join) in probes {
                let (result, _) = try compare(query, snapshot: pin.snapshot, environment: environment,
                    calls: calls, host: host, maximum: 64, stopAfter: 2, requireBoundedJoin: join)
                XCTAssertNil(result.error)
            }
        }
    }
    func testPulledBatchPrefixFailureAndLimitDoNotExposeLaterOutcomes() throws {
        let (database, environment, calls) = try fixture(failingAt: 17); defer { try? database.close() }
        let pin = database.core.pinLatest(); defer { pin.close() }
        try scratch { host in
            for query in [
                "SELECT FN(id) FROM t ORDER BY v DESC",
                "SELECT FN(id),count(*) FROM t GROUP BY id ORDER BY id",
                "SELECT FN(row_number() OVER(ORDER BY id)),count(*) FROM t GROUP BY id ORDER BY id",
            ] {
                let (failure, _) = try compare(query, snapshot: pin.snapshot, environment: environment,
                    calls: calls, host: host, maximum: 64)
                XCTAssertEqual(failure.error, "22012")
                let (window, _) = try compare(query + " LIMIT 3", snapshot: pin.snapshot, environment: environment,
                    calls: calls, host: host, maximum: 3)
                XCTAssertNil(window.error)
            }
        }
    }
    #endif
}
