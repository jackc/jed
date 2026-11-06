import Foundation
import XCTest
@testable import Jed

final class CollationMigrationBoundaryTests: XCTestCase {
    private func fixture(_ name: String, _ ext: String = "jed") throws -> [UInt8] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: ext, subdirectory: "Fixtures"))
        return Array(try Data(contentsOf: url))
    }
    private func environment() throws -> Environment {
        var builder = Environment.builder(); try builder.loadUnicodeData(fixture("unicode", "jucd")); return builder.build()
    }
    private func fails(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }
    private func count(_ session: Session, _ sql: String) throws -> Int64 {
        let rows = try session.query(sql); defer { rows.close() }
        guard let row = try rows.next(), case .integer(let count) = row[0] else { throw JedError.corruption("expected count") }
        return count
    }
    func testHostDefaultLifecycleNoopsAndPrivateBlockActions() throws {
        let environment = try environment(), database = try Database(environment: environment)
        defer { try? database.close() }
        let unicode = try SQLText("unicode"), byteOrder = try SQLText("C")
        XCTAssertEqual(environment.loadedCollations().map(\.name), environment.loadedCollations().map(\.name).sorted())
        XCTAssertTrue(environment.loadedCollations().allSatisfy { !$0.isDefault && $0.verdict == .full })
        let session = try database.session(); defer { session.close() }
        let start = database.version
        try session.setDefaultCollation(byteOrder)
        XCTAssertEqual(try session.upgradeCollations(), 0); XCTAssertEqual(database.version, start)
        fails("42704") { try session.setDefaultCollation(SQLText("absent")) }
        XCTAssertEqual(database.version, start)
        try session.begin()
        try session.execute("CREATE TEMP TABLE private_rows (id i32) ON COMMIT DELETE ROWS")
        try session.execute("INSERT INTO private_rows VALUES (1)")
        try session.setDefaultCollation(unicode)
        try session.setDefaultCollation(unicode)
        XCTAssertEqual(try session.defaultCollation(), unicode)
        XCTAssertEqual(try database.defaultCollation(), byteOrder)
        XCTAssertEqual(try count(session, "SELECT count(*) FROM private_rows"), 1)
        fails("42704") { try session.setDefaultCollation(SQLText("absent")) }
        XCTAssertEqual(session.status, .open)
        try session.commit()
        XCTAssertEqual(try database.defaultCollation(), unicode)
        XCTAssertEqual(try count(session, "SELECT count(*) FROM private_rows"), 0)
        let committed = database.version
        try session.setDefaultCollation(unicode)
        XCTAssertEqual(database.version, committed)
        let info = try XCTUnwrap(session.collations().first)
        XCTAssertTrue(info.isDefault); XCTAssertEqual(info.verdict, .full)
        let reader = try database.readSession(); defer { reader.close() }
        fails("25006") { try reader.setDefaultCollation(byteOrder) }
        fails("25006") { _ = try reader.upgradeCollations() }
        try session.begin(); try session.setDefaultCollation(byteOrder); try session.rollback()
        XCTAssertEqual(try session.defaultCollation(), unicode)
    }
    func testSkewedFixtureMigrationRepinsAndPreservesPinnedSnapshot() throws {
        let environment = try environment()
        let database = try Database.deserialize(fixture("collation_skew_corrupt"), options: DeserializeOptions(environment: environment))
        defer { try? database.close() }
        let reader = try database.readSession(); defer { reader.close() }
        let before = database.core.pinLatest(); defer { before.close() }
        let oldTable = try XCTUnwrap(before.snapshot.catalog.tables.first), oldStore = try XCTUnwrap(before.snapshot.tables[oldTable.name])
        XCTAssertEqual(try reader.collations().first?.verdict, .skewed)
        let version = database.version
        XCTAssertEqual(try database.upgradeCollations(), 1)
        XCTAssertEqual(database.version, version + 1)
        XCTAssertEqual(try database.collations().first?.verdict, .full)
        XCTAssertEqual(try reader.collations().first?.verdict, .skewed)
        let after = database.core.pinLatest(); defer { after.close() }
        let newTable = try XCTUnwrap(after.snapshot.catalog.tables.first)
        XCTAssertTrue(oldStore.revision === after.snapshot.tables[oldTable.name]?.revision)
        XCTAssertFalse(before.snapshot.indexes[oldTable.indexes[0].name]?.revision === after.snapshot.indexes[oldTable.indexes[0].name]?.revision)
        try CatalogEnvironment.checkWritable(newTable, image: after.snapshot.catalog, environment: environment)
        fails("XX002") { try CatalogEnvironment.checkWritable(oldTable, image: before.snapshot.catalog, environment: environment) }
        XCTAssertEqual(try database.upgradeCollations(), 0); XCTAssertEqual(database.version, version + 1)
        let reopened = try Database.deserialize(database.serialize(), options: DeserializeOptions(environment: environment))
        defer { try? reopened.close() }
        XCTAssertEqual(try reopened.collations().first?.verdict, .full)
    }
    func testUnsupportedIndexMigrationIsAtomicAndDoesNotAbortBlock() throws {
        let database = try Database(environment: environment()), session = try database.session()
        defer { session.close(); try? database.close() }
        try session.execute("CREATE TABLE a (id text COLLATE \"unicode\" PRIMARY KEY, n i32)")
        try session.execute("INSERT INTO a VALUES ('a', 1), ('Z', 2)")
        try session.execute("CREATE TABLE z (id text COLLATE \"unicode\" PRIMARY KEY, n i32)")
        try session.execute("INSERT INTO z VALUES ('b', 3)")
        try session.execute("CREATE INDEX z_partial ON z (n) WHERE n > 0")
        try session.withHostCatalogWrite { snapshot in
            var image = snapshot.catalog
            for index in image.collations.indices { image.collations[index].unicode = try SQLText("9999.0.0") }
            try snapshot.updateCatalog(image, schemaChanged: true)
        }
        let old = database.core.pinLatest(); defer { old.close() }
        let version = database.version
        try session.begin()
        fails("0A000") { _ = try session.upgradeCollations() }
        XCTAssertEqual(session.status, .open)
        try session.withHostCatalogRead { snapshot in
            for (name, store) in old.snapshot.tables { XCTAssertTrue(snapshot.tables[name]?.revision === store.revision) }
            XCTAssertEqual(snapshot.catalog.collations.first?.unicode.description, "9999.0.0")
        }
        try session.commit()
        XCTAssertEqual(database.version, version)
    }
    func testSortedGINBuilderUsesEverySharedPackingVector() throws {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        let snapshot = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let documents = try XCTUnwrap(snapshot["documents"] as? [String: [String: Any]])
        let vectors = try XCTUnwrap(documents["spec/encoding/gin_posting_runs.toml"]?["pack"] as? [[String: Any]])
        func bytes(_ hex: String) throws -> [UInt8] {
            let text = Array(hex.utf8)
            return try stride(from: 0, to: text.count, by: 2).map {
                try XCTUnwrap(UInt8(String(decoding: text[$0..<($0 + 2)], as: UTF8.self), radix: 16))
            }
        }
        for vector in vectors {
            let term = try bytes(XCTUnwrap(vector["term"] as? String)), keys = try XCTUnwrap(vector["keys"] as? [String]).map(bytes)
            var offset = 0
            let built = try GinIndex.buildSorted(codec: PlainPageCodec(pageSize: 4096)) {
                guard offset < keys.count else { return nil }; defer { offset += 1 }
                return GinPosting(term: term, storageKey: keys[offset])
            }
            var cursor = TreeCursor(built.tree), result: [[UInt8]] = []; defer { cursor.close() }
            while let record = try cursor.next() { result.append(record.key) }
            XCTAssertEqual(result, try XCTUnwrap(vector["runs"] as? [String]).map(bytes))
            XCTAssertEqual(offset, keys.count)
        }
    }
}
