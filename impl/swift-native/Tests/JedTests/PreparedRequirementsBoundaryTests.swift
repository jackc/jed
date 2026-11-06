import XCTest
@testable import Jed

final class PreparedRequirementsBoundaryTests: XCTestCase {
    private func result(_ rows: Rows) throws -> ([[UInt8]], Int64) {
        let values = try rows.collect().map { try SpillRowCodec.encode($0) }
        return (values, rows.cost)
    }
    private func fails(_ code: String, _ body: () throws -> Void,
                       file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) {
            XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line)
        }
    }

    func testGenericHitKeepsConvertedCurrentParametersAndAdmissionErrors() throws {
        let database = try Database(); defer { try? database.close() }
        let sql = "SELECT $1::i16,$2::i32[]"
        let statement = try database.prepare(sql)
        let first: [SQLValue] = [.integer(7), .text(try SQLText("[0:1]={3,4}"))]
        _ = try result(database.queryPrepared(statement, parameters: first))
        let entry = try XCTUnwrap(statement.selectCache)
        let requirements = try XCTUnwrap(entry.generic).requirements
        XCTAssertFalse(requirements.mutatesSequences)
        XCTAssertFalse(requirements.needsEagerSubqueryPreparation)
        for parameters: [SQLValue] in [
            [.integer(8), .text(try SQLText("[-2:-1]={5,6}"))],
            [.null, .null]
        ] {
            let prepared = try result(database.queryPrepared(statement, parameters: parameters))
            let fresh = try result(database.query(sql, parameters: parameters))
            XCTAssertEqual(prepared.0, fresh.0)
            XCTAssertEqual(prepared.1, fresh.1)
            XCTAssertTrue(statement.selectCache === entry)
            XCTAssertTrue(try XCTUnwrap(entry.generic).requirements === requirements)
        }
        XCTAssertEqual(statement.selectCacheHits, 2)
        fails("22003") { _ = try database.queryPrepared(statement, parameters: [.integer(40_000), .null]) }
        fails("42601") { _ = try database.queryPrepared(statement, parameters: [.integer(1)]) }
        XCTAssertTrue(statement.selectCache === entry)
        // An admission failure in a transaction still poisons that transaction.
        let session = try database.session(); defer { session.close() }
        try session.begin(writable: false)
        fails("22003") { _ = try session.queryPrepared(statement, parameters: [.integer(40_000), .null]) }
        XCTAssertEqual(session.status, .failed)
        try session.rollback()
    }

    func testUncachedShadowBindsItsOwnSchemaAndMissingRelationWinsAfterInvalidation() throws {
        let database = try Database(), local = try database.session()
        defer { local.close(); try? database.close() }
        // The temporary relation must precede main creation to make a legal
        // same-name shadow with a different parameter descriptor.
        try local.execute("CREATE TEMP TABLE t(id text)")
        try local.execute("INSERT INTO t VALUES('alpha')")
        try database.execute("CREATE TABLE t(id i16 PRIMARY KEY)")
        try database.execute("INSERT INTO t VALUES(1),(2)")
        let sql = "SELECT id FROM t WHERE id=$1"
        let statement = try database.prepare(sql)
        _ = try result(database.queryPrepared(statement, parameters: [.integer(1)]))
        let entry = try XCTUnwrap(statement.selectCache)
        let requirements = try XCTUnwrap(entry.generic).requirements
        let parameters: [SQLValue] = [.text(try SQLText("alpha"))]
        let shadow = try result(local.queryPrepared(statement, parameters: parameters))
        let fresh = try result(local.query(sql, parameters: parameters))
        XCTAssertEqual(shadow.0, fresh.0); XCTAssertEqual(shadow.1, fresh.1)
        XCTAssertTrue(statement.selectCache === entry)
        XCTAssertEqual(statement.selectCacheHits, 0)
        let current = try result(database.queryPrepared(statement, parameters: [.integer(2)]))
        XCTAssertEqual(current.0, [try SpillRowCodec.encode([.integer(2)])])
        XCTAssertTrue(try XCTUnwrap(entry.generic).requirements === requirements)
        try database.execute("DROP TABLE t")
        // A miss must resolve the missing relation before admitting a value
        // which would fail against the old integer parameter schema.
        fails("42P01") { _ = try database.queryPrepared(statement, parameters: parameters) }
    }

    func testUncachedEagerSubqueryAndCachedSequenceRoutingKeepTheirBoundaries() throws {
        let database = try Database(); defer { try? database.close() }
        try database.execute("CREATE TABLE t(id i32)")
        try database.execute("INSERT INTO t VALUES(1),(2)")
        let scalar = try database.prepare("SELECT (SELECT id FROM t)")
        for _ in 0..<2 {
            // Eager cardinality failure occurs before a Rows handle is returned.
            fails("21000") { _ = try database.queryPrepared(scalar) }
            XCTAssertNil(scalar.selectCache)
        }
        try database.execute("CREATE SEQUENCE s")
        let sequence = try database.prepare("SELECT nextval($1)")
        let parameters: [SQLValue] = [.text(try SQLText("s"))]
        var requirements: PreparedSelectRequirements?
        for value: Int64 in [1, 2] {
            let rows = try result(database.queryPrepared(sequence, parameters: parameters))
            XCTAssertEqual(rows.0, [try SpillRowCodec.encode([.integer(value)])])
            let current = try XCTUnwrap(try XCTUnwrap(sequence.selectCache).generic).requirements
            XCTAssertTrue(current.mutatesSequences)
            if let requirements { XCTAssertTrue(current === requirements) }
            else { requirements = current }
        }
        XCTAssertEqual(sequence.selectCacheHits, 1)
        let version = database.version
        fails("42804") { _ = try database.queryPrepared(sequence, parameters: [.integer(3)]) }
        XCTAssertEqual(database.version, version)
    }
}
