import XCTest
@testable import Jed

/// The cached single-row key read replaces the general prepared path for one
/// statement shape. Nothing about it may be observable: the same rows, the
/// same cost and the same errors as the statement run unprepared.
final class PreparedPointReadTests: XCTestCase {
    private func outcome(_ run: () throws -> Rows) -> String {
        do {
            let rows = try run()
            var lines: [String] = []
            while let row = try rows.next() { lines.append(String(describing: try SpillRowCodec.encode(row))) }
            return lines.joined(separator: "|") + " cost \(rows.cost)"
        } catch let error as JedError { return "error " + error.code }
        catch { return "error \(error)" }
    }
    private func fill(_ database: Database) throws {
        try database.execute("CREATE TABLE t(id i64 PRIMARY KEY, a i32, note text)")
        try database.execute("INSERT INTO t SELECT g, (g * 3)::i32, CASE WHEN g % 500 = 0 THEN " +
            "(SELECT string_agg(md5((g * 1000 + i)::text), '') FROM generate_series(1, 400) s2(i)) " +
            "WHEN g % 50 = 0 THEN repeat('x', 40000) WHEN g % 7 = 0 THEN NULL ELSE 'n' || g::text END " +
            "FROM generate_series(1, 3000) s(g)")
        try database.execute("CREATE TABLE pair(a i32, b i64, v i16, PRIMARY KEY (a, b))")
        try database.execute("INSERT INTO pair SELECT g % 40, g, (g % 300)::i16 FROM generate_series(1, 2000) s(g)")
    }
    private let single: [[SQLValue]] = [1, 7, 50, 500, 1500, 2999, 3000, 3001, -5, 0].map { [.integer($0)] }
    private let double: [[SQLValue]] = [(1, 1), (1, 41), (39, 1999), (0, 2000), (5, 6), (40, 40), (-1, 3)]
        .map { [.integer($0.0), .integer($0.1)] }

    func testPointReadMatchesTheUnpreparedStatement() throws {
        let database = try Database()
        defer { try? database.close() }
        try fill(database)
        let cases: [(String, [[SQLValue]])] = [
            ("SELECT id, a, note FROM t WHERE id = $1", single),
            ("SELECT note, note, id FROM t WHERE $1 = id", single),
            ("SELECT a FROM t WHERE id = $1", single),
            ("SELECT v, b FROM pair WHERE a = $1 AND b = $2", double),
            ("SELECT v FROM pair WHERE b = $2 AND a = $1", double),
        ]
        let session = try database.session()
        defer { session.close() }
        for (sql, bindings) in cases {
            let prepared = try session.prepare(sql)
            for _ in 0..<2 {
                for parameters in bindings {
                    let general = outcome { try session.query(sql, parameters: parameters) }
                    let cached = outcome { try session.queryPrepared(prepared, parameters: parameters) }
                    XCTAssertEqual(cached, general, "\(sql) \(parameters)")
                }
            }
            XCTAssertEqual(prepared.pointReads, bindings.count * 2 - 1, sql)
            // NULL never names a key; the general path answers it.
            let nulls = Array(repeating: SQLValue.null, count: bindings[0].count)
            XCTAssertEqual(outcome { try session.queryPrepared(prepared, parameters: nulls) },
                           outcome { try session.query(sql, parameters: nulls) }, sql)
            XCTAssertEqual(prepared.pointReads, bindings.count * 2 - 1, sql)
        }
    }

    func testOtherShapesAndLimitedSessionsTakeTheGeneralPath() throws {
        let database = try Database()
        defer { try? database.close() }
        try fill(database)
        let session = try database.session()
        defer { session.close() }
        for sql in ["SELECT a FROM t WHERE id = $1 AND a > 0", "SELECT a + 1 FROM t WHERE id = $1",
                    "SELECT v FROM pair WHERE a = $1", "SELECT a FROM t WHERE id = $1 LIMIT 1",
                    "SELECT a FROM t WHERE id >= $1 AND id <= $1"] {
            let prepared = try session.prepare(sql)
            for parameters in single {
                XCTAssertEqual(outcome { try session.queryPrepared(prepared, parameters: parameters) },
                               outcome { try session.query(sql, parameters: parameters) }, sql)
            }
            XCTAssertEqual(prepared.pointReads, 0, sql)
        }
        // A ceiling keeps the general path and its abort schedule.
        let sql = "SELECT id, a, note FROM t WHERE id = $1"
        for limit in [Int64(1_000_000), 1, 2, 3, 4, 5, 6] {
            let limited = try database.session()
            defer { limited.close() }
            limited.maxCost = limit
            let prepared = try limited.prepare(sql)
            for parameters in single {
                XCTAssertEqual(outcome { try limited.queryPrepared(prepared, parameters: parameters) },
                               outcome { try limited.query(sql, parameters: parameters) }, "limit \(limit)")
            }
            XCTAssertEqual(prepared.pointReads, 0)
        }
    }

    func testPointReadHonoursMemoryBudgetAndTransactions() throws {
        let database = try Database()
        defer { try? database.close() }
        try fill(database)
        let sql = "SELECT id, a, note FROM t WHERE id = $1"
        for budget in [Int64(1), 64, 4096, 1 << 20] {
            let session = try database.session()
            defer { session.close() }
            session.maxQueryMemoryBytes = budget
            let prepared = try session.prepare(sql)
            for _ in 0..<2 {
                for parameters in single {
                    XCTAssertEqual(outcome { try session.queryPrepared(prepared, parameters: parameters) },
                                   outcome { try session.query(sql, parameters: parameters) }, "budget \(budget)")
                }
            }
        }
        // Rows a transaction has written live on writer leaves.
        let session = try database.session()
        defer { session.close() }
        let prepared = try session.prepare(sql)
        _ = outcome { try session.queryPrepared(prepared, parameters: [.integer(7)]) }
        try session.begin()
        try session.execute("UPDATE t SET note = 'changed' WHERE id = 7")
        try session.execute("DELETE FROM t WHERE id = 50")
        try session.execute("INSERT INTO t VALUES (9000, 1, repeat('y', 30000))")
        for _ in 0..<2 {
            for parameters in single + [[.integer(9000)]] {
                XCTAssertEqual(outcome { try session.queryPrepared(prepared, parameters: parameters) },
                               outcome { try session.query(sql, parameters: parameters) }, "in transaction")
            }
        }
        try session.rollback()
        for parameters in single {
            XCTAssertEqual(outcome { try session.queryPrepared(prepared, parameters: parameters) },
                           outcome { try session.query(sql, parameters: parameters) }, "after rollback")
        }
        XCTAssertGreaterThan(prepared.pointReads, 0)
    }
}
