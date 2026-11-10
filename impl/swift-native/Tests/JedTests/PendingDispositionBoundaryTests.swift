import XCTest
@testable import Jed

final class PendingDispositionBoundaryTests: XCTestCase {
    private func noise(_ count: Int) -> [UInt8] {
        var state: UInt32 = 42
        return (0..<count).map { _ in
            state ^= state << 13; state ^= state >> 17; state ^= state << 5
            return UInt8(truncatingIfNeeded: state)
        }
    }
    func testKeptPendingRawAndCompressedChainsSurviveAndRedisposeWithoutPager() throws {
        let type = TypeDescriptor(.scalar(.bytea)), codec = try PlainPageCodec(pageSize: 256)
        for compressed in [false, true] {
            let piece = noise(compressed ? 64 : 80)
            let payload = compressed ? piece + piece + piece + piece : piece
            let other = noise(70)
            let original = try RecordDisposition.plan(key: [1], values: [.value(.bytea(payload)), .value(.bytea(other))],
                types: [type, type], codec: codec)
            XCTAssertEqual(original.cells[0].bytes?.first, compressed ? 4 : 2)
            let pending = try XCTUnwrap(original.overflows[0])
            let keptMeter = CostMeter(), freshMeter = CostMeter()
            let kept = try RecordDisposition.plan(key: [1], values: [.kept(original.cells[0], pending: pending), .value(.bytea(other))],
                types: [type, type], codec: codec, meter: keptMeter)
            _ = try RecordDisposition.plan(key: [1], values: [.value(.bytea(payload)), .value(.bytea(other))],
                types: [type, type], codec: codec, meter: freshMeter)
            XCTAssertEqual(keptMeter.accrued, freshMeter.accrued)
            XCTAssertEqual(kept.cells[0].bytes, original.cells[0].bytes)
            XCTAssertEqual(kept.overflows[0]?.bytes, pending.bytes)
            if case .bytea(let value) = try kept.materialize(column: 0, type: type) { XCTAssertEqual(value, payload) }
            else { XCTFail("wrong kept value") }
            let smaller = try RecordDisposition.plan(key: [1], values: [.kept(kept.cells[0], pending: kept.overflows[0]), .value(.bytea([]))],
                types: [type, type], codec: codec)
            XCTAssertEqual(smaller.cells[0].bytes?.first, compressed ? 3 : 0)
            XCTAssertNil(smaller.overflows[0])
            if case .bytea(let value) = try smaller.materialize(column: 0, type: type) { XCTAssertEqual(value, payload) }
            else { XCTFail("wrong redisposed value") }
            XCTAssertThrowsError(try RecordDisposition.plan(key: [1], values: [.kept(original.cells[0]), .value(.bytea(other))],
                types: [type, type], codec: codec)) { XCTAssertEqual(($0 as? JedError)?.code, "XX001") }
        }
    }
}
