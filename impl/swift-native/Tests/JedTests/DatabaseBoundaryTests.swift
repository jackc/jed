import XCTest
import Synchronization
@testable import Jed

final class DatabaseBoundaryTests: XCTestCase {
    private func fails(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }
    private func text(_ value: SQLValue) -> String? { if case .text(let text) = value { return text.description }; return nil }

    func testConvenienceSessionsDoNotRetainVariablesAndExplicitSessionsDo() throws {
        let database = try Database(), session = try database.session()
        try session.setVariable(SQLText("myapp.tenant"), value: SQLText("one"))
        XCTAssertEqual(session.variable(try SQLText("MYAPP.TENANT"))?.description, "one")
        let explicit = try session.query("SELECT current_setting('MYAPP.TENANT')")
        XCTAssertEqual(try explicit.next()?.first.flatMap(text), "one")
        let fresh = try database.query("SELECT current_setting('myapp.tenant', true)")
        XCTAssertTrue(try XCTUnwrap(fresh.next()?.first).isNull)
        explicit.close(); fresh.close()
        XCTAssertEqual(database.core.livePinCount, 0)
    }
    func testCursorOwnsIndependentPinAndDelaysHostKernelUntilPull() throws {
        let calls = Mutex(0)
        var builder = Environment.builder()
        try builder.register(HostFunction(name: SQLText("probe"), arguments: [], result: .i64, kernel: { _ in
            calls.withLock { $0 += 1 }; return .integer(7)
        }))
        let database = try Database(environment: builder.build()), session = try database.readSession()
        let rows = try session.query("SELECT probe() AS value")
        XCTAssertEqual(calls.withLock { $0 }, 0)
        XCTAssertEqual(database.core.livePinCount, 2)
        XCTAssertEqual(rows.columns.map { $0.name.description }, ["value"])
        session.close(); XCTAssertEqual(database.core.livePinCount, 1)
        _ = try rows.next(); XCTAssertEqual(calls.withLock { $0 }, 1)
        XCTAssertNil(try rows.next()); XCTAssertEqual(database.core.livePinCount, 0)
        XCTAssertTrue(rows.isClosed); rows.close()
    }
    func testCursorFreezesStatementLimitsAndSharesLifetimeAccrual() throws {
        let database = try Database(), session = try database.session()
        session.maxCost = 5
        let old = try session.query("SELECT 1 + 2")
        session.maxCost = 1
        _ = try old.next(); XCTAssertEqual(old.cost, 2); old.close()
        fails("54P01") { _ = try session.query("SELECT 1").next() }
        XCTAssertEqual(session.lifetimeCost, 3)
        session.maxCost = 0
        let unlimited = try session.query("SELECT 1")
        session.lifetimeMaxCost = 1
        _ = try unlimited.next(); unlimited.close()
        XCTAssertEqual(session.lifetimeCost, 4)
        fails("54P02") { _ = try session.query("SELECT 1") }
        XCTAssertEqual(session.lifetimeCost, 4)
        XCTAssertEqual(database.core.livePinCount, 0)
    }
    func testCollectorAdmissionFailureAbortsTransactionAndClosesPin() throws {
        let database = try Database(), session = try database.session()
        session.maxQueryMemoryBytes = 1
        try session.begin(writable: false)
        let streamed = try session.query("SELECT 1")
        XCTAssertNotNil(try streamed.next()); streamed.close()
        XCTAssertEqual(session.status, .open)
        let collected = try session.query("SELECT 1")
        fails("54P05") { _ = try collected.collect() }
        XCTAssertEqual(collected.error?.code, "54P05")
        XCTAssertTrue(collected.isClosed); XCTAssertEqual(session.status, .failed)
        XCTAssertEqual(database.core.livePinCount, 1)
        try session.rollback(); XCTAssertEqual(database.core.livePinCount, 0)
    }
    func testCancelledDeferredCursorDoesNotRunKernelOrLeakPin() throws {
        let calls = Mutex(0)
        var builder = Environment.builder()
        try builder.register(HostFunction(name: SQLText("probe"), arguments: [], result: .i64, kernel: { _ in
            calls.withLock { $0 += 1 }; return .integer(1)
        }))
        let database = try Database(environment: builder.build()), token = CancellationToken()
        let rows = try database.query("SELECT probe()", cancellation: token)
        token.cancel(); fails("57014") { _ = try rows.next() }
        XCTAssertEqual(calls.withLock { $0 }, 0)
        XCTAssertEqual(database.core.livePinCount, 0)
    }
    func testScopedTransactionCannotCommitItselfAndAlwaysReleasesGate() throws {
        let database = try Database(), session = try database.session()
        fails("2D000") { try session.update { try $0.commit() } }
        XCTAssertEqual(session.status, .idle)
        XCTAssertEqual(try database.core.maintenance { 4 }, 4)
        try session.begin()
        fails("55006") { try database.close() }
        XCTAssertFalse(database.isClosed)
        session.close(); try database.close(); try database.close()
        fails("55000") { _ = try database.session() }
    }
    func testBindingsValidateEveryInputBeforePullAndDoNotReadClock() throws {
        let calls = Mutex(0)
        let clock = AdvancingClock(start: 100, step: 10)
        var builder = Environment.builder(); builder.setClockSource(clock)
        try builder.register(HostFunction(name: SQLText("probe"), arguments: [], result: .i64, kernel: { _ in
            calls.withLock { $0 += 1 }; return .integer(1)
        }))
        let database = try Database(environment: builder.build()), session = try database.session()
        fails("42804") { _ = try session.query("SELECT probe(), $1::i16, $2::text", parameters: [.integer(1), .integer(2)]) }
        fails("22003") { _ = try session.query("SELECT probe(), $1::i16", parameters: [.integer(40_000)]) }
        fails("42601") { _ = try session.query("SELECT probe()", parameters: [.integer(1)]) }
        fails("42P18") { _ = try session.query("SELECT $2::i32", parameters: [.integer(1), .integer(2)]) }
        fails("42P18") { _ = try session.query("SELECT $1", parameters: [.integer(1)]) }
        fails("22007") { _ = try session.query("SELECT $1::date", parameters: [.text(SQLText("today"))]) }
        XCTAssertEqual(try clock.microseconds(), 100)
        XCTAssertEqual(calls.withLock { $0 }, 0)
        XCTAssertEqual(session.lifetimeCost, 0)
        XCTAssertEqual(database.core.livePinCount, 0)
        fails("42601") { _ = try session.query("BEGIN", parameters: [.integer(1)]) }
        XCTAssertEqual(session.status, .idle)
    }
}
