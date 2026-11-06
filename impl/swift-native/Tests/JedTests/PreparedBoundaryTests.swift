import XCTest
@testable import Jed

final class PreparedBoundaryTests: XCTestCase {
    private func fails(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) {
            XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line)
        }
    }
    private func equalRows(_ actual: [[SQLValue]], _ expected: [[SQLValue]], file: StaticString = #filePath, line: UInt = #line) {
        func cell(_ value: SQLValue) -> String {
            switch value {
            case .integer(let value): return "integer:\(value)"
            case .text(let value): return "text:\(value.description)"
            case .null: return "NULL"
            default: XCTFail("unexpected value kind in prepared test", file: file, line: line); return "unexpected"
            }
        }
        XCTAssertEqual(actual.map { $0.map(cell) }, expected.map { $0.map(cell) }, file: file, line: line)
    }
    func testStandaloneParseAndTotalExecutionSeam() throws {
        let source = try Database(), session = try source.session()
        let create = try session.prepare("CREATE TABLE t(id i32 PRIMARY KEY, v text)")
        let insert = try session.prepare("INSERT INTO t VALUES ($1,$2) RETURNING id,v")
        let select = try source.prepare("SELECT v FROM t WHERE id=$1")
        let begin = try session.prepare("BEGIN"), rollback = try session.prepare("ROLLBACK")
        session.close(); try source.close()
        let database = try Database(); defer { try? database.close() }
        let executing = try database.session(); defer { executing.close() }
        XCTAssertEqual(try database.executePrepared(create).command, "CREATE TABLE")
        let returning = try database.queryPrepared(insert, parameters: [.integer(1), .text(SQLText("one"))])
        try equalRows(returning.collect(), [[.integer(1), .text(SQLText("one"))]])
        try equalRows(database.queryPrepared(select, parameters: [.integer(1)]).collect(), [[.text(SQLText("one"))]])
        fails("42601") { _ = try database.executePrepared(insert, parameters: [.integer(2)]) }
        XCTAssertEqual(try executing.executePrepared(begin).command, "BEGIN")
        try executing.executePrepared(insert, parameters: [.integer(2), .text(SQLText("two"))])
        fails("42601") { _ = try executing.prepare("SELECT (") }
        XCTAssertEqual(executing.status, .failed)
        fails("25P02") { _ = try executing.queryPrepared(select, parameters: [.integer(1)]) }
        XCTAssertEqual(try executing.executePrepared(rollback).command, "ROLLBACK")
        try equalRows(database.query("SELECT id FROM t ORDER BY id").collect(), [[.integer(1)]])
    }

    func testInsertCacheSurvivesRowsAndRollbackButRespectsWorkingSchema() throws {
        let database = try Database(); defer { try? database.close() }
        try database.execute("CREATE TABLE t(id i32 PRIMARY KEY, v i32 DEFAULT 7 CHECK(v>0))")
        let statement = try database.prepare("INSERT INTO t(id) VALUES($1) RETURNING v")
        let writer = try database.writeSession(); defer { writer.close() }
        try equalRows(writer.queryPrepared(statement, parameters: [.integer(1)]).collect(), [[.integer(7)]])
        XCTAssertNotNil(statement.insertCache)
        try equalRows(writer.queryPrepared(statement, parameters: [.integer(2)]).collect(), [[.integer(7)]])
        XCTAssertEqual(statement.insertCacheHits, 1)
        try writer.rollback()
        try equalRows(database.queryPrepared(statement, parameters: [.integer(1)]).collect(), [[.integer(7)]])
        XCTAssertEqual(statement.insertCacheHits, 2)
        try writer.begin(writable: true)
        try writer.execute("ALTER TABLE t ALTER COLUMN v SET DEFAULT 9")
        try equalRows(writer.queryPrepared(statement, parameters: [.integer(2)]).collect(), [[.integer(9)]])
        XCTAssertEqual(statement.insertCacheHits, 2)
        try writer.rollback()
        try equalRows(database.queryPrepared(statement, parameters: [.integer(2)]).collect(), [[.integer(7)]])
        XCTAssertEqual(statement.insertCacheHits, 3)
        try equalRows(database.queryPrepared(statement, parameters: [.integer(3)]).collect(), [[.integer(7)]])
        XCTAssertEqual(statement.insertCacheHits, 4)
    }

    func testInsertCacheChecksTempCreatedBeforeAnotherSessionCreatesMain() throws {
        let database = try Database(); defer { try? database.close() }
        let privateSession = try database.session(); defer { privateSession.close() }
        // A local CREATE precludes visible overlaps. Another session may then
        // create a persistent name which this existing private table shadows.
        try privateSession.execute("CREATE TEMP TABLE t(id i32 PRIMARY KEY,v i32 DEFAULT 11)")
        try database.execute("CREATE TABLE t(id i32 PRIMARY KEY,v i32 DEFAULT 7)")
        let statement = try database.prepare("INSERT INTO t(id) VALUES($1) RETURNING v")
        try equalRows(database.queryPrepared(statement, parameters: [.integer(1)]).collect(), [[.integer(7)]])
        try equalRows(privateSession.queryPrepared(statement, parameters: [.integer(1)]).collect(), [[.integer(11)]])
        XCTAssertEqual(statement.insertCacheHits, 0)
        try equalRows(database.queryPrepared(statement, parameters: [.integer(2)]).collect(), [[.integer(7)]])
        XCTAssertEqual(statement.insertCacheHits, 1)
    }

    func testInsertCacheRebuildsIndexEvaluationAndAdmission() throws {
        let database = try Database(); defer { try? database.close() }
        try database.execute("CREATE TABLE t(id i32 PRIMARY KEY, v text CHECK(length(v)>0))")
        try database.execute("CREATE UNIQUE INDEX tv ON t(lower(v)) WHERE id>0")
        let insert = try database.prepare("INSERT INTO t VALUES($1,$2) RETURNING lower(v)")
        let first = try database.queryPrepared(insert, parameters: [.integer(1), .text(SQLText("A"))])
        try equalRows(first.collect(), [[.text(SQLText("a"))]])
        let firstCost = first.cost
        let second = try database.queryPrepared(insert, parameters: [.integer(2), .text(SQLText("B"))])
        try equalRows(second.collect(), [[.text(SQLText("b"))]])
        XCTAssertEqual(second.cost, firstCost)
        XCTAssertEqual(insert.insertCacheHits, 1)
        fails("23505") { try database.executePrepared(insert, parameters: [.integer(3), .text(SQLText("a"))]) }
        fails("23514") { try database.executePrepared(insert, parameters: [.integer(3), .text(SQLText(""))]) }
        let reader = try database.readSession(); defer { reader.close() }
        fails("25006") { try reader.executePrepared(insert, parameters: [.integer(3), .text(SQLText("C"))]) }
        let cancelled = CancellationToken(); cancelled.cancel()
        fails("57014") { try database.executePrepared(insert, parameters: [.integer(3), .text(SQLText("C"))], cancellation: cancelled) }
        try equalRows(database.query("SELECT id FROM t ORDER BY id").collect(), [[.integer(1)], [.integer(2)]])
    }

    func testInsertCacheIdentityAndExclusions() throws {
        let first = try Database(), second = try Database()
        defer { try? first.close(); try? second.close() }
        try first.execute("CREATE TABLE t(id i32 PRIMARY KEY,v i32 DEFAULT 1)")
        try second.execute("CREATE TABLE t(id i32 PRIMARY KEY,v i32 DEFAULT 2)")
        let insert = try first.prepare("INSERT INTO t(id) VALUES($1) RETURNING v")
        try equalRows(first.queryPrepared(insert, parameters: [.integer(1)]).collect(), [[.integer(1)]])
        try equalRows(second.queryPrepared(insert, parameters: [.integer(1)]).collect(), [[.integer(2)]])
        XCTAssertEqual(insert.insertCacheHits, 0)
        for sql in ["INSERT INTO t VALUES(2,2) ON CONFLICT DO NOTHING", "INSERT INTO t SELECT 3,3",
                    "WITH x AS (SELECT 4) INSERT INTO t SELECT * FROM x, (VALUES(4)) v"] {
            let statement = try first.prepare(sql)
            try first.executePrepared(statement)
            XCTAssertNil(statement.insertCache)
        }
    }
}
