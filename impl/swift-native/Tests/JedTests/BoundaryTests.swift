import XCTest
@testable import Jed

final class BoundaryTests: XCTestCase {
    func testStableFloatStorageTypeCodes() {
        XCTAssertEqual(ScalarType.f64.rawValue, 12)
        XCTAssertEqual(ScalarType.f32.rawValue, 13)
    }
    func testTextUsesByteIdentityAndCodePoints() throws {
        let composed = try SQLText("é")
        let decomposed = try SQLText("e\u{301}")
        XCTAssertNotEqual(composed, decomposed)
        XCTAssertEqual(composed.scalarCount, 1)
        XCTAssertEqual(decomposed.scalarCount, 2)
        XCTAssertEqual(Set([composed, decomposed]).count, 2)
        XCTAssertLessThan(try SQLText("\u{e000}"), try SQLText("😀"))
    }

    func testInvalidUTF8AndNULAtHostBoundary() throws {
        for bytes: [UInt8] in [[0], [0xc0, 0x80], [0xed, 0xa0, 0x80], [0xf4, 0x90, 0x80, 0x80], [0xe2, 0x82, 0x27]] {
            XCTAssertThrowsError(try SQLText(utf8: bytes)) { error in
                XCTAssertEqual((error as? JedError)?.code, "22021")
                XCTAssertNil((error as? JedError)?.position)
            }
        }
        XCTAssertEqual(try SQLText("\u{10ffff}").scalarCount, 1)
        XCTAssertThrowsError(try SQLText(utf8: [0xe2, 0x82, 0x27])) { error in
            XCTAssertEqual((error as? JedError)?.message, "invalid byte sequence for encoding \"UTF8\": 0xe2 0x82 0x27")
        }
    }

    func testReaderBoundsAreStructuredAndSlicedOffsetsAreCorrect() throws {
        var writer = ByteWriter()
        writer.u8(99)
        writer.i16(-32768)
        writer.i32(.max)
        writer.i64(.min)
        var reader = ByteReader(writer.bytes[1...])
        XCTAssertEqual(try reader.i16(), .min)
        XCTAssertEqual(try reader.i32(), .max)
        XCTAssertEqual(try reader.i64(), .min)
        try reader.requireEnd()
        XCTAssertThrowsError(try reader.u8()) { XCTAssertEqual(($0 as? JedError)?.code, "XX001") }
        XCTAssertThrowsError(try reader.take(-1))
        XCTAssertThrowsError(try reader.take(Int.max))
    }

    func testFormatChecksumKnownAnswers() {
        let bytes = Array("123456789".utf8)
        XCTAssertEqual(Checksums.crc32(bytes), 0xcbf43926)
        XCTAssertEqual(Checksums.crc64(bytes), 0x6c40df5f0b497347)
        XCTAssertEqual(Checksums.crc32([]), 0)
        XCTAssertEqual(Checksums.crc64([]), 0)
    }

    func testArrayShapeBoundaryCannotOverflow() throws {
        XCTAssertThrowsError(try SQLArray(dimensions: [Int.max], lowerBounds: [1], elements: []))
        XCTAssertThrowsError(try SQLArray(dimensions: [1], lowerBounds: [.max], elements: [.integer(1)]))
        let array = try SQLArray(dimensions: [2], lowerBounds: [-3], elements: [.null, .integer(1)])
        XCTAssertEqual(array.lowerBounds, [-3])
        XCTAssertEqual(try SQLArray(elements: []).dimensions, [])
    }

    func testLZ4RejectsMalformedBlocksWithoutLargeReservation() {
        for (block, length): ([UInt8], Int) in [([], 0), ([0xf0], 100), ([0, 0, 0], 4),
            ([0x10, 97, 2, 0, 0], 5), ([0x10, 97, 1, 0], 5), ([0x20, 97], 2), ([0], Int.max)] {
            XCTAssertThrowsError(try LZ4.decompress(block, rawLength: length)) {
                XCTAssertEqual(($0 as? JedError)?.code, "XX001")
            }
        }
    }
}
