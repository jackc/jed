#if os(Linux) || canImport(Darwin)
import Foundation
import XCTest
@testable import Jed

final class AttachmentCatalogBoundaryTests: XCTestCase {
    func testAttachedCatalogKeepsViewMetadataWithoutResolvingViewColumns() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-attached-catalog-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let path = directory.appendingPathComponent("catalog.jed").path
        let child = try Database.create(options: CreateOptions(path: path, locking: .exclusive))
        try child.execute("CREATE TABLE item (id i32 PRIMARY KEY)")
        try child.execute("CREATE VIEW visible AS SELECT id FROM item")
        let mainColumns = try child.query("SELECT table_name FROM jed_columns ORDER BY table_name").collect()
        XCTAssertEqual(try mainColumns.map(SpillRowCodec.encode), try ["item", "visible"].map {
            try SpillRowCodec.encode([.text(SQLText($0))])
        })
        try child.close()

        let database = try Database(); defer { try? database.close() }
        try database.attach(try SQLText("aux"), source: .file(path, OpenOptions(locking: .exclusive)), readOnly: true)
        let columns = try database.query("SELECT table_name FROM aux.jed_columns ORDER BY table_name").collect()
        XCTAssertEqual(try columns.map(SpillRowCodec.encode), [try SpillRowCodec.encode([.text(SQLText("item"))])])
        let views = try database.query("SELECT name FROM aux.jed_views ORDER BY name").collect()
        XCTAssertEqual(try views.map(SpillRowCodec.encode), [try SpillRowCodec.encode([.text(SQLText("visible"))])])
    }
}
#endif
