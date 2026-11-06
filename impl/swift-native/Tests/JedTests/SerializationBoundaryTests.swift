import Foundation
import XCTest
@testable import Jed

final class SerializationBoundaryTests: XCTestCase {
    private func fails(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }
    private func fixture(_ name: String) throws -> [UInt8] {
        Array(try Data(contentsOf: XCTUnwrap(Bundle.module.url(forResource: name, withExtension: "jed", subdirectory: "Fixtures"))))
    }
    private func directory() throws -> URL {
        let result = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-serialization-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: result, withIntermediateDirectories: false)
        addTeardownBlock { try FileManager.default.removeItem(at: result) }
        return result
    }
    func testNamedExportPinsOnlyTargetAndExcludesWorkingChanges() throws {
        let database = try Database(pageSize: 1024), session = try database.session(), name = try SQLText("scratch")
        defer { session.close(); try? database.close() }
        try database.attach(name, source: .memory, readOnly: false)
        try session.execute("CREATE TABLE scratch.items (id i32 PRIMARY KEY, value text)")
        try session.execute("INSERT INTO scratch.items VALUES (1, 'committed')")
        let attached = try database.domains.handle(name)
        let baseline = try database.serialize("scratch"), main = try database.serialize(), version = attached.version
        let pin = try database.domains.pinCommitted(name, main: database)
        XCTAssertEqual(database.core.livePinCount, 0)
        XCTAssertEqual(attached.core.livePinCount, 1)
        fails("55006") { try database.detach(name) }
        try session.begin()
        try session.execute("INSERT INTO scratch.items VALUES (2, 'pending')")
        // Export neither waits for the active writer nor adopts its pending tree.
        XCTAssertEqual(try database.serialize("scratch"), baseline)
        try session.commit()
        XCTAssertEqual(pin.snapshot.txid, version)
        XCTAssertEqual(attached.version, version + 1)
        XCTAssertEqual(try database.serialize(), main)
        let exported = try database.serialize("scratch")
        XCTAssertNotEqual(exported, baseline)
        let copy = try Database.deserialize(exported); defer { try? copy.close() }
        XCTAssertEqual(copy.pageSize, 1024); XCTAssertEqual(copy.version, version + 1)
        XCTAssertEqual(try copy.serialize(), exported)
        let rows = try copy.query("SELECT count(*) FROM items"); defer { rows.close() }
        guard let row = try rows.next(), case .integer(let count) = row[0] else { return XCTFail("missing imported count") }
        XCTAssertEqual(count, 2)
        pin.close()
        try database.detach(name)
        for invalid in ["scratch", "Scratch", "temp", "MAIN"] { fails("42704") { _ = try database.serialize(invalid) } }
    }
    #if os(Linux) || canImport(Darwin)
    func testReadOnlyFileExportsCanonicalBytesAndEncryptedTargetRefuses() throws {
        let root = try directory(), path = root.appendingPathComponent("plain.jed"), encrypted = root.appendingPathComponent("encrypted.jed")
        let original = try fixture("compressed_table")
        try Data(original).write(to: path)
        try Data(fixture("encrypted_empty_db")).write(to: encrypted)
        let database = try Database(); defer { try? database.close() }
        try database.attach(SQLText("archive"), source: .file(path.path, OpenOptions(locking: .exclusive)), readOnly: true)
        XCTAssertEqual(try database.serialize("archive"), original)
        let key = try Credential.hostKey([UInt8](repeating: 0x4a, count: 32))
        try database.attach(SQLText("secret"), source: .file(encrypted.path, OpenOptions(credential: key, locking: .exclusive)), readOnly: true)
        fails("0A000") { _ = try database.serialize("secret") }
        XCTAssertEqual(try database.domains.handle(SQLText("secret")).core.livePinCount, 0)
        try database.detach(SQLText("secret"))
    }
    #endif
}
