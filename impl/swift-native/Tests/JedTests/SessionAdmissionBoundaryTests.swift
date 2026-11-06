import Foundation
import XCTest
@testable import Jed

final class SessionAdmissionBoundaryTests: XCTestCase {
    private func fails(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) { XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line) }
    }
    func testEagerWriterOwnsGateAndFailedAdmissionReleasesPrivateState() throws {
        let database = try Database(); defer { try? database.close() }
        let lazy = try database.session(), reader = try database.readSession()
        defer { lazy.close(); reader.close() }
        XCTAssertEqual(lazy.status, .idle); XCTAssertEqual(reader.status, .idle)
        XCTAssertEqual(database.core.livePinCount, 1)
        let writer = try database.writeSession(); defer { writer.close() }
        XCTAssertEqual(writer.status, .open); XCTAssertTrue(database.core.hasLocalWriter)
        fails("55006") { try database.close() }
        var options = SessionOptions(); options.lockTimeoutMilliseconds = 1
        fails("55P03") { _ = try database.writeSession(options: options) }
        try writer.execute("CREATE TABLE published_after_commit (id i32)")
        let version = database.version
        try writer.commit()
        XCTAssertEqual(writer.status, .idle); XCTAssertEqual(database.version, version + 1)
        XCTAssertFalse(database.core.hasLocalWriter)
        try reader.withHostCatalogRead { XCTAssertTrue($0.catalog.tables.isEmpty) }
        try lazy.withHostCatalogRead { XCTAssertEqual($0.catalog.tables.count, 1) }
        let successor = try database.writeSession(options: options)
        XCTAssertEqual(successor.status, .open)
        successor.close()
        XCTAssertFalse(database.core.hasLocalWriter)
        reader.close(); lazy.close()
        XCTAssertEqual(database.core.livePinCount, 0)
    }
    #if os(Linux) || canImport(Darwin)
    func testReadOnlyDatabaseConveniencesRemainPinnedAndIdle() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-readonly-session-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: root, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: root) }
        let path = root.appendingPathComponent("file.jed").path
        let created = try Database.create(options: CreateOptions(path: path, locking: .exclusive)); try created.close()
        let database = try Database.open(path, options: OpenOptions(readOnly: true, locking: .exclusive))
        defer { try? database.close() }
        let sessions = [try database.session(), try database.readSession(), try database.writeSession()]
        defer { for session in sessions { session.close() } }
        XCTAssertEqual(database.core.livePinCount, 3)
        XCTAssertFalse(database.core.hasLocalWriter)
        for session in sessions {
            XCTAssertEqual(session.status, .idle)
            fails("25006") { try session.execute("CREATE TABLE forbidden (id i32)") }
            XCTAssertEqual(session.status, .idle)
        }
    }
    #endif
}
