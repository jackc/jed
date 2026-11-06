import Synchronization
import XCTest
@testable import Jed

final class HostBatchScanBoundaryTests: XCTestCase {
    private final class Calls: Sendable {
        private let state = Mutex<[Int]>([])
        func withLock<T>(_ body: (inout [Int]) -> T) -> T { state.withLock { body(&$0) } }
    }
    private func database(_ calls: Calls, volatility: HostFunction.Volatility = .stable) throws -> Database {
        var builder = Environment.builder()
        try builder.register(HostFunction(name: SQLText("batch_id"), arguments: [.i64], result: .i64, cost: 3,
            volatility: volatility, batchKernel: { columns, output in
                calls.withLock { $0.append(columns[0].count) }
                output.append(contentsOf: columns[0])
            }))
        try builder.register(HostFunction(name: SQLText("scalar_id"), arguments: [.i64], result: .i64, cost: 3,
            volatility: volatility, kernel: { $0[0] }))
        let database = try Database(environment: builder.build(), pageSize: 512)
        try database.execute("CREATE TABLE t(id i64 PRIMARY KEY,x i64)")
        try database.execute("INSERT INTO t SELECT i,i FROM generate_series(1,130) g(i)")
        return database
    }
    private struct Outcome: Equatable { var values: [Int64] = []; var cost: Int64 = 0; var error: String? }
    private func run(_ database: Database, _ sql: String, limit: Int64 = 0) throws -> Outcome {
        var options = SessionOptions(); options.maxCost = limit
        let session = try database.session(options: options); defer { session.close() }
        var result = Outcome()
        let rows = try session.query(sql); defer { rows.close() }
        do {
            while let row = try rows.next() {
                for value in row { if case .integer(let n) = value { result.values.append(n) } }
            }
        } catch { result.error = (error as? JedError)?.code }
        result.cost = rows.cost; return result
    }

    func testPrimaryScanWindowsLimitCloseAndScalarReplayCosts() throws {
        let calls = Calls(), database = try database(calls); defer { try? database.close() }
        let expected = try run(database, "SELECT scalar_id(x) FROM t ORDER BY id")
        XCTAssertEqual(try run(database, "SELECT batch_id(x) FROM t ORDER BY id"), expected)
        XCTAssertEqual(calls.withLock { $0 }, [64, 64, 2])
        calls.withLock { $0 = [] }
        let one = try run(database, "SELECT batch_id(x) FROM t ORDER BY id LIMIT 1")
        XCTAssertEqual(one.values, [1]); XCTAssertEqual(calls.withLock { $0 }, [1])
        calls.withLock { $0 = [] }
        XCTAssertEqual(try run(database, "SELECT DISTINCT id,batch_id(id) FROM t ORDER BY id LIMIT 1").values, [1, 1])
        XCTAssertEqual(calls.withLock { $0 }, [1])
        calls.withLock { $0 = [] }
        let cursor = try database.query("SELECT batch_id(x) FROM t ORDER BY id")
        XCTAssertNotNil(try cursor.next()); cursor.close()
        XCTAssertEqual(calls.withLock { $0 }, [64]); XCTAssertEqual(database.core.livePinCount, 0)
        for limit: Int64 in [20, 70, 200] {
            let scalar = try run(database, "SELECT scalar_id(x) FROM t ORDER BY id", limit: limit)
            XCTAssertEqual(try run(database, "SELECT batch_id(x) FROM t ORDER BY id", limit: limit), scalar)
        }
    }

    func testFilterProjectionAndIntervalWindowsKeepCanonicalResultsAndCharges() throws {
        let calls = Calls(), database = try database(calls); defer { try? database.close() }
        for suffix in ["WHERE batch_id(x)>100 ORDER BY id OFFSET 3 LIMIT 4",
                       "WHERE id IN (1,3,5,90,130) ORDER BY id LIMIT 4",
                       "WHERE id<5 OR id>125 ORDER BY id"] {
            calls.withLock { $0 = [] }
            let sql = "SELECT batch_id(x) FROM t " + suffix
            XCTAssertEqual(try run(database, sql), try run(database, sql.replacingOccurrences(of: "batch_id", with: "scalar_id")))
            XCTAssertTrue(calls.withLock { !$0.isEmpty && $0.allSatisfy { $0 > 0 && $0 <= 64 } })
        }
    }

    func testConditionalAndVolatileCallsStayBatchOfOne() throws {
        for volatility: HostFunction.Volatility in [.stable, .volatile] {
            let calls = Calls(), database = try database(calls, volatility: volatility)
            defer { try? database.close() }
            let sql = volatility == .stable
                ? "SELECT CASE WHEN id%2=0 THEN batch_id(x) ELSE 0::i64 END FROM t ORDER BY id"
                : "SELECT batch_id(x) FROM t ORDER BY id"
            XCTAssertEqual(try run(database, sql), try run(database, sql.replacingOccurrences(of: "batch_id", with: "scalar_id")))
            XCTAssertEqual(calls.withLock { $0 }, Array(repeating: 1, count: volatility == .stable ? 65 : 130))
        }
    }

    func testPlainProjectionUsesTheCanonicalInHandChunk() throws {
        let calls = Calls(), database = try database(calls); defer { try? database.close() }
        try database.execute("DELETE FROM t")
        try database.execute("INSERT INTO t SELECT i,CASE WHEN i%10=0 THEN NULL ELSE i END FROM generate_series(1,1100) g(i)")
        XCTAssertEqual(try run(database, "SELECT batch_id(x) FROM t"), try run(database, "SELECT scalar_id(x) FROM t"))
        XCTAssertEqual(calls.withLock { $0 }, [922, 68])
    }

    func testCorrelatedResidentArgumentsBatchWithoutCallingAnArbitraryOuterGetter() throws {
        let calls = Calls(), database = try database(calls); defer { try? database.close() }
        let sql = "SELECT (SELECT sum(v) FROM (SELECT batch_id(t.x) AS v FROM t q WHERE q.id<4 ORDER BY q.id) s) FROM t WHERE id=2"
        XCTAssertEqual(try run(database, sql), try run(database, sql.replacingOccurrences(of: "batch_id", with: "scalar_id")))
        XCTAssertTrue(calls.withLock { $0.contains { $0 > 1 } })
    }
}
