import Foundation
import XCTest
import Synchronization
@testable import Jed

private final class MaintenanceRecordedStore: BlockStore {
    let memory = MemoryBlockStore()
    private let events = Mutex<[String]>([])
    func read(at offset: UInt64, count: Int) throws -> [UInt8] { try memory.read(at: offset, count: count) }
    func write(at offset: UInt64, bytes: [UInt8]) throws {
        events.withLock { $0.append("write:\(offset)") }; try memory.write(at: offset, bytes: bytes)
    }
    func size() throws -> UInt64 { try memory.size() }
    func setSize(_ count: UInt64) throws { try memory.setSize(count) }
    func sync() throws { events.withLock { $0.append("sync") } }
    func reset() { events.withLock { $0.removeAll() } }
    var log: [String] { events.withLock { $0 } }
}

final class MaintenanceBoundaryTests: XCTestCase {
    private func fixture(_ name: String) throws -> [UInt8] {
        Array(try Data(contentsOf: XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures"))))
    }
    private func fails(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }
    private func directory() throws -> URL {
        let path = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-maintenance-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: path, withIntermediateDirectories: false)
        addTeardownBlock { try FileManager.default.removeItem(at: path) }
        return path
    }
    private func key(_ byte: UInt8) throws -> Credential { try .hostKey([UInt8](repeating: byte, count: 32)) }

    func testMemoryCompactionRequiresNoPinsOrWriterAndRetiresOldAddressSpace() throws {
        let database = try Database.deserialize(fixture("compressed_table"))
        defer { try? database.close() }
        let original = database.core.storage, version = database.version, storageBytes = database.storageBytes
        let pin = database.core.pinLatest()
        fails("55006") { try database.compact() }
        pin.close()
        let writer = try database.core.acquireWriter()
        fails("55006") { try database.compact() }
        writer.close()
        fails("42704") { try database.compact("MAIN") }
        database.maxStorageBytes = 1 // Explicit compaction is permitted even after lowering Q4.
        try database.compact()
        XCTAssertEqual(database.version, version + 1)
        XCTAssertEqual(database.storageBytes, storageBytes)
        XCTAssertFalse(database.core.storage === original)
        fails("58030") { _ = try original.pager.readStored(0) }
        XCTAssertEqual(database.core.storage.committed.catalog.tables.map(\.name), original.committed.catalog.tables.map(\.name))
    }

    func testKeyBlockWriteOrderGenerationAndNoDataPageChanges() throws {
        let source = MaintenanceRecordedStore(), random = SeededRandomSource(seed: 17)
        let old = try key(0x4a), new = try key(0x5b)
        let domain = try StorageDomain.create(store: source, kind: .file, catalogID: 801, pageSize: 1024,
            credential: old, entropy: { try random.bytes(count: $0) })
        let pager = domain.pager, selected = try XCTUnwrap(pager.selectedKeyBlock)
        let codec = try XCTUnwrap(pager.codec as? EncryptedPageCodec)
        let before = try source.read(at: 0, count: Int(source.size()))
        let replacement = try codec.rewrap(selected, credential: new, parameters: .default, entropy: { try random.bytes(count: $0) })
        source.reset()
        try pager.changeKeyBlock(replacement, replacing: selected, syncDirectory: {})
        XCTAssertEqual(source.log, ["sync", "write:3072", "sync", "write:2048", "sync"])
        let after = try source.read(at: 0, count: Int(source.size()))
        XCTAssertEqual(Array(before[..<2048]), Array(after[..<2048]))
        XCTAssertEqual(Array(before[4096...]), Array(after[4096...]))
        XCTAssertEqual(pager.selectedKeyBlock?.generation, selected.generation + 1)
        XCTAssertEqual(try replacement.unlock(new).count, 32)
        fails("28P01") { _ = try replacement.unlock(old) }
        source.reset()
        fails("55006") { try pager.changeKeyBlock(replacement, replacing: selected, syncDirectory: {}) }
        XCTAssertEqual(source.log, [])
        XCTAssertFalse(pager.isPoisoned)
    }

    func testRewrapPreservesRecoveredGenerationStabilizationBeforeDataWrites() throws {
        let source = MaintenanceRecordedStore(), random = SeededRandomSource(seed: 81)
        let old = try key(0x4a), new = try key(0x5b)
        let created = try StorageDomain.create(store: source, kind: .file, catalogID: 811, pageSize: 1024,
            credential: old, entropy: { try random.bytes(count: $0) })
        var first = created.committed, catalog = first.catalog
        catalog.sequences.append(CatalogSequence(name: try SQLText("first")))
        try first.updateCatalog(catalog, schemaChanged: true)
        let initial = try XCTUnwrap(created.prepare(first, entropy: { try random.bytes(count: $0) }))
        _ = try created.persist(initial)
        let reopened = try StorageDomain.open(store: source, catalogID: 812, credential: old, validateCatalog: { _ in })
        let pager = reopened.pager, selected = try XCTUnwrap(pager.selectedKeyBlock)
        let codec = try XCTUnwrap(pager.codec as? EncryptedPageCodec)
        let replacement = try codec.rewrap(selected, credential: new, parameters: .default, entropy: { try random.bytes(count: $0) })
        try pager.changeKeyBlock(replacement, replacing: selected, syncDirectory: {})
        var second = reopened.committed; catalog = second.catalog
        catalog.sequences.append(CatalogSequence(name: try SQLText("second")))
        try second.updateCatalog(catalog, schemaChanged: true)
        let prepared = try XCTUnwrap(reopened.prepare(second, entropy: { try random.bytes(count: $0) }))
        let before = try source.read(at: 0, count: Int(source.size()))
        source.reset(); pager.armFault(.sync(ordinal: 1))
        fails("58030") { _ = try reopened.persist(prepared) }
        XCTAssertTrue(source.log.isEmpty, "the injected pre-write barrier must precede every body write")
        XCTAssertEqual(try source.read(at: 0, count: Int(source.size())), before)
        XCTAssertTrue(pager.isPoisoned)
    }

    #if os(Linux) || canImport(Darwin)
    func testFileCompactionPreservesModeLockBundleAndCanonicalBody() throws {
        let root = try directory(), path = root.appendingPathComponent("database.jed")
        let original = try fixture("compressed_table")
        try Data(original + [UInt8](repeating: 0, count: 16384)).write(to: path)
        try FileManager.default.setAttributes([.posixPermissions: 0o640], ofItemAtPath: path.path)
        let database = try Database.open(path.path, options: OpenOptions(cacheBytes: 1024, locking: .exclusive))
        defer { try? database.close() }
        let version = database.version, live = database.storageBytes, size = database.pageSize
        try Data("incomplete prior image".utf8).write(to: URL(fileURLWithPath: path.path + ".jedtmp"))
        try database.compact()
        XCTAssertEqual(database.version, version + 1); XCTAssertEqual(database.storageBytes, live)
        XCTAssertEqual(try Data(contentsOf: path).count, original.count)
        XCTAssertEqual(Array(try Data(contentsOf: path).dropFirst(2 * size)), Array(original.dropFirst(2 * size)))
        XCTAssertEqual((try FileManager.default.attributesOfItem(atPath: path.path)[.posixPermissions] as? NSNumber)?.intValue, 0o640)
        XCTAssertFalse(FileManager.default.fileExists(atPath: path.path + ".jedtmp"))
        fails("55006") { _ = try Database.open(path.path, options: OpenOptions(locking: .exclusive, fileLockTimeoutMilliseconds: 0)) }
    }

    func testCredentialChangeAllowsReadersAndFailedBarrierRequiresReopen() throws {
        let path = try directory().appendingPathComponent("database.jed").path
        let old = try key(0x4a), new = try key(0x5b), third = try key(0x6c)
        let database = try Database.create(options: CreateOptions(path: path, pageSize: 1024, credential: old, locking: .exclusive))
        let pin = database.core.pinLatest(), version = database.version
        try database.changeCredential(credential: new)
        XCTAssertEqual(database.version, version)
        _ = try pin.snapshot.pool.pager?.readStored(pin.snapshot.pool.codec.firstBodyPage)
        pin.close()
        database.core.storage.pager.armFault(.sync(ordinal: 2))
        fails("58030") { try database.changeCredential(credential: third) }
        fails("58030") { try database.checkOpen() }
        // Failed flush still closes the host and releases its exclusive bundle.
        fails("58030") { try database.close() }
        XCTAssertTrue(database.isClosed)
        fails("28P01") { _ = try Database.open(path, options: OpenOptions(credential: new, locking: .exclusive)) }
        let recovered = try Database.open(path, options: OpenOptions(credential: third, locking: .exclusive))
        XCTAssertEqual(recovered.version, version)
        XCTAssertEqual(recovered.core.storage.pager.selectedKeyBlock?.generation, 3)
        try recovered.close()
    }

    func testRekeyRoundTripRebuildsLayoutAndQ4RefusalLeavesOriginalFile() throws {
        let path = try directory().appendingPathComponent("database.jed").path
        try Data(fixture("encrypted_index_table")).write(to: URL(fileURLWithPath: path))
        var builder = Environment.builder(); builder.useInsecureKeyEntropy(seed: 45)
        let old = try key(0x4a)
        let database = try Database.open(path, options: OpenOptions(environment: builder.build(), cacheBytes: 1024,
            credential: old, locking: .exclusive, scratchEncryption: .on))
        defer { try? database.close() }
        try database.rekey(credential: nil)
        let version = database.version, oldCatalog = database.core.storage.committed.catalog.tables
        let unchanged = try Data(contentsOf: URL(fileURLWithPath: path))
        database.maxStorageBytes = 1
        fails("54P06") { try database.rekey(credential: old) }
        XCTAssertEqual(try Data(contentsOf: URL(fileURLWithPath: path)), unchanged)
        XCTAssertEqual(database.version, version)
        XCTAssertFalse(FileManager.default.fileExists(atPath: path + ".jedtmp"))
        database.maxStorageBytes = 0
        try database.rekey(credential: old)
        XCTAssertTrue(database.core.storage.pager.selected.meta.encrypted)
        XCTAssertEqual(database.version, version + 1)
        XCTAssertEqual(database.core.storage.pager.selectedKeyBlock?.generation, 1)
        let encrypted = try Data(contentsOf: URL(fileURLWithPath: path))
        let opened = try Pager.open(store: (database.host as! FileDatabaseHost).store, credential: old)
        XCTAssertEqual(opened.selected.meta.txid, version + 1)
        let beforeRotate = database.core.storage.pager.selectedKeyBlock
        try database.rekey(credential: old)
        XCTAssertNotEqual(database.core.storage.pager.selectedKeyBlock, beforeRotate)
        XCTAssertNotEqual(try Data(contentsOf: URL(fileURLWithPath: path)), encrypted)
        try database.rekey(credential: nil)
        XCTAssertFalse(database.core.storage.pager.selected.meta.encrypted)
        XCTAssertEqual(database.version, version + 3)
        let afterCatalog = database.core.storage.committed.catalog.tables
        XCTAssertEqual(afterCatalog.map(\.name), oldCatalog.map(\.name))
        XCTAssertEqual(afterCatalog.map(\.rowCount), oldCatalog.map(\.rowCount))
        for table in afterCatalog {
            var cursor = try XCTUnwrap(database.core.storage.committed.tables[table.name]).cursor()
            var count: Int64 = 0
            while let record = try cursor.next() {
                for column in record.cells.indices { _ = try record.materialize(column: column, pager: database.core.storage.pager, pageCount: database.core.storage.pager.selected.meta.pageCount) }
                count += 1
            }
            XCTAssertEqual(count, table.rowCount)
        }
    }
    #endif
}
