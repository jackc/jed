#if os(Linux) || canImport(Darwin)
import Foundation
import XCTest
@testable import Jed

final class AttachmentReuseBoundaryTests: XCTestCase {
    private func changed(_ source: DomainSnapshot, ordinal: Int) throws -> DomainSnapshot {
        var result = source, catalog = source.catalog
        catalog.sequences.append(CatalogSequence(name: try SQLText("s\(ordinal)")))
        try result.updateCatalog(catalog, schemaChanged: true)
        return result
    }
    func testDirectFileAttachmentPreservesFreePagesUntilRootReaderCloses() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-direct-reuse-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let child = try Database.create(options: CreateOptions(path: directory.appendingPathComponent("a.jed").path,
            pageSize: 1024, locking: .exclusive))
        let database = try Database(pageSize: 1024)
        defer { try? database.close(); try? child.close() }
        for ordinal in 0..<3 {
            let writer = try child.core.acquireWriter()
            defer { writer.close() }
            let working = try changed(writer.snapshot, ordinal: ordinal)
            let plan = try XCTUnwrap(child.core.storage.prepare(working, verifyLiveCount: true, forceReclaim: true))
            _ = try writer.persist(plan)
        }
        let name = try SQLText("a")
        try database.domains.attach(name, readOnly: false, main: database) { child }
        let free = try child.core.storage.pager.allocationSnapshot(reuseAllowed: true).persistedFree
        XCTAssertFalse(free.isEmpty)
        let read = try database.domains.read(main: database)
        defer { read.close() }
        func commit(_ ordinal: Int) throws {
            let writer = try database.domains.acquire(main: database, cancellation: nil, timeoutMilliseconds: 0)
            defer { writer.close() }
            var working = writer.initial
            working.snapshots[name] = try changed(working.snapshot(name), ordinal: ordinal)
            try writer.commit(working, through: database.domains, synchronous: true)
        }
        try commit(3)
        XCTAssertTrue(Set(free).isDisjoint(with: child.core.storage.lastWritten))
        XCTAssertEqual(try child.core.storage.pager.allocationSnapshot(reuseAllowed: true).persistedFree, free)
        read.close()
        try commit(4)
        XCTAssertFalse(Set(free).isDisjoint(with: child.core.storage.lastWritten))
    }
}
#endif
