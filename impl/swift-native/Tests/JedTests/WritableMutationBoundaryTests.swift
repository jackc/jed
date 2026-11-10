import XCTest
@testable import Jed

/// The lexical last-write-wins rule is a deliberate PG divergence; these
/// invariants exercise packed roots and secondary entries without SQL copies.
final class WritableMutationBoundaryTests: XCTestCase {
    private func fixture() throws -> (DomainSnapshot, SessionSettings, ExpressionContext) {
        let codec = try PlainPageCodec(pageSize: 8192)
        let root = PageSealer.nullReference(codec)
        let pool = try NodePool(codec: codec, cacheBytes: nil) { _ in
            throw JedError("XX000", "resident test tree must not read a device")
        }
        let index = CatalogIndex(name: try SQLText("items_value"), keys: [.column(1)], root: root)
        let table = CatalogTable(name: try SQLText("items"), columns: [
            CatalogColumn(name: try SQLText("id"), type: CatalogTypeUse(.scalar(.i32)), notNull: true),
            CatalogColumn(name: try SQLText("value"), type: CatalogTypeUse(.scalar(.i32)))
        ], primaryKey: [0], indexes: [index], root: root, rowCount: 1)
        var image = CatalogImage(); image.tables = [table]
        var snapshot = try DomainSnapshot(catalogID: 29, txid: 0, catalog: image, pool: pool,
            tables: [:], indexes: [:], gistTrees: [:])
        let types = try table.columns.map { try $0.type.resolve(snapshot.types) }
        var store = snapshot.emptyStore(types: types), tree = snapshot.emptyStore(types: [])
        let key = try KeyEncoding.encode(.integer(1), type: types[0])
        let record = try RecordDisposition.plan(key: key, values: [.value(.integer(1)), .value(.integer(7))], types: types, codec: codec)
        try store.put(record, netStaging: false)
        let prefix = try KeyEncoding.encodeSlot(.integer(7), type: types[1])
        try tree.put(PackedRecord(key: prefix + key, cells: []), netStaging: false)
        snapshot.setTableStore(table.name, store); snapshot.setIndexStore(index.name, tree)
        let settings = SessionSettings(environment: Environment.builder().build(), options: SessionOptions())
        return (snapshot, settings, ExpressionContext(meter: CostMeter()))
    }

    func testRepeatedPinnedMutationRemovesTheWorkingSecondaryEntry() throws {
        var (working, settings, context) = try fixture()
        let pin = working, name = try SQLText("items"), indexName = try SQLText("items_value")
        let relation = try MutationExecutor.Relation(SQLName(name: name, span: SQLSourceSpan(byteStart: 0, byteEnd: 0, position: 1)), snapshot: pin)
        let plans = try relation.table.indexes.map {
            try StoredIndexPlan.resolve(table: relation.table, index: $0, snapshot: pin, settings: settings, context: context)
        }
        let key = try KeyEncoding.encode(.integer(1), type: relation.types[0])
        let original = try XCTUnwrap(pin.tables[name]?.get(key))
        func replacement(_ value: Int64) throws -> PackedRecord {
            try RecordDisposition.plan(key: key, values: [.value(.integer(1)), .value(.integer(value))],
                types: relation.types, codec: pin.pool.codec)
        }
        func apply(_ next: PackedRecord?) throws {
            let store = try XCTUnwrap(working.tables[name])
            try MutationExecutor.mergeWrites([.init(old: original, new: next)], relation: relation,
                indexes: plans, allocated: store, snapshot: &working, settings: settings)
        }
        func entries() throws -> [[UInt8]] {
            var cursor = try XCTUnwrap(working.indexes[indexName]).cursor(); defer { cursor.close() }
            var keys: [[UInt8]] = []
            while let row = try cursor.next() { keys.append(row.key) }
            return keys
        }
        try apply(replacement(9)); try apply(replacement(10))
        let final = try XCTUnwrap(working.tables[name]?.get(key))
        if case .integer(let value) = try ScanRecord(final, types: working.tables[name]!.tree.types, pager: nil).value(1) { XCTAssertEqual(value, 10) } else { XCTFail("expected integer") }
        if case .integer(let value) = try ScanRecord(original, types: working.tables[name]!.tree.types, pager: nil).value(1) { XCTAssertEqual(value, 7) } else { XCTFail("expected integer") }
        XCTAssertEqual(try entries(), [try KeyEncoding.encodeSlot(.integer(10), type: relation.types[1]) + key])
        XCTAssertEqual(working.catalog.tables.first?.rowCount, 1)
        try apply(nil)
        XCTAssertNil(try working.tables[name]?.get(key)); XCTAssertEqual(try entries(), [])
        XCTAssertEqual(working.catalog.tables.first?.rowCount, 0)
        try apply(replacement(11))
        XCTAssertEqual(working.catalog.tables.first?.rowCount, 1)
        XCTAssertEqual(try entries(), [try KeyEncoding.encodeSlot(.integer(11), type: relation.types[1]) + key])
        XCTAssertEqual(context.meter.accrued, 0)
    }

    func testHiddenInsertedKeyIsRejectedBeforeChangingRoots() throws {
        var (working, settings, context) = try fixture()
        let name = try SQLText("items")
        let relation = try MutationExecutor.Relation(SQLName(name: name, span: SQLSourceSpan(byteStart: 0, byteEnd: 0, position: 1)), snapshot: working)
        let key = try KeyEncoding.encode(.integer(2), type: relation.types[0])
        let inserted = try RecordDisposition.plan(key: key, values: [.value(.integer(2)), .value(.integer(8))], types: relation.types, codec: working.pool.codec)
        let plans = try relation.table.indexes.map {
            try StoredIndexPlan.resolve(table: relation.table, index: $0, snapshot: working, settings: settings, context: context)
        }
        var allocated = try XCTUnwrap(working.tables[name])
        try MutationExecutor.mergeWrites([.init(old: nil, new: inserted)], relation: relation,
            indexes: plans, allocated: allocated, snapshot: &working, settings: settings)
        let revision = working.tables[name]!.revision
        allocated = working.tables[name]!
        XCTAssertThrowsError(try MutationExecutor.mergeWrites([.init(old: nil, new: inserted)], relation: relation,
            indexes: plans, allocated: allocated, snapshot: &working, settings: settings)) {
            XCTAssertEqual(($0 as? JedError)?.code, "23505")
        }
        XCTAssertTrue(working.tables[name]!.revision === revision)
        XCTAssertEqual(working.catalog.tables.first?.rowCount, 2)
    }
}
