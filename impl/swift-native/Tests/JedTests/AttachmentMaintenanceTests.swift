import Foundation
import XCTest
@testable import Jed

final class AttachmentMaintenanceTests: XCTestCase {
    private func directory() throws -> URL {
        let value = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-attachment-maintenance-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: value, withIntermediateDirectories: false)
        addTeardownBlock { try FileManager.default.removeItem(at: value) }
        return value
    }
    private func fails(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }
    private func key(_ byte: UInt8) throws -> Credential { try .hostKey([UInt8](repeating: byte, count: 32)) }
    private func fixture(_ name: String) throws -> Data {
        try Data(contentsOf: XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures")))
    }
    func testNamedMemoryCompactionUsesOnlyTargetWatermarkAndPreservesOtherVersions() throws {
        let database = try Database(pageSize: 1024); defer { try? database.close() }
        let name = try SQLText("scratch")
        try database.attach(name, source: .memory, readOnly: false)
        let attached = try database.domains.handle(name)
        let mainVersion = database.version, targetVersion = attached.version
        let mainPin = database.core.pinLatest(); defer { mainPin.close() }
        let targetPin = attached.core.pinLatest()
        fails("55006") { try database.compact("scratch") }
        targetPin.close()
        try database.compact("scratch")
        XCTAssertEqual(database.version, mainVersion)
        XCTAssertEqual(attached.version, targetVersion + 1)
        XCTAssertEqual(mainPin.snapshot.txid, mainVersion)
        let session = try database.session(); defer { session.close() }
        try session.begin()
        fails("55006") { try database.compact("scratch") }
        try session.rollback()
        fails("42704") { try database.compact("Scratch") }
        fails("42704") { try database.compact("temp") }
        try database.attach(try SQLText("frozen"), source: .memory, readOnly: true)
        fails("25006") { try database.compact("frozen") }
        fails("25006") { try database.changeCredential("frozen", credential: key(7)) }
    }
    func testMaintenanceIdentityCannotMoveToReattachedDomainAfterPreflight() throws {
        let database = try Database(); defer { try? database.close() }
        let name = try SQLText("scratch")
        try database.attach(name, source: .memory, readOnly: false)
        let old = try database.domains.maintenanceTarget(name, main: database)
        try database.detach(name); try database.attach(name, source: .memory, readOnly: false)
        var invoked = false
        fails("42704") {
            try database.domains.withMaintenance(name, main: database, expected: old) { invoked = true }
        }
        XCTAssertFalse(invoked)
    }
    #if os(Linux) || canImport(Darwin)
    func testFileAttachmentCompactionAndCredentialChangesRemainIndependent() throws {
        let root = try directory(), plain = root.appendingPathComponent("plain.jed")
        try fixture("compressed_table").write(to: plain)
        let encrypted = root.appendingPathComponent("encrypted.jed").path
        let first = try key(0x41), second = try key(0x42)
        let created = try Database.create(options: CreateOptions(path: encrypted, pageSize: 1024,
            credential: first, locking: .exclusive))
        try created.close()
        let database = try Database(); defer { try? database.close() }
        try database.attach(try SQLText("work"), source: .file(plain.path, OpenOptions(locking: .exclusive)), readOnly: false)
        try database.attach(try SQLText("secret"), source: .file(encrypted, OpenOptions(credential: first, locking: .exclusive)), readOnly: false)
        let work = try database.domains.handle(SQLText("work")), secret = try database.domains.handle(SQLText("secret"))
        let mainVersion = database.version, workVersion = work.version, secretVersion = secret.version
        let pin = secret.core.pinLatest(); defer { pin.close() }
        try database.compact("work")
        XCTAssertEqual(database.version, mainVersion); XCTAssertEqual(work.version, workVersion + 1)
        let session = try database.session(); defer { session.close() }
        try session.begin()
        fails("55006") { try database.changeCredential("secret", credential: second) }
        try session.rollback()
        try database.changeCredential("secret", credential: second)
        XCTAssertEqual(secret.version, secretVersion)
        let block = try XCTUnwrap(secret.core.storage.pager.selectedKeyBlock)
        _ = try block.unlock(second)
        fails("28P01") { _ = try block.unlock(first) }
        fails("55006") { try database.rekey("secret", credential: second) }
        pin.close()
        try database.rekey("secret", credential: nil)
        XCTAssertFalse(secret.core.storage.pager.selected.meta.encrypted)
        XCTAssertEqual(secret.version, secretVersion + 1)
        XCTAssertEqual(database.version, mainVersion)
    }
    func testAttachmentEncryptionUsesRootScratchPolicy() throws {
        let root = try directory(), mainPath = root.appendingPathComponent("main.jed").path
        let targetPath = root.appendingPathComponent("target.jed").path
        let target = try Database.create(options: CreateOptions(path: targetPath, pageSize: 1024, locking: .exclusive))
        try target.close()
        let main = try Database.create(options: CreateOptions(path: mainPath, pageSize: 1024, locking: .exclusive))
        try main.attach(try SQLText("target"), source: .file(targetPath, OpenOptions(locking: .exclusive, scratchEncryption: .on)), readOnly: false)
        fails("55000") { try main.rekey("target", credential: key(9)) }
        try main.close()
        let sealed = try Database.open(mainPath, options: OpenOptions(locking: .exclusive, scratchEncryption: .on))
        defer { try? sealed.close() }
        // The attachment's default auto scratch policy is plaintext here. The
        // root's sealed scratch policy controls this multi-domain handle.
        try sealed.attach(try SQLText("target"), source: .file(targetPath, OpenOptions(locking: .exclusive)), readOnly: false)
        let mainVersion = sealed.version
        try sealed.rekey("target", credential: key(9))
        XCTAssertEqual(sealed.version, mainVersion)
        XCTAssertTrue(try sealed.domains.handle(SQLText("target")).core.storage.pager.selected.meta.encrypted)
    }
    #endif
}
