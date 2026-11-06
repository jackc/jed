import Foundation
import XCTest
import Synchronization
@testable import Jed
#if os(Linux)
import Glibc
#elseif canImport(Darwin)
import Darwin
#endif

final class FileHostBoundaryTests: XCTestCase {
    private func fixture(_ name: String, extension ext: String = "jed") throws -> [UInt8] {
        Array(try Data(contentsOf: XCTUnwrap(Bundle.module.url(forResource: name, withExtension: ext, subdirectory: "Fixtures"))))
    }
    private func fails(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }
    private func directory() throws -> URL {
        let path = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-host-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: path, withIntermediateDirectories: false)
        addTeardownBlock { try FileManager.default.removeItem(at: path) }
        return path
    }
    func testDeserializeCopiesInputPreservesVersionAndExportsCanonicalFixture() throws {
        var bytes = try fixture("compressed_table"), original = bytes
        let database = try Database.deserialize(bytes)
        defer { try? database.close() }
        bytes[0] ^= 1
        XCTAssertEqual(try database.serialize(), original)
        XCTAssertEqual(database.version, database.core.storage.pager.selected.meta.txid)
        XCTAssertNil(database.path)
        for name in ["temp", "MAIN", "absent"] { fails("42704") { _ = try database.serialize(name) } }
        fails("XX001") { _ = try Database.deserialize([1, 2, 3]) }
        fails("55000") { _ = try Database.deserialize(fixture("encrypted_empty_db")) }
        original[0] ^= 2
        XCTAssertNotEqual(try database.serialize(), original)
    }
    func testAbsentCollationRefusesOpenButSkewRemainsReadableAndWriteBlocked() throws {
        let bytes = try fixture("collation_skew_corrupt")
        fails("XX002") { _ = try Database.deserialize(bytes) }
        var builder = Environment.builder(); try builder.loadUnicodeData(fixture("unicode", extension: "jucd"))
        let environment = builder.build(), database = try Database.deserialize(bytes, options: DeserializeOptions(environment: environment))
        defer { try? database.close() }
        let snapshot = database.core.pinLatest(); defer { snapshot.close() }
        XCTAssertFalse(snapshot.snapshot.catalog.tables.isEmpty)
        let table = try XCTUnwrap(snapshot.snapshot.catalog.tables.first)
        fails("XX002") { try CatalogEnvironment.checkWritable(table, image: snapshot.snapshot.catalog, environment: environment) }
        XCTAssertEqual(try database.serialize(), bytes)
    }

    func testRecoveredHostRetainsDirectoryBarrierUntilFirstCommitOnly() throws {
        let bytes = MemoryBlockStore()
        _ = try StorageDomain.create(store: bytes, kind: .file, catalogID: 301, pageSize: 1024)
        let barriers = Mutex(0)
        let recovered = try StorageDomain.open(store: bytes, kind: .file, catalogID: 302,
            validateCatalog: { _ in }, durableDirectorySync: { barriers.withLock { $0 += 1 } })
        let core = DatabaseCore(storage: recovered)
        XCTAssertEqual(barriers.withLock { $0 }, 0)
        for name in ["sequence_a", "sequence_b"] {
            let writer = try core.acquireWriter(); defer { writer.close() }
            var snapshot = writer.snapshot, image = snapshot.catalog
            image.sequences.append(CatalogSequence(name: try SQLText(name)))
            try snapshot.updateCatalog(image, schemaChanged: true)
            let plan = try XCTUnwrap(writer.prepare(snapshot))
            try writer.persist(plan)
            XCTAssertEqual(barriers.withLock { $0 }, 1)
        }
    }

    #if os(Linux) || canImport(Darwin)
    func testCreateOwnershipProtocolAndCloseOrder() throws {
        let path = try directory().appendingPathComponent("database.jed").path
        let database = try Database.create(options: CreateOptions(path: path, pageSize: 1024, locking: .exclusive))
        XCTAssertEqual(database.path, path); XCTAssertEqual(database.version, 1)
        XCTAssertEqual(try [UInt8](Data(contentsOf: URL(fileURLWithPath: path))), try database.serialize())
        let ownerPath = path + ".lock/owner"
        let owner = try [UInt8](Data(contentsOf: URL(fileURLWithPath: ownerPath)))
        XCTAssertEqual(owner.count, 43)
        var record = ByteReader(owner)
        XCTAssertEqual(Array(try record.take(8)), Array("JEDOWNER".utf8)); XCTAssertEqual(try record.u16(), 1)
        XCTAssertEqual(try record.u8(), 0); XCTAssertEqual(try record.u32(), UInt32(getuid()))
        fails("55006") { _ = try Database.open(path, options: OpenOptions(locking: .exclusive, fileLockTimeoutMilliseconds: 0)) }
        let host = try XCTUnwrap(database.host as? FileDatabaseHost)
        let writer = try database.core.acquireWriter()
        fails("55006") { try database.close() }
        XCTAssertFalse(database.isClosed); try host.checkOpen()
        writer.close(); try database.close(); try database.close()
        fails("58030") { _ = try host.store.read(at: 0, count: 1) }
        XCTAssertFalse(FileManager.default.fileExists(atPath: ownerPath))
        XCTAssertTrue(FileManager.default.fileExists(atPath: path + ".lock/presence"))
        XCTAssertTrue(FileManager.default.fileExists(atPath: path + ".lock/protocol-v2"))
        fails("58P02") { _ = try Database.create(options: CreateOptions(path: path, locking: .exclusive)) }
        let reopened = try Database.open(path, options: OpenOptions(locking: .exclusive))
        try reopened.close()
        #if os(Linux)
        let automatic = try Database.open(path)
        XCTAssertNotNil(automatic.host as? ServiceDatabaseHost); try automatic.close()
        #else
        fails("0A000") { _ = try Database.open(path) }
        #endif
    }
    func testSymlinkIdentityHardLinkRefusalAndReadOnlyBudget() throws {
        let root = try directory(), path = root.appendingPathComponent("source.jed").path
        let bytes = try fixture("encrypted_index_table")
        try Data(bytes).write(to: URL(fileURLWithPath: path))
        let key = try Credential.hostKey([UInt8](repeating: 0x4a, count: 32))
        let database = try Database.open(path, options: OpenOptions(cacheBytes: 1, credential: key, readOnly: true, locking: .exclusive))
        XCTAssertTrue(database.isReadOnly)
        XCTAssertLessThanOrEqual(database.residentLeaves, 1)
        XCTAssertTrue(try XCTUnwrap(database.host as? FileDatabaseHost).scratch.sealed)
        fails("0A000") { _ = try database.serialize() }
        let link = root.appendingPathComponent("link.jed").path
        try FileManager.default.createSymbolicLink(atPath: link, withDestinationPath: path)
        fails("55006") { _ = try Database.open(link, options: OpenOptions(credential: key, locking: .exclusive)) }
        try database.close()
        let hard = root.appendingPathComponent("hard.jed").path
        try FileManager.default.linkItem(atPath: path, toPath: hard)
        fails("55006") { _ = try Database.open(hard, options: OpenOptions(credential: key, locking: .exclusive)) }
        try FileManager.default.removeItem(atPath: hard)
        let reopened = try Database.open(link, options: OpenOptions(credential: key, locking: .exclusive)); try reopened.close()
        XCTAssertEqual(try [UInt8](Data(contentsOf: URL(fileURLWithPath: path))), bytes)
    }
    func testFailedOpenAndUnknownProtocolReleaseProcessClaim() throws {
        let root = try directory(), path = root.appendingPathComponent("database.jed").path
        fails("58P01") { _ = try Database.open(path, options: OpenOptions(locking: .exclusive)) }
        try Data(fixture("encrypted_empty_db")).write(to: URL(fileURLWithPath: path))
        fails("28P02") { _ = try Database.open(path, options: OpenOptions(locking: .exclusive)) }
        let marker = path + ".lock/protocol-v999"
        try Data().write(to: URL(fileURLWithPath: marker))
        fails("0A000") { _ = try Database.open(path, options: OpenOptions(locking: .exclusive)) }
        try FileManager.default.removeItem(atPath: marker)
        let key = try Credential.hostKey([UInt8](repeating: 0x4a, count: 32))
        let opened = try Database.open(path, options: OpenOptions(credential: key, locking: .exclusive)); try opened.close()
        let presence = path + ".lock/presence"
        try FileManager.default.removeItem(atPath: presence)
        try FileManager.default.createSymbolicLink(atPath: presence, withDestinationPath: marker)
        fails("58030") { _ = try Database.open(path, options: OpenOptions(credential: key, locking: .exclusive)) }
        try FileManager.default.removeItem(atPath: presence)
        XCTAssertEqual(presence.withCString { mkfifo($0, mode_t(0o600)) }, 0)
        fails("58030") { _ = try Database.open(path, options: OpenOptions(credential: key, locking: .exclusive)) }
        try FileManager.default.removeItem(atPath: presence)
        let retry = try Database.open(path, options: OpenOptions(credential: key, locking: .exclusive)); try retry.close()
    }
    func testEncryptedCreateUsesDedicatedEntropyAndCanReopen() throws {
        let path = try directory().appendingPathComponent("database.jed").path
        var builder = Environment.builder(); builder.useInsecureKeyEntropy(seed: 0x4a454442)
        let key = try Credential.hostKey([UInt8](repeating: 0x4a, count: 32))
        let database = try Database.create(options: CreateOptions(environment: builder.build(), path: path,
            pageSize: 4096, credential: key, locking: .exclusive))
        XCTAssertTrue(try XCTUnwrap(database.host as? FileDatabaseHost).scratch.sealed)
        let first = try database.keyEntropy(16)
        let expected = SeededRandomSource(seed: 0x4a454442)
        // DEK + salt + wrap nonce + catalog nonce + both meta nonces.
        for count in [32, 32, 12, 24, 24, 24] { _ = try expected.bytes(count: count) }
        XCTAssertEqual(first, try expected.bytes(count: 16))
        try database.close()
        let reopened = try Database.open(path, options: OpenOptions(credential: key, locking: .exclusive))
        XCTAssertEqual(reopened.pageSize, 4096); XCTAssertEqual(reopened.version, 1); try reopened.close()
        fails("55000") { _ = try Database.create(options: CreateOptions(credential: key)) }
    }
    #endif
}
