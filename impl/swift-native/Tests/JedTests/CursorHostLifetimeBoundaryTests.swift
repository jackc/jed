import Foundation
import XCTest
@testable import Jed

final class CursorHostLifetimeBoundaryTests: XCTestCase {
    #if os(Linux) || canImport(Darwin)
    func testEscapedFileCursorKeepsHostUntilCloseWithoutRetainingSession() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-cursor-lifetime-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let path = directory.appendingPathComponent("database.jed").path
        do {
            let seed = try Database.create(options: CreateOptions(path: path, pageSize: 512, locking: .exclusive))
            defer { try? seed.close() }
            try seed.execute("CREATE TABLE t(id i32 PRIMARY KEY, value i32)")
            try seed.execute("INSERT INTO t SELECT i,i*2 FROM generate_series(1,120) g(i)")
        }
        weak var database: Database?
        weak var session: Session?
        let first: Rows, second: Rows
        do {
            let opened = try Database.open(path, options: OpenOptions(cacheBytes: 1, locking: .exclusive))
            let read = try opened.readSession()
            database = opened; session = read
            first = try read.query("SELECT id,value FROM t ORDER BY id")
            second = try read.query("SELECT id FROM t ORDER BY id DESC")
            read.close()
            opened.core.storage.pool.invalidateAll()
        }
        XCTAssertNil(session, "cursor retention must not extend session or TEMP ownership")
        XCTAssertNotNil(database, "cursor owns the host after the application's last handle reference")
        XCTAssertEqual(try first.nextRow()?.get(0, as: Int.self), 1)
        XCTAssertEqual(try second.nextRow()?.get(0, as: Int.self), 120)
        first.close(); first.close()
        XCTAssertNotNil(database, "a sibling cursor keeps its own host lifetime")
        var count = 1
        while let row = try second.nextRow() {
            count += 1
            XCTAssertEqual(try row.get(0, as: Int.self), 121 - count)
        }
        XCTAssertEqual(count, 120)
        XCTAssertTrue(second.isClosed)
        XCTAssertNil(database, "exhaustion releases host ownership even while closed Rows remain referenced")
        let reopened = try Database.open(path, options: OpenOptions(locking: .exclusive, fileLockTimeoutMilliseconds: 0))
        try reopened.close()
    }
    #endif
}
