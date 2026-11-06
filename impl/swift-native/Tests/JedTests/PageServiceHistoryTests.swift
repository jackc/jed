import XCTest
@testable import Jed

final class PageServiceHistoryTests: XCTestCase {
    private func meta(_ txid: UInt64) throws -> ValidatedMeta {
        let manifest = try CommitManifest.build(pageSize: 1024, encrypted: false, txid: txid,
            root: PageReference(2), pageCount: 3, freeList: PageReference(0),
            livePages: 1, entries: [], chain: [])
        return try ValidatedMeta(meta: manifest.meta, slot: 0, floorSlot: 0,
            bytes: manifest.meta.encode(inline: []), entries: [], chain: [])
    }
    func testExactInvalidationWindowAgesLargeSetsAndReset() throws {
        let incarnation = [UInt8](repeating: 1, count: 16)
        var history = try PageServiceHistory(incarnation: incarnation, initial: meta(1))
        history.publish(txid: 2, digest: 2, indices: [3, 7])
        history.publish(txid: 3, digest: 3, indices: [3, 5])
        let answer = history.answer(ServiceCachePoint(incarnation: incarnation, txid: 1), selected: try meta(3))
        XCTAssertEqual(answer.invalidate, 0); XCTAssertEqual(answer.indices, [3, 5, 7])
        XCTAssertEqual(history.answer(.cold, selected: try meta(3)).invalidate, 1)
        history.publish(txid: 4, digest: 4, indices: Array(0...UInt32(PageServiceProtocol.maxInvalidationEntries)))
        XCTAssertEqual(history.answer(ServiceCachePoint(incarnation: incarnation, txid: 3), selected: try meta(4)).invalidate, 1)
        for txid in UInt64(5)...260 { history.publish(txid: txid, digest: txid, indices: [UInt32(txid)]) }
        XCTAssertEqual(history.windowStart, 4)
        let aged = history.answer(ServiceCachePoint(incarnation: incarnation, txid: 4), selected: try meta(260))
        XCTAssertEqual(aged.invalidate, 0); XCTAssertEqual(aged.indices, Array(5...260).map(UInt32.init))
        history.publish(txid: 261, digest: 261, indices: [], reset: true)
        XCTAssertEqual(history.answer(ServiceCachePoint(incarnation: incarnation, txid: 260), selected: try meta(261)).invalidate, 1)
        XCTAssertEqual(history.answer(ServiceCachePoint(incarnation: incarnation, txid: 261), selected: try meta(261)).invalidate, 0)
    }
    func testResolutionSeedsImmediatePredecessorAndBoundsRetention() throws {
        let predecessor = try meta(3), current = try meta(4)
        var history = try PageServiceHistory(incarnation: [UInt8](repeating: 2, count: 16), initial: current, predecessor: predecessor.bytes)
        let digest = try ServiceIncomingDelta.resolutionDigest(predecessor.bytes)
        XCTAssertEqual(history.status(txid: 3, digest: digest, current: 4), 0)
        XCTAssertEqual(history.status(txid: 3, digest: digest ^ 1, current: 4), 1)
        XCTAssertEqual(history.status(txid: 2, digest: 0, current: 4), 2)
        XCTAssertEqual(history.status(txid: 5, digest: 0, current: 4), 1)
        for txid in UInt64(5)...260 { history.publish(txid: txid, digest: txid, indices: []) }
        XCTAssertEqual(history.status(txid: 4, digest: 4, current: 260), 2)
        XCTAssertEqual(history.status(txid: 5, digest: 5, current: 260), 0)
    }
}
