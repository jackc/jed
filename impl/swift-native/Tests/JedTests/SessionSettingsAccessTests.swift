import Foundation
import XCTest
@testable import Jed

final class SessionSettingsAccessTests: XCTestCase {
    private func fails(_ code: String, _ body: () throws -> Void, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try body(), file: file, line: line) {
            XCTAssertEqual(($0 as? JedError)?.code, code, file: file, line: line)
        }
    }
    func testRuntimeZeroAndFrozenOperatorBudget() throws {
        let database = try Database(); defer { try? database.close() }
        var options = SessionOptions(); options.workMemoryBytes = 0
        let session = try database.session(options: options); defer { session.close() }
        XCTAssertEqual(session.workMemoryBytes, 256 * 1024 * 1024)
        session.workMemoryBytes = 257
        let services = StatementServices(settings: session.settings, meter: try session.settings.newMeter())
        session.workMemoryBytes = 0
        XCTAssertEqual(services.context().spill?.workMemoryBytes, 257)
        XCTAssertEqual(StatementServices(settings: session.settings, meter: try session.settings.newMeter()).context().spill?.workMemoryBytes, 0)
        XCTAssertEqual(session.workMemoryBytes, 0)
        session.maxSQLLength = 7
        fails("54000") { try session.execute("SELECT 1") }
        session.maxSQLLength = 0
        try session.execute("SELECT 1")
        XCTAssertEqual(session.status, .idle)
    }
    func testRuntimeTimeoutUsesExistingGateAndReadOnlySettingsRemainMutable() throws {
        let database = try Database(); defer { try? database.close() }
        let holder = try database.writeSession(); defer { holder.close() }
        let contender = try database.session(); defer { contender.close() }
        contender.lockTimeoutMilliseconds = 1
        fails("55P03") { try contender.begin() }
        XCTAssertEqual(contender.status, .idle)
        XCTAssertTrue(database.core.hasLocalWriter)
        let reader = try database.readSession(); defer { reader.close() }
        reader.workMemoryBytes = 0; reader.maxSQLLength = 0; reader.lockTimeoutMilliseconds = 2
        reader.temporaryBufferBytes = 0; reader.maxTemporaryStorageBytes = 0
        XCTAssertEqual(reader.workMemoryBytes, 0)
        XCTAssertEqual(reader.temporaryBufferBytes, 0)
        XCTAssertEqual(reader.maxTemporaryStorageBytes, 0)
        try reader.execute("SELECT 1")
    }
    func testRuntimePrivilegesAffectNewStatementsAndKeepExplicitDenials() throws {
        let database = try Database(); defer { try? database.close() }
        try database.execute("CREATE TABLE t (id i32 PRIMARY KEY)")
        try database.execute("INSERT INTO t VALUES (1)")
        let session = try database.session(); defer { session.close() }
        let admitted = try session.query("SELECT id FROM t"); defer { admitted.close() }
        session.defaultPrivileges = []
        XCTAssertNotNil(try admitted.next())
        fails("42501") { try session.execute("SELECT id FROM t") }
        session.grant(.select, on: try SQLText("t"))
        try session.execute("SELECT id FROM t")
        session.revoke(.select, on: try SQLText("t"))
        session.defaultPrivileges = .allTables
        fails("42501") { try session.execute("SELECT id FROM t") }
        XCTAssertEqual(session.defaultPrivileges, .allTables)
    }
    func testDDLGateInheritanceOccursOnlyAtSessionCreation() throws {
        let database = try Database(); defer { try? database.close() }
        var options = SessionOptions(); options.allowDDL = false
        let session = try database.session(options: options); defer { session.close() }
        XCTAssertFalse(session.allowTemporaryDDL)
        session.allowDDL = true
        XCTAssertFalse(session.allowTemporaryDDL)
        try session.execute("CREATE TABLE persistent (id i32)")
        fails("42501") { try session.execute("CREATE TEMP TABLE denied_temp (id i32)") }
        session.allowDDL = false; session.allowTemporaryDDL = true
        try session.execute("CREATE TEMP TABLE allowed_temp (id i32)")
        fails("42501") { try session.execute("CREATE TABLE denied_main (id i32)") }
        XCTAssertFalse(session.allowDDL)
        XCTAssertTrue(session.allowTemporaryDDL)
    }
    func testLoadedZoneMetadataIsSortedImmutableAndSharedByHandles() throws {
        var builder = Environment.builder()
        let bare = builder.build()
        XCTAssertTrue(bare.loadedTimeZones().isEmpty)
        let url = try XCTUnwrap(Bundle.module.url(forResource: "tzdata.jtz", withExtension: nil, subdirectory: "Fixtures"))
        let bytes = Array(try Data(contentsOf: url))
        try builder.loadTimeZoneData(bytes)
        let environment = builder.build(), first = environment.loadedTimeZones()
        XCTAssertFalse(first.isEmpty)
        XCTAssertEqual(first.map(\.name), first.map(\.name).sorted())
        XCTAssertEqual(Set(first.map(\.name)).count, first.count)
        XCTAssertTrue(first.allSatisfy { !$0.tzdataVersion.utf8.isEmpty })
        XCTAssertFalse(first.contains { $0.name.description == "UTC" })
        try builder.loadTimeZoneData(bytes)
        XCTAssertEqual(builder.build().loadedTimeZones(), first)
        XCTAssertTrue(bare.loadedTimeZones().isEmpty)
        let database = try Database(environment: environment); defer { try? database.close() }
        let session = try database.session(); defer { session.close() }
        XCTAssertEqual(database.loadedTimeZones(), first)
        XCTAssertEqual(session.loadedTimeZones(), first)
    }
}
