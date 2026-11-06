import Foundation
import XCTest
@testable import Jed

final class SimIntegrityBoundaryTests: XCTestCase {
    func testRetainedRootPayloadMustMatchItsSealedChain() throws {
        let device = MemoryBlockStore(), storage = try StorageDomain.create(store: device, kind: .file, catalogID: 1, pageSize: 512)
        let name = try SQLText("t"), integer = TypeDescriptor(.scalar(.i64)), bytes = TypeDescriptor(.scalar(.bytea))
        var snapshot = storage.committed, image = snapshot.catalog
        image.tables.append(CatalogTable(name: name, columns: [
            CatalogColumn(name: try SQLText("id"), type: CatalogTypeUse(.scalar(.i64)), notNull: true),
            CatalogColumn(name: try SQLText("v"), type: CatalogTypeUse(.scalar(.bytea)))
        ], primaryKey: [0], root: PageSealer.nullReference(storage.pager.codec)))
        try snapshot.updateCatalog(image, schemaChanged: true)
        var table = snapshot.emptyStore(types: [integer, bytes])
        let content = (0..<4096).map { UInt8(truncatingIfNeeded: $0 &* 73 &+ ($0 >> 4)) }
        let row = try RecordDisposition.plan(key: KeyEncoding.encode(.integer(1), type: integer),
            values: [.value(.integer(1)), .value(.bytea(content))], types: [integer, bytes], codec: storage.pager.codec)
        try table.put(row, netStaging: true); snapshot.setTableStore(name, table)
        _ = try storage.persist(XCTUnwrap(storage.prepare(snapshot)))
        let tree = try XCTUnwrap(storage.committed.tables[name]).tree
        let root = try tree.resolve(XCTUnwrap(tree.root))
        XCTAssertFalse(root.pending.isEmpty)
        try SimIntegrity.live(storage)
        let head = try XCTUnwrap(root.record(at: 0).cells[1].largeFacts(encrypted: false)?.external)
        var changed = try storage.pager.readBody(head)
        changed[16] ^= 1
        let sealed = try storage.pager.codec.sealBody(pageIndex: head.index, plaintext: changed, salt: [])
        try device.write(at: UInt64(head.index) * UInt64(storage.pager.pageSize), bytes: sealed.bytes)
        XCTAssertThrowsError(try SimIntegrity.live(storage)) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX001")
            XCTAssertTrue(String(describing: $0).contains("cache differs from its committed chain"))
        }
    }

    func testFrozenStaleRootRegressionChecksResidentReferences() throws {
        let path = try XCTUnwrap(Bundle.module.url(forResource: "stale-root-leaf-chains", withExtension: "sim", subdirectory: "Fixtures/Simulation/regress"))
        let result = try SimRunner(SimTrace(String(contentsOf: path, encoding: .utf8))).run()
        XCTAssertEqual(result.violation, "")
    }

    func testIndependentImageAndLiveWalksOnFrozenStorageForms() throws {
        let environment = Environment.builder().build()
        for name in ["index_table", "overflow_table", "gist_range_table", "gist_scalar_table", "gist_exclude_table",
                     "encrypted_index_table", "encrypted_overflow_table"] {
            let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures"))
            let bytes = [UInt8](try Data(contentsOf: url))
            let credential: Credential? = name.hasPrefix("encrypted_") ? try .hostKey(Array(repeating: 0x4a, count: 32)) : nil
            let facts = try SimIntegrity.image(bytes: bytes, credential: credential, environment: environment)
            XCTAssertEqual(facts.reachable.count, Int(facts.livePages), name)
            XCTAssertTrue(facts.reachable.isDisjoint(with: facts.freePages), name)
            XCTAssertFalse(facts.pointerKinds["root", default: []].isEmpty, name)
            let copy = MemoryBlockStore(); try copy.write(at: 0, bytes: bytes)
            let storage = try StorageDomain.open(store: copy, catalogID: 1, cacheBytes: 0,
                credential: credential, validateCatalog: { _ in })
            try SimIntegrity.live(storage)
            if name.contains("overflow") {
                XCTAssertFalse((facts.pointerKinds["overflow", default: []] + facts.pointerKinds["overflow_lz4", default: []]).isEmpty, name)
            }
        }
    }

    func testImageCheckerRejectsAValidlyEncodedButMiskeyedTableRow() throws {
        let device = MemoryBlockStore(), storage = try StorageDomain.create(store: device, kind: .file, catalogID: 1, pageSize: 1024)
        var snapshot = storage.committed, image = snapshot.catalog
        let name = try SQLText("t"), integer = TypeDescriptor(.scalar(.i32))
        image.tables.append(CatalogTable(name: name, columns: [CatalogColumn(name: try SQLText("id"),
            type: CatalogTypeUse(.scalar(.i32)), notNull: true)], primaryKey: [0], root: PageSealer.nullReference(storage.pager.codec)))
        try snapshot.updateCatalog(image, schemaChanged: true)
        var table = snapshot.emptyStore(types: [integer])
        let row = try RecordDisposition.plan(key: KeyEncoding.encode(.integer(2), type: integer),
            values: [.value(.integer(1))], types: [integer], codec: storage.pager.codec)
        try table.put(row, netStaging: true); snapshot.setTableStore(name, table)
        let plan = try XCTUnwrap(storage.prepare(snapshot))
        _ = try storage.persist(plan)
        let bytes = try device.read(at: 0, count: Int(device.size()))
        XCTAssertThrowsError(try SimIntegrity.image(bytes: bytes, credential: nil, environment: Environment.builder().build())) {
            XCTAssertEqual(($0 as? JedError)?.code, "XX001")
            XCTAssertTrue(String(describing: $0).contains("row_key_not_primary_key"))
        }
    }
}
