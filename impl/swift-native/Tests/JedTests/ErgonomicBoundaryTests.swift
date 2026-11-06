import Foundation
import XCTest
@testable import Jed

private struct HostIdentifier: SQLValueConvertible, SQLValueDecodable {
    let value: Int32
    func sqlValue() -> SQLValue { .integer(Int64(value)) }
    static func fromSQLValue(_ value: SQLValue) throws -> Self { Self(value: try Int32.fromSQLValue(value)) }
}

final class ErgonomicBoundaryTests: XCTestCase {
    private func code<T>(_ expected: String, _ operation: @autoclosure () throws -> T) {
        XCTAssertThrowsError(try operation()) { XCTAssertEqual(($0 as? JedError)?.code, expected) }
    }
    func testNativeArgumentsPreparedQueriesAndTypedRowsUseExecutingHandle() throws {
        let database = try Database(); defer { try? database.close() }
        let uuid = UUID(uuidString: "00112233-4455-6677-8899-aabbccddeeff")!
        let amount = try SQLDecimal.parse("123.4500"), bytes = Data([0, 255, 1])
        try database.execute("CREATE TABLE t(id i32 PRIMARY KEY,label text,amount decimal,bytes bytea,u uuid,flags i32[])")
        let insert = try database.prepare("INSERT INTO t VALUES($1,$2,$3,$4,$5,$6)")
        let statement = try database.prepare("SELECT * FROM t WHERE id=$1")
        let handle = try database.session(); defer { handle.close() }
        try handle.update { session in
            try session.executePrepared(insert, arguments: [HostIdentifier(value: 7), "e\u{301}", amount, bytes, uuid, [Int32(1), 2]])
            let row = try XCTUnwrap(session.queryRowPrepared(statement, arguments: [7]))
            XCTAssertEqual(try row.get("id", as: HostIdentifier.self).value, 7)
            XCTAssertEqual(try row.get("label", as: SQLText.self).utf8, Array("e\u{301}".utf8))
            XCTAssertEqual(try row.get(2, as: SQLDecimal.self).scale, 4)
            XCTAssertEqual(try row.get("bytes", as: Data.self), bytes)
            XCTAssertEqual(try row.get("u", as: UUID.self), uuid)
            XCTAssertEqual(try row.get("flags", as: [Int32].self), [1, 2])
        }
        let queryable: any SQLQuerying = database
        XCTAssertEqual(try queryable.queryRows("SELECT id FROM t").first?.get(0, as: Int64.self), 7)
        XCTAssertNil(try database.queryRowPrepared(statement, arguments: [8]))
        XCTAssertEqual(database.core.livePinCount, 0)
    }

    func testNullNarrowingExactNamesAndConversionFailures() throws {
        let database = try Database(); defer { try? database.close() }
        let row = try XCTUnwrap(database.queryRow("SELECT 5000000000::i64 AS big,NULL::text AS label,1 AS \"é\""))
        XCTAssertNil(try row.get("label", as: String?.self))
        code("22004", try row.get("label", as: String.self))
        code("22003", try row.get("big", as: Int32.self))
        code("42804", try row.get("big", as: String.self))
        code("42703", try row.get(-1, as: Int.self))
        code("42703", try row.get("é", as: Int.self))
        XCTAssertEqual(try row.get("e\u{301}", as: Int.self), 1)
        XCTAssertEqual(try UInt64.fromSQLValue(.integer(Int64.max)), UInt64(Int64.max))
        code("22003", try UInt8.fromSQLValue(.integer(-1)))
        code("22003", try UInt64.max.sqlValue())
        code("22021", try "bad\0text".sqlValue())
    }

    func testEmptyArraysBytesAndExactRepresentations() throws {
        let database = try Database(); defer { try? database.close() }
        let row = try XCTUnwrap(database.queryRow("SELECT $1::i32[] AS a,$2::bytea AS b,$3::text AS n,$4::f64 AS f",
            arguments: [[Int32](), [UInt8](), Optional<String>.none, Double(bitPattern: 0x8000_0000_0000_0000)]))
        XCTAssertEqual(try row.get("a", as: [Int32].self), [])
        XCTAssertEqual(try row.get("b", as: [UInt8].self), [])
        XCTAssertNil(try row.get("n", as: String?.self))
        XCTAssertEqual(try row.get("f", as: Double.self).bitPattern, 0x8000_0000_0000_0000)
        let shaped = try SQLArray(dimensions: [2], lowerBounds: [-4], elements: [.integer(1), .null])
        let raw = try XCTUnwrap(database.queryRow("SELECT $1::i32[] AS a", arguments: [shaped]))
        XCTAssertEqual(try raw.get("a", as: SQLArray.self).lowerBounds, [-4])
        XCTAssertEqual(try raw.get("a", as: [Int32?].self), [1, nil])
    }

    func testArgumentFailurePoisonsBlockWhileHostDecodingDoesNot() throws {
        let database = try Database(), session = try database.session()
        defer { session.close(); try? database.close() }
        try session.begin()
        code("22003", try session.query("SELECT $1::i64", arguments: [UInt64.max]))
        XCTAssertEqual(session.status, .failed)
        try session.rollback(); try session.begin()
        let row = try XCTUnwrap(session.queryRow("SELECT 1"))
        code("42804", try row.get(0, as: String.self))
        XCTAssertEqual(session.status, .open)
        try session.rollback()
        let cancelled = CancellationToken(); cancelled.cancel()
        code("57014", try database.queryRow("SELECT $1::i32", arguments: [1], cancellation: cancelled))
    }

    func testTypedCollectionUsesQ1AndShortIterationClosesPins() throws {
        let database = try Database(), session = try database.session()
        defer { session.close(); try? database.close() }
        try session.execute("CREATE TABLE t(id i32 PRIMARY KEY)")
        try session.execute("INSERT INTO t SELECT i FROM generate_series(1,100) g(i)")
        session.maxQueryMemoryBytes = 512
        code("54P05", try session.queryRows("SELECT id FROM t ORDER BY id"))
        XCTAssertEqual(database.core.livePinCount, 0)
        let rows = try session.query("SELECT id FROM t ORDER BY id")
        var seen: [Int32] = []
        try rows.forEachRow { row in seen.append(try row.get(0)); return seen.count < 2 }
        XCTAssertEqual(seen, [1, 2]); XCTAssertTrue(rows.isClosed)
        XCTAssertEqual(database.core.livePinCount, 0)
        XCTAssertEqual(try session.queryMap("VALUES(1),(2)") { try $0.get(0, as: Int.self) }, [1, 2])
        XCTAssertEqual(try session.queryRow("SELECT id FROM t ORDER BY id")?.get(0, as: Int.self), 1)
        XCTAssertEqual(database.core.livePinCount, 0)
    }

    func testDatabaseScopedTransactionsShareNativeHelpersAndRollback() throws {
        enum Stop: Error { case requested }
        let database = try Database(); defer { try? database.close() }
        try database.execute("CREATE TABLE t(id i32)")
        XCTAssertThrowsError(try database.update { session in
            try session.execute("INSERT INTO t VALUES($1)", arguments: [1])
            throw Stop.requested
        })
        XCTAssertEqual(try database.queryRow("SELECT count(*) FROM t")?.get(0, as: Int64.self), 0)
        _ = try database.update { try $0.execute("INSERT INTO t VALUES($1)", arguments: [2]) }
        let value = try database.view { session in try session.queryRow("SELECT id FROM t")?.get(0, as: Int.self) }
        XCTAssertEqual(value, 2)
    }
}
