import Foundation
import XCTest
@testable import Jed

final class BlockStoreTests: XCTestCase {
    private func exercise(_ store: any BlockStore) throws {
        XCTAssertEqual(try store.size(), 0)
        try store.setSize(20_000)
        XCTAssertEqual(try store.read(at: 8190, count: 8), [UInt8](repeating: 0, count: 8))
        try store.write(at: 8190, bytes: [1, 2, 3, 4, 5])
        try store.sync()
        XCTAssertEqual(try store.read(at: 8190, count: 8), [1, 2, 3, 4, 5, 0, 0, 0])
        try store.setSize(8193)
        try store.setSize(20_000)
        XCTAssertEqual(try store.read(at: 8190, count: 8), [1, 2, 3, 0, 0, 0, 0, 0])
        XCTAssertThrowsError(try store.read(at: 19_999, count: 2)) { XCTAssertEqual(($0 as? JedError)?.code, "58030") }
        XCTAssertThrowsError(try store.read(at: UInt64.max, count: 1))
        XCTAssertThrowsError(try store.read(at: 0, count: -1))
        XCTAssertThrowsError(try store.setSize(UInt64.max))
        XCTAssertEqual(try store.read(at: 20_000, count: 0), [])
        try store.setSize(0)
        try store.setSize(8192)
        XCTAssertEqual(try store.read(at: 8190, count: 2), [0, 0])
    }

    func testMemoryHostByteContract() throws { try exercise(MemoryBlockStore()) }

    #if os(Linux) || canImport(Darwin)
    func testUnixHostByteContract() throws {
        let path = FileManager.default.temporaryDirectory.appendingPathComponent("jed-swift-host-\(UUID().uuidString)").path
        defer { try? FileManager.default.removeItem(atPath: path) }
        try exercise(UnixBlockStore(path: path, mode: .createExclusive))
        XCTAssertThrowsError(try UnixBlockStore(path: path, mode: .createExclusive))
        let reader = try UnixBlockStore(path: path, mode: .readOnly)
        XCTAssertEqual(try reader.size(), 8192)
        XCTAssertThrowsError(try reader.write(at: 0, bytes: [1]))
    }
    #endif

    func testLogicalPayloadUsesBytesAndTouchedMask() throws {
        let large = SQLValue.text(try SQLText("😀e\u{301}"))
        XCTAssertEqual(MemorySizes.payload(large), 7)
        XCTAssertEqual(MemorySizes.row([large, .integer(1)]), 103)
        XCTAssertEqual(MemorySizes.row([large, .integer(1)], touched: [false]), 96)
        let shaped = try SQLArray(dimensions: [1], lowerBounds: [-3], elements: [large])
        XCTAssertEqual(MemorySizes.payload(.array(shaped)), 55)
        XCTAssertEqual(MemorySizes.payload(.decimal(try SQLDecimal.parse("0.00000"))), 0)
    }
}
