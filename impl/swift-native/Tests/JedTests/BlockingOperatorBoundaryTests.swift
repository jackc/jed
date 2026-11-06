import Foundation
import Synchronization
import XCTest
@testable import Jed

/// Native scratch paths are unlinked at creation. Track the borrowed host
/// boundary instead of mistaking an empty directory for a closed descriptor.
private final class OperatorScratchHost: ScratchFileHost {
    private struct State { var created = 0; var active = 0 }
    private let state = Mutex(State())
    private let inner: any ScratchFileHost
    var sealed: Bool { inner.sealed }
    var created: Int { state.withLock { $0.created } }
    var active: Int { state.withLock { $0.active } }
    init(_ inner: any ScratchFileHost) { self.inner = inner }
    func create(kind: String) throws -> any ScratchStore {
        let store = try inner.create(kind: kind)
        state.withLock { $0.created += 1; $0.active += 1 }
        return OperatorScratchStore(store) { self.state.withLock { $0.active -= 1 } }
    }
}
private final class OperatorScratchStore: ScratchStore {
    private let inner: any ScratchStore
    private let closed = Mutex(false)
    private let released: @Sendable () -> Void
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

final class BlockingOperatorBoundaryTests: XCTestCase {
    #if os(Linux) || canImport(Darwin)
    private func makeDatabase(_ environment: Environment) throws -> Database {
        let database = try Database(environment: environment)
        try database.execute("CREATE TABLE t (id i64 PRIMARY KEY, k i64, v i64)")
        let values = (0..<17).map { "(\($0),\($0 % 3),\($0))" }.joined(separator: ",")
        try database.execute("INSERT INTO t VALUES " + values)
        return database
    }

    private func execute(_ sql: String, snapshot: DomainSnapshot, environment: Environment,
                         host: any ScratchFileHost, resources: StatementResources, requireBoundedJoin: Bool = false) throws -> SelectExecution {
        var settings = SessionSettings(environment: environment, options: SessionOptions())
        settings.options.workMemoryBytes = 128
        settings.scratchFileHost = host
        guard case .query(let syntax) = try SQLParser(sql).parse() else {
            throw JedError("XX000", "boundary test expected SELECT")
        }
        let context = StatementServices(settings: settings, meter: CostMeter(resources: resources)).context()
        var plan = try SelectPlan.resolve(syntax, snapshot: snapshot, settings: settings, scalar: context.scalar)
        if requireBoundedJoin {
            plan = try plan.selectAccess(parameters: context.parameters, context: context)
            XCTAssertTrue(BlockingJoinRows.eligible(plan), "cleanup probe must reach the bounded join driver")
        }
        return SelectExecution(plan: plan, context: context)
    }

    func testEarlyCloseReleasesBoundedOperatorFilesAndCharges() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-operator-close-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let host = OperatorScratchHost(NativeScratchFileHost(directory: directory.path, sealed: true,
            names: SeededRandomSource(seed: 71), keys: SeededRandomSource(seed: 73)))
        let environment = Environment.builder().build(), database = try makeDatabase(environment)
        defer { try? database.close() }
        let pin = database.core.pinLatest(); defer { pin.close() }
        // Values and costs belong to the shared corpus. These probes assert
        // only the host lifetime of the output owner after a partial drain.
        let probes: [(String, Int, Bool)] = [
            ("SELECT k, array_agg(v ORDER BY v) FROM t GROUP BY k ORDER BY k", 1, false),
            ("SELECT k, sum(v), row_number() OVER (ORDER BY k) FROM t GROUP BY k ORDER BY k", 1, false),
            ("SELECT DISTINCT ON (k) k, v FROM t ORDER BY k, v DESC", 1, false),
            ("SELECT DISTINCT id FROM t ORDER BY id", 6, false),
            ("SELECT a.id, row_number() OVER (ORDER BY a.id, b.id) FROM t a LEFT JOIN t b ON a.k=b.k ORDER BY a.id,b.id", 1, true),
        ]
        for (sql, count, boundedJoin) in probes {
            let created = host.created
            let diagnostics = MemoryDiagnostics()
            let resources = StatementResources(queryLimit: 1_000_000, diagnostics: diagnostics)
            let execution = try execute(sql, snapshot: pin.snapshot, environment: environment, host: host, resources: resources, requireBoundedJoin: boundedJoin)
            defer { execution.close() }
            for _ in 0..<count { XCTAssertNotNil(try execution.next(), sql) }
            XCTAssertGreaterThan(host.created, created, sql)
            XCTAssertGreaterThan(host.active, 0, sql)
            execution.close(); execution.close()
            XCTAssertEqual(resources.query.used, 0, sql)
            XCTAssertEqual(diagnostics.underflows, 0, sql)
            XCTAssertEqual(host.active, 0, sql)
            XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [], sql)
        }
    }

    func testFailedFoldAndWindowAdmissionReleaseEveryScratchOwner() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-operator-failure-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let host = OperatorScratchHost(NativeScratchFileHost(directory: directory.path, sealed: true,
            names: SeededRandomSource(seed: 79), keys: SeededRandomSource(seed: 83)))
        var builder = Environment.builder()
        try builder.register(HostFunction(name: SQLText("fail_fold"), arguments: [.i64], result: .i64, kernel: { arguments in
            if case .integer(13) = arguments[0] { throw JedError("22012", "injected fold failure") }
            return arguments[0]
        }))
        let environment = builder.build(), database = try makeDatabase(environment)
        defer { try? database.close() }
        let pin = database.core.pinLatest(); defer { pin.close() }
        let probes: [(String, Int64, String)] = [
            ("SELECT k, sum(fail_fold(v)) FROM t GROUP BY k", 1_000_000, "22012"),
            ("SELECT k, count(*), row_number() OVER (ORDER BY k) FROM t GROUP BY k", 1, "54P05"),
        ]
        for (sql, limit, code) in probes {
            let created = host.created
            let diagnostics = MemoryDiagnostics(), resources = StatementResources(queryLimit: limit, diagnostics: diagnostics)
            let execution = try execute(sql, snapshot: pin.snapshot, environment: environment, host: host, resources: resources)
            XCTAssertThrowsError(try execution.next(), sql) { XCTAssertEqual(($0 as? JedError)?.code, code) }
            execution.close(); execution.close()
            XCTAssertEqual(resources.query.used, 0, sql)
            XCTAssertEqual(diagnostics.underflows, 0, sql)
            XCTAssertGreaterThan(host.created, created, sql)
            XCTAssertEqual(host.active, 0, sql)
            XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: directory.path), [], sql)
        }
    }
    #endif
}
