#if os(Linux) || canImport(Darwin)
import Foundation
import XCTest
@testable import Jed
#if os(Linux)
import Glibc
#else
import Darwin
#endif

final class PageServiceClaimTests: XCTestCase {
    func testExternalClaimSurvivesClientBundleCloseAndOwnerRecordBelongsToPresenceHolder() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-claim-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: false)
        defer { try? FileManager.default.removeItem(at: directory) }
        let path = directory.appendingPathComponent("db.jed").path
        let claim = try FilePathClaim.acquire(path); defer { claim.close() }
        let owner = try FileLockBundle.acquireExclusive(path: path, timeoutMilliseconds: 0, claim: claim)
        let record = try XCTUnwrap(FileLockBundle.readOwner(path))
        XCTAssertFalse(record.serving)
        // Distinct open descriptions exercise the kernel's ownership decision,
        // while one external claim models the same handle changing roles.
        let candidate = try FileLockBundle.prepare(path: path, timeoutMilliseconds: 0, claim: claim)
        XCTAssertFalse(try candidate.tryOwnership()); candidate.close()
        XCTAssertEqual(FileLockBundle.readOwner(path)?.incarnation, record.incarnation)
        XCTAssertThrowsError(try FilePathClaim.acquire(path)) { XCTAssertEqual(($0 as? JedError)?.code, "55006") }
        owner.close()
        XCTAssertNil(FileLockBundle.readOwner(path))
        XCTAssertThrowsError(try FilePathClaim.acquire(path))
        let promoted = try FileLockBundle.acquireExclusive(path: path, timeoutMilliseconds: 0, claim: claim)
        promoted.close(); claim.close()
        let next = try FilePathClaim.acquire(path); next.close()
    }
}
#endif
