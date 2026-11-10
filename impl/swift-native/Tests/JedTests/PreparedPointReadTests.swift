import XCTest
@testable import Jed

/// The cached single-row key read replaces the general prepared path for one
/// statement shape. Nothing about it may be observable: the same rows, the
/// same cost, the same errors with the same text (a ceiling's abort names the
/// accrued cost) and the same session lifetime cost as the statement run
/// unprepared.
final class PreparedPointReadTests: XCTestCase {
    private typealias Case = (sql: String, bindings: [[SQLValue]])

    private func fill(_ database: Database) throws {
        try database.execute("CREATE TABLE t(id i64 PRIMARY KEY, a i32, note text)")
        try database.execute("INSERT INTO t SELECT g, (g * 3)::i32, CASE WHEN g % 500 = 0 THEN " +
            "(SELECT string_agg(md5((g * 1000 + i)::text), '') FROM generate_series(1, 400) s2(i)) " +
            "WHEN g % 50 = 0 THEN repeat('x', 40000) WHEN g % 7 = 0 THEN NULL ELSE 'n' || g::text END " +
            "FROM generate_series(1, 3000) s(g)")
        try database.execute("CREATE TABLE pair(a i32, b i64, v i16, PRIMARY KEY (a, b))")
        try database.execute("INSERT INTO pair SELECT g % 40, g, (g % 300)::i16 FROM generate_series(1, 2000) s(g)")
        try database.execute("CREATE TABLE u(id uuid PRIMARY KEY, v i32, note text)")
        try database.execute("INSERT INTO u SELECT md5(g::text)::uuid, g, CASE WHEN g % 9 = 0 THEN repeat('u', 30000) " +
            "ELSE 'u' || g::text END FROM generate_series(1, 1500) s(g)")
        try database.execute("CREATE TABLE tenant(org i32, id uuid, v i64, PRIMARY KEY (org, id))")
        try database.execute("INSERT INTO tenant SELECT g % 5, md5(g::text)::uuid, g FROM generate_series(1, 1500) s(g)")
    }
    private let single: [[SQLValue]] = [1, 7, 50, 500, 1500, 2999, 3000, 3001, -5, 0].map { [.integer($0)] }
    private let double: [[SQLValue]] = [(1, 1), (1, 41), (39, 1999), (0, 2000), (5, 6), (40, 40), (-1, 3)]
        .map { [.integer($0.0), .integer($0.1)] }
    private func cases(_ database: Database) throws -> [Case] {
        // Stored UUIDs (one with a large value beside it) and one absent.
        var ids = try database.query("SELECT id FROM u WHERE v IN (1, 2, 9, 700, 1500) ORDER BY v").collect().map { $0[0] }
        ids.append(.uuid(try SQLUUID(bytes: Array(repeating: 0xAB, count: 16))))
        let text = SQLValue.text(try SQLText("c4ca4238-a0b9-2382-0dcc-509a6f75849b"))
        let orgs: [[SQLValue]] = try database.query("SELECT org, id FROM tenant WHERE v IN (3, 4, 1499) ORDER BY v").collect()
            + [[.integer(9), ids[0]], [.integer(1), ids.last!]]
        return [
            ("SELECT id, a, note FROM t WHERE id = $1", single),
            ("SELECT note, note, id FROM t WHERE $1 = id", single),
            ("SELECT a FROM t WHERE id = $1", single),
            ("SELECT v, b FROM pair WHERE a = $1 AND b = $2", double),
            ("SELECT v FROM pair WHERE b = $2 AND a = $1", double),
            ("SELECT v, note, id FROM u WHERE id = $1", ids.map { [$0] } + [[text]]),
            ("SELECT v FROM u WHERE $1 = id", ids.map { [$0] }),
            ("SELECT v, id FROM tenant WHERE org = $1 AND id = $2", orgs),
        ]
    }

    /// Every statement's outcome on one fresh session, in order. A prepared
    /// run executes the list twice so each binding also meets a warm cache.
    private func transcript(_ database: Database, _ cases: [Case], prepared: Bool, passes: Int = 2,
                            configure: (Session) -> Void = { _ in }) throws -> (lines: [String], pointReads: Int) {
        let session = try database.session()
        defer { session.close() }
        configure(session)
        var lines: [String] = [], pointReads = 0
        for item in cases {
            let statement = try session.prepare(item.sql)
            for _ in 0..<passes {
                for parameters in item.bindings {
                    var line: String
                    do {
                        let rows = prepared ? try session.queryPrepared(statement, parameters: parameters)
                                            : try session.query(item.sql, parameters: parameters)
                        var cells: [String] = []
                        while let row = try rows.next() { cells.append(String(describing: try SpillRowCodec.encode(row))) }
                        line = cells.joined(separator: "|") + " cost \(rows.cost)"
                    } catch let error as JedError { line = "error \(error.code) \(error.message)" }
                    lines.append("\(item.sql) \(line) lifetime \(session.lifetimeCost)")
                }
            }
            pointReads += statement.pointReads
        }
        return (lines, pointReads)
    }
    private func assertSame(_ database: Database, _ cases: [Case], _ label: String, expectPointReads: Bool? = true,
                            configure: (Session) -> Void = { _ in }) throws {
        let general = try transcript(database, cases, prepared: false, configure: configure)
        let cached = try transcript(database, cases, prepared: true, configure: configure)
        XCTAssertEqual(cached.lines.count, general.lines.count, label)
        for (a, b) in zip(cached.lines, general.lines) where a != b { XCTFail("\(label)\n cached  \(a)\n general \(b)"); break }
        XCTAssertEqual(general.pointReads, 0, label)
        if expectPointReads == true { XCTAssertGreaterThan(cached.pointReads, 0, label) }
        if expectPointReads == false { XCTAssertEqual(cached.pointReads, 0, label) }
    }

    func testPointReadMatchesTheUnpreparedStatement() throws {
        let database = try Database()
        defer { try? database.close() }
        try fill(database)
        let cases = try cases(database)
        try assertSame(database, cases, "unlimited")
        // All but the first execution of every statement used the cached read.
        let cached = try transcript(database, cases, prepared: true)
        XCTAssertEqual(cached.pointReads, cases.reduce(0) { $0 + $1.bindings.count * 2 - 1 })
        // NULL never names a key; the general path answers it.
        let nulls: [Case] = cases.map { ($0.sql, $0.bindings + [Array(repeating: SQLValue.null, count: $0.bindings[0].count)]) }
        try assertSame(database, nulls, "with NULL")
    }

    func testPointReadKeepsCostCeilingSchedules() throws {
        let database = try Database()
        defer { try? database.close() }
        try fill(database)
        let cases = try cases(database)
        // Every abort point of a one-row read, then ceilings never reached.
        for limit in Array(Int64(1)...14) + [40, 1_000_000] {
            try assertSame(database, cases, "max_cost \(limit)") { $0.maxCost = limit }
        }
        // A lifetime budget runs out part-way through the list.
        for budget in [Int64(1), 5, 37, 150, 400, 1_000, 5_000, 10_000_000] {
            try assertSame(database, cases, "lifetime \(budget)", expectPointReads: budget > 5_000 ? true : nil) { $0.lifetimeMaxCost = budget }
        }
        try assertSame(database, cases, "both ceilings") { $0.maxCost = 9; $0.lifetimeMaxCost = 300 }
        for budget in [Int64(1), 64, 300, 4096, 1 << 20] {
            try assertSame(database, cases, "memory \(budget)", expectPointReads: nil) { $0.maxQueryMemoryBytes = budget }
            try assertSame(database, cases, "memory \(budget) with ceiling", expectPointReads: nil) { $0.maxQueryMemoryBytes = budget; $0.maxCost = 500 }
        }
    }

    func testPointReadChecksPrivilegesFirst() throws {
        let database = try Database()
        defer { try? database.close() }
        try fill(database)
        let cases = try cases(database)
        try assertSame(database, cases, "select on everything but DDL") { $0.allowDDL = false }
        try assertSame(database, cases, "granted t and u only") {
            $0.defaultPrivileges = []
            $0.grant(.select, on: try! SQLText("t")); $0.grant(.select, on: try! SQLText("u"))
        }
        try assertSame(database, cases, "nothing granted", expectPointReads: false) { $0.defaultPrivileges = [] }
        // A grant withdrawn after the plan was cached is refused, not served.
        let session = try database.session()
        defer { session.close() }
        let statement = try session.prepare("SELECT a FROM t WHERE id = $1")
        for _ in 0..<3 { _ = try session.queryPrepared(statement, parameters: [.integer(1)]).collect() }
        XCTAssertEqual(statement.pointReads, 2)
        session.revoke(.select, on: try SQLText("t"))
        XCTAssertThrowsError(try session.queryPrepared(statement, parameters: [.integer(1)])) {
            XCTAssertEqual(($0 as? JedError)?.code, "42501")
        }
    }

    func testOtherShapesTakeTheGeneralPath() throws {
        let database = try Database()
        defer { try? database.close() }
        try fill(database)
        let other: [Case] = ["SELECT a FROM t WHERE id = $1 AND a > 0", "SELECT a + 1 FROM t WHERE id = $1",
                             "SELECT v FROM pair WHERE a = $1", "SELECT a FROM t WHERE id = $1 LIMIT 1",
                             "SELECT a FROM t WHERE id >= $1 AND id <= $1"].map { ($0, single) }
        try assertSame(database, other, "other shapes", expectPointReads: false)
        try assertSame(database, other, "other shapes, limited", expectPointReads: false) { $0.maxCost = 20 }
    }

    func testPointReadSeesItsTransaction() throws {
        let database = try Database()
        defer { try? database.close() }
        try fill(database)
        let sql = "SELECT id, a, note FROM t WHERE id = $1"
        for limit in [Int64(0), 6, 1_000_000] {
            let session = try database.session()
            defer { session.close() }
            session.maxCost = limit
            let prepared = try session.prepare(sql)
            func same(_ label: String, _ bindings: [[SQLValue]]) {
                for parameters in bindings {
                    func run(_ body: () throws -> Rows) -> String {
                        do {
                            let rows = try body()
                            return "\(try rows.collect().map { String(describing: try SpillRowCodec.encode($0)) }) cost \(rows.cost)"
                        } catch let error as JedError { return "error \(error.code) \(error.message)" }
                        catch { return "\(error)" }
                    }
                    let cached = run { try session.queryPrepared(prepared, parameters: parameters) }
                    XCTAssertEqual(cached, run { try session.query(sql, parameters: parameters) }, "\(label) limit \(limit)")
                }
            }
            same("before", single)
            if limit == 6 { continue }
            // Rows a transaction has written live on writer leaves.
            try session.begin()
            try session.execute("UPDATE t SET note = 'changed' WHERE id = 7")
            try session.execute("DELETE FROM t WHERE id = 50")
            try session.execute("INSERT INTO t VALUES (9000, 1, repeat('y', 30000))")
            for _ in 0..<2 { same("in transaction", single + [[.integer(9000)]]) }
            try session.rollback()
            same("after rollback", single)
            XCTAssertGreaterThan(prepared.pointReads, 0)
        }
    }
}
