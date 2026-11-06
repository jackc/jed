import XCTest
@testable import Jed

final class PreparedSelectBoundaryTests: XCTestCase {
    private func integers(_ rows: Rows) throws -> ([[Int64?]], Int64) {
        var result: [[Int64?]] = []
        while let row = try rows.next() {
            result.append(row.map {
                switch $0 { case .integer(let value): return value; case .null: return nil
                default: XCTFail("expected integer result"); return nil }
            })
        }
        return (result, rows.cost)
    }
    func testPointLookupReusesPlanAndNullOverridePreservesGenericEntry() throws {
        let database = try Database(); defer { try? database.close() }
        try database.execute("CREATE TABLE t(id i32 PRIMARY KEY,v i32)")
        try database.execute("INSERT INTO t VALUES(1,10),(2,20),(3,30)")
        let statement = try database.prepare("SELECT v FROM t WHERE id=$1")
        XCTAssertEqual(try integers(database.queryPrepared(statement, parameters: [.integer(1)])).0, [[10]])
        let cached = try XCTUnwrap(statement.selectCache)
        XCTAssertFalse(cached.sensitive); XCTAssertEqual(statement.probePlans, 1); XCTAssertEqual(statement.customPlans, 0)
        XCTAssertEqual(try integers(database.queryPrepared(statement, parameters: [.integer(2)])).0, [[20]])
        XCTAssertEqual(statement.probePlans, 1); XCTAssertEqual(statement.selectCacheHits, 1)
        XCTAssertTrue(try integers(database.queryPrepared(statement, parameters: [.null])).0.isEmpty)
        XCTAssertEqual(statement.customPlans, 1); XCTAssertTrue(statement.selectCache === cached)
        XCTAssertEqual(try integers(database.queryPrepared(statement, parameters: [.integer(3)])).0, [[30]])
        XCTAssertEqual(statement.selectCacheHits, 2); XCTAssertTrue(statement.selectCache === cached)
    }

    func testSignatureTracksOnlyRelevantRowsAndAnalyzeAndRollbackRestoresHit() throws {
        let database = try Database(); defer { try? database.close() }
        try database.execute("CREATE TABLE t(id i32 PRIMARY KEY,v i32)")
        try database.execute("CREATE TABLE other(id i32 PRIMARY KEY)")
        try database.execute("INSERT INTO t VALUES(1,10)")
        let statement = try database.prepare("SELECT v FROM t WHERE id=$1")
        _ = try integers(database.queryPrepared(statement, parameters: [.integer(1)]))
        let cached = try XCTUnwrap(statement.selectCache)
        try database.execute("INSERT INTO other VALUES(1)")
        _ = try integers(database.queryPrepared(statement, parameters: [.integer(1)]))
        XCTAssertTrue(statement.selectCache === cached); XCTAssertEqual(statement.probePlans, 1)
        let writer = try database.writeSession(); defer { writer.close() }
        try writer.execute("UPDATE t SET v=11 WHERE id=1")
        XCTAssertEqual(try integers(writer.queryPrepared(statement, parameters: [.integer(1)])).0, [[11]])
        XCTAssertTrue(statement.selectCache === cached)
        try writer.rollback()
        XCTAssertEqual(try integers(database.queryPrepared(statement, parameters: [.integer(1)])).0, [[10]])
        XCTAssertTrue(statement.selectCache === cached)
        let misses = statement.selectCacheMisses
        try database.execute("ANALYZE t")
        _ = try integers(database.queryPrepared(statement, parameters: [.integer(1)]))
        XCTAssertEqual(statement.selectCacheMisses, misses + 1); XCTAssertFalse(statement.selectCache === cached)
    }

    func testSensitiveMemoMatchesFreshCostsAndEvictsOldestAfterEightBindings() throws {
        let database = try Database(); defer { try? database.close() }
        try database.execute("CREATE TABLE t(id i32 PRIMARY KEY,v i32)")
        try database.execute("INSERT INTO t SELECT i,CASE WHEN i<=180 THEN 1 ELSE i END FROM generate_series(1,200) AS g(i)")
        try database.execute("CREATE INDEX tv ON t(v)")
        try database.execute("ANALYZE t")
        let statement = try database.prepare("SELECT id FROM t WHERE v=$1 ORDER BY id")
        for value: Int64 in [1,199,1] {
            let prepared = try integers(database.queryPrepared(statement, parameters: [.integer(value)]))
            let fresh = try integers(database.query("SELECT id FROM t WHERE v=\(value) ORDER BY id"))
            XCTAssertEqual(prepared.0, fresh.0); XCTAssertEqual(prepared.1, fresh.1)
        }
        let cache = try XCTUnwrap(statement.selectCache)
        XCTAssertTrue(cache.sensitive); XCTAssertEqual(statement.probePlans, 1)
        XCTAssertEqual(statement.customPlans, 2); XCTAssertEqual(statement.selectCacheHits, 1)
        for value: Int64 in 190...200 { _ = try integers(database.queryPrepared(statement, parameters: [.integer(value)])) }
        XCTAssertEqual(cache.memo.count, 8)
        let before = statement.customPlans
        _ = try integers(database.queryPrepared(statement, parameters: [.integer(1)]))
        XCTAssertEqual(statement.customPlans, before + 1)
    }

    func testDetachedJoinAndCompoundPlansMatchFreshExecution() throws {
        let database = try Database(); defer { try? database.close() }
        for table in ["a", "b", "c"] {
            try database.execute("CREATE TABLE \(table)(id i32 PRIMARY KEY,k i32)")
            try database.execute("INSERT INTO \(table) VALUES(1,1),(2,1),(3,2)")
        }
        for sql in [
            "SELECT a.id,b.id FROM a JOIN b ON a.k=b.k WHERE a.id=$1 ORDER BY a.id,b.id",
            "SELECT a.id,b.id,c.id FROM a JOIN b ON a.k=b.k JOIN c ON b.id=c.id WHERE a.id=$1 ORDER BY a.id,b.id,c.id",
            "SELECT id FROM a WHERE id=$1 UNION ALL SELECT id FROM b WHERE id=$1 ORDER BY id"
        ] {
            let statement = try database.prepare(sql)
            for value: Int64 in [1,2] {
                let prepared = try integers(database.queryPrepared(statement, parameters: [.integer(value)]))
                let fresh = try integers(database.query(sql, parameters: [.integer(value)]))
                XCTAssertEqual(prepared.0, fresh.0); XCTAssertEqual(prepared.1, fresh.1)
            }
            XCTAssertNotNil(statement.selectCache); XCTAssertEqual(statement.probePlans, 1)
            XCTAssertEqual(statement.selectCacheHits, 1)
        }
    }

    func testDatabaseIdentityTempShadowAndNoncacheableBodies() throws {
        let database = try Database(), other = try Database()
        defer { try? database.close(); try? other.close() }
        let local = try database.session(); defer { local.close() }
        try local.execute("CREATE TEMP TABLE t(id i32)")
        try local.execute("INSERT INTO t VALUES(9)")
        try database.execute("CREATE TABLE t(id i32)")
        try database.execute("INSERT INTO t VALUES(1)")
        let statement = try database.prepare("SELECT id FROM t")
        XCTAssertEqual(try integers(database.queryPrepared(statement)).0, [[1]])
        let cached = try XCTUnwrap(statement.selectCache)
        XCTAssertEqual(try integers(local.queryPrepared(statement)).0, [[9]])
        XCTAssertTrue(statement.selectCache === cached)
        let constant = try database.prepare("SELECT 1")
        _ = try integers(database.queryPrepared(constant)); _ = try integers(other.queryPrepared(constant))
        XCTAssertEqual(constant.probePlans, 2); XCTAssertEqual(constant.selectCacheHits, 0)
        for sql in ["SELECT (SELECT 1)", "SELECT * FROM (SELECT 1) d", "WITH x AS (SELECT 1) SELECT * FROM x"] {
            let statement = try database.prepare(sql)
            _ = try integers(database.queryPrepared(statement)); _ = try integers(database.queryPrepared(statement))
            XCTAssertNil(statement.selectCache)
        }
    }

    func testDynamicTimezoneIndexExcludesBothCacheSlotsEvenWhenReadDoesNotUseIt() throws {
        let database = try Database(); defer { try? database.close() }
        try database.execute("CREATE TABLE t(id i32 PRIMARY KEY, ts timestamptz, zone text)")
        try database.execute("CREATE INDEX tz ON t(timezone(zone,ts))")
        let insert = try database.prepare("INSERT INTO t VALUES($1,TIMESTAMPTZ '2024-01-01 00:00:00+00','UTC')")
        let select = try database.prepare("SELECT id FROM t WHERE id=$1")
        for value: Int64 in [1,2] {
            try database.executePrepared(insert, parameters: [.integer(value)])
            _ = try database.queryPrepared(select, parameters: [.integer(value)]).collect()
        }
        XCTAssertNil(insert.insertCache)
        XCTAssertNil(select.selectCache)
        XCTAssertEqual(insert.insertCacheHits, 0)
        XCTAssertEqual(select.selectCacheHits, 0)
    }

    func testExactMemoKeysAcceptLargeBindingsAndPreserveRepresentations() throws {
        let database = try Database(); defer { try? database.close() }
        let statement = try database.prepare("SELECT 1")
        _ = try database.executePrepared(statement)
        let signature = try XCTUnwrap(statement.selectCache).signature
        let cache = PreparedSelectCache(signature: signature, parameters: [TypeDescriptor(.scalar(.text))],
            sensitive: true, nullRead: false, generic: nil)
        let large = SQLValue.text(try SQLText(String(repeating: "x", count: 70_000)))
        let key = try XCTUnwrap(cache.key([large]))
        XCTAssertEqual(key, cache.key([large]))
        XCTAssertNotEqual(key, cache.key([.text(try SQLText(String(repeating: "x", count: 69_999)))]))
        // The entry's resolved types are fixed. Comparisons here exercise its
        // representation key separately from host binding to those types.
        XCTAssertNotEqual(cache.key([.f64(0)]), cache.key([.f64(0x8000_0000_0000_0000)]))
        XCTAssertNotEqual(cache.key([.decimal(try SQLDecimal.parse("1.0"))]),
            cache.key([.decimal(try SQLDecimal.parse("1.00"))]))
        XCTAssertNotEqual(cache.key([.array(try SQLArray(dimensions: [1], lowerBounds: [1], elements: [.integer(1)]))]),
            cache.key([.array(try SQLArray(dimensions: [1], lowerBounds: [-1], elements: [.integer(1)]))]))
    }
}
