import XCTest
import Foundation
import Dispatch
import Synchronization
@testable import Jed

final class PageServiceRoleTests: XCTestCase {
    private func storage(_ catalog: UInt64) throws -> StorageDomain {
        try StorageDomain.create(store: MemoryBlockStore(), kind: .file, catalogID: catalog, pageSize: 1024)
    }
    func testOldRolePinsAndWriterCannotAffectSameTxidInNewRole() throws {
        let original = try storage(1201), core = DatabaseCore(storage: original)
        let oldPin = try core.beginRead(), oldWriter = try core.acquireWriter()
        let replacement = try storage(1201)
        core.installRole(storage: replacement, remote: nil)
        let newPin = try core.beginRead(), newWriter = try core.acquireWriter(wait: false)
        XCTAssertEqual(oldPin.snapshot.txid, newPin.snapshot.txid)
        XCTAssertEqual(core.livePinCount, 1)
        XCTAssertThrowsError(try oldPin.checkRole()) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
        XCTAssertThrowsError(try oldPin.retain()) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
        XCTAssertThrowsError(try oldWriter.prepare(oldWriter.snapshot)) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
        XCTAssertThrowsError(try original.pager.readStored(2)) { XCTAssertEqual(($0 as? JedError)?.code, "08006") }
        oldPin.close(); oldWriter.close()
        XCTAssertEqual(core.livePinCount, 1)
        XCTAssertThrowsError(try core.acquireWriter(wait: false)) { XCTAssertEqual(($0 as? JedError)?.code, "55006") }
        newWriter.close(); newPin.close()
        XCTAssertEqual(core.livePinCount, 0)
    }
    func testRoleReplacementWaitsOnlyForActiveStoragePublication() throws {
        let core = DatabaseCore(storage: try storage(1202)), replacement = try storage(1202)
        let persisted = DispatchSemaphore(value: 0), release = DispatchSemaphore(value: 0)
        let attempted = DispatchSemaphore(value: 0), replaced = DispatchSemaphore(value: 0), finished = DispatchSemaphore(value: 0)
        let failure = Mutex<JedError?>(nil)
        Thread.detachNewThread {
            defer { finished.signal() }
            do {
                let writer = try core.acquireWriter(); defer { writer.close() }
                var snapshot = writer.snapshot, image = snapshot.catalog
                image.sequences.append(CatalogSequence(name: try SQLText("old_role")))
                try snapshot.updateCatalog(image, schemaChanged: true)
                guard let plan = try writer.prepare(snapshot) else { throw JedError("XX000", "missing test commit") }
                try writer.persist(plan, afterPersist: { persisted.signal(); release.wait() })
            } catch { failure.withLock { $0 = error as? JedError ?? JedError("XX000", "test worker failed") } }
        }
        XCTAssertEqual(persisted.wait(timeout: .now() + 2), .success)
        Thread.detachNewThread {
            attempted.signal(); core.installRole(storage: replacement, remote: nil); replaced.signal()
        }
        XCTAssertEqual(attempted.wait(timeout: .now() + 2), .success)
        XCTAssertEqual(replaced.wait(timeout: .now() + 0.02), .timedOut)
        release.signal()
        XCTAssertEqual(finished.wait(timeout: .now() + 2), .success)
        XCTAssertEqual(replaced.wait(timeout: .now() + 2), .success)
        XCTAssertNil(failure.withLock { $0 })
        XCTAssertTrue(core.storage === replacement)
        XCTAssertEqual(core.version, 1)
    }
}
