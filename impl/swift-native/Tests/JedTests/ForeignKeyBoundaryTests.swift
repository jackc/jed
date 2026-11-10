import XCTest
@testable import Jed

final class ForeignKeyBoundaryTests: XCTestCase {
    private func fixture(order: UInt8) throws -> (DomainSnapshot, ForeignKeyValidation.Plan, SessionSettings) {
        let codec = try PlainPageCodec(pageSize: 8192)
        let root = PageSealer.nullReference(codec)
        let pool = try NodePool(codec: codec, cacheBytes: nil) { _ in throw JedError("XX000", "resident test tree must not read a device") }
        let parent = CatalogTable(name: try SQLText("parent"), columns: [
            CatalogColumn(name: try SQLText("id"), type: CatalogTypeUse(.scalar(.i64)), notNull: true)
        ], primaryKey: [0], root: root)
        let index = CatalogIndex(name: try SQLText("child_fk"), keys: [.column(1), .column(2)], root: root, keyOptions: [order, order])
        let foreign = CatalogForeignKey(name: try SQLText("fk"), columns: [1], referencedTable: parent.name,
            referencedColumns: [0], onDelete: 2, onUpdate: 2)
        let child = CatalogTable(name: try SQLText("child"), columns: [
            CatalogColumn(name: try SQLText("id"), type: CatalogTypeUse(.scalar(.i32)), notNull: true),
            CatalogColumn(name: try SQLText("pid"), type: CatalogTypeUse(.scalar(.i16))),
            CatalogColumn(name: try SQLText("tail"), type: CatalogTypeUse(.scalar(.i32)))
        ], primaryKey: [0], indexes: [index], foreignKeys: [foreign], root: root)
        var image = CatalogImage(); image.tables = [child, parent]
        var snapshot = try DomainSnapshot(catalogID: 17, txid: 0, catalog: image, pool: pool,
            tables: [:], indexes: [:], gistTrees: [:])
        let types = try child.columns.map { try $0.type.resolve(snapshot.types) }
        var store = snapshot.emptyStore(types: types), tree = snapshot.emptyStore(types: [])
        let values: [[SQLValue]] = [[.integer(10), .integer(7), .null], [.integer(2), .integer(7), .integer(9)],
                                  [.integer(3), .integer(8), .integer(1)], [.integer(4), .null, .integer(7)]]
        for row in values {
            let key = try KeyEncoding.encode(row[0], type: types[0])
            try store.put(RecordDisposition.plan(key: key, values: row.map(RecordInput.value), types: types, codec: codec), netStaging: false)
            var prefix: [UInt8] = []
            for ordinal in [1, 2] {
                prefix += try KeyEncoding.encodeSlot(row[ordinal], type: types[ordinal], descending: order & 1 != 0, nullsFirst: order & 2 != 0)
            }
            try tree.put(PackedRecord(key: prefix + key, cells: []), netStaging: false)
        }
        snapshot.setTableStore(child.name, store); snapshot.setIndexStore(index.name, tree)
        snapshot.setTableStore(parent.name, snapshot.emptyStore(types: [TypeDescriptor(.scalar(.i64))]))
        let plan = try ForeignKeyValidation.Plan(child: child, foreign: foreign, snapshot: snapshot)
        return (snapshot, plan, SessionSettings(environment: Environment.builder().build(), options: SessionOptions()))
    }

    func testKeyedGatherDecodesNullableOrderedTailAndVisitsStorageOrder() throws {
        for order in UInt8(0)...UInt8(3) {
            let (snapshot, plan, settings) = try fixture(order: order)
            let meter = CostMeter(), context = ExpressionContext(meter: meter)
            let tuple: [SQLValue] = [.integer(7)]
            let key = try plan.encodeParentTuple(tuple, context: context)
            let target = ForeignKeyActions.Target(plan: plan, action: 2, deleting: true, tuples: [tuple], replacements: nil, positions: [key: 0])
            var gather = try target.gather(snapshot: snapshot, settings: settings, context: context, touched: [1])
            defer { gather.close() }
            var ids: [Int64] = []
            while let record = try gather.next() {
                if case .integer(let value) = try ScanRecord(record, types: plan.childTypes, pager: nil).value(0) { ids.append(value) }
            }
            XCTAssertEqual(ids, [2, 10])
            XCTAssertEqual(meter.accrued, 3, "one index leaf plus two table point lookups; row reads belong to the mutation loop")
            let limited = CostMeter(limit: 3)
            XCTAssertThrowsError(try target.gather(snapshot: snapshot, settings: settings,
                context: ExpressionContext(meter: limited), touched: [1])) {
                XCTAssertEqual(($0 as? JedError)?.code, "54P01")
            }
            XCTAssertEqual(limited.accrued, 3)
        }
    }

    func testActionDepthGuardRunsBeforeChildScanAndRestoresState() throws {
        var (snapshot, plan, settings) = try fixture(order: 0)
        let meter = CostMeter(), context = ExpressionContext(meter: meter)
        let old = ForeignKeyValidation.Candidate(key: [], value: { _ in .integer(7) })
        let state = ForeignKeyActions.State(); state.depth = 256
        let revision = snapshot.tables[plan.child.name]!.revision
        XCTAssertThrowsError(try ForeignKeyActions.apply([.init(old: old, new: nil)], parent: plan.parent,
            deleting: true, snapshot: &snapshot, settings: settings, context: context, state: state)) {
            XCTAssertEqual(($0 as? JedError)?.code, "54001")
        }
        XCTAssertEqual(state.depth, 256)
        XCTAssertEqual(meter.accrued, 0)
        XCTAssertTrue(snapshot.tables[plan.child.name]!.revision === revision)
    }
}
