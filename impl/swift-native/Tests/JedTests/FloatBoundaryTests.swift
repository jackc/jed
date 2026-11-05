import XCTest
@testable import Jed

/// Representation properties supplement the SQL float corpus: both IEEE widths
/// retain their bits through text, including binade and midpoint boundaries.
final class FloatBoundaryTests: XCTestCase {
    private func verify64(_ bits: UInt64, file: StaticString = #filePath, line: UInt = #line) throws {
        let absolute = bits & 0x7fff_ffff_ffff_ffff
        guard absolute < 0x7ff0_0000_0000_0000 else { return }
        let text = try FloatText.render(.f64(bits))
        guard case .f64(let roundtrip) = try FloatText.parse(text, type: .f64) else { return XCTFail("wrong width", file: file, line: line) }
        XCTAssertEqual(roundtrip, bits, text.description, file: file, line: line)
        if absolute == 0 { return }
        let rendered = try SQLDecimal.parse(text.description)
        let positive = SQLDecimal(negative: false, magnitude: rendered.magnitude, scale: rendered.scale)
        let doubled = positive.addingUncapped(positive)
        let value = try BinaryFloatDecimal.exact(bits64: absolute)
        let previous = try BinaryFloatDecimal.exact(bits64: absolute - 1)
        XCTAssertGreaterThan(doubled.compare(to: value.addingUncapped(previous)), 0, text.description, file: file, line: line)
        let next = absolute == 0x7fef_ffff_ffff_ffff ? value.addingUncapped(value).addingUncapped(previous.negated()) : try BinaryFloatDecimal.exact(bits64: absolute + 1)
        XCTAssertLessThan(doubled.compare(to: value.addingUncapped(next)), 0, text.description, file: file, line: line)
    }
    private func verify32(_ bits: UInt32, file: StaticString = #filePath, line: UInt = #line) throws {
        let absolute = bits & 0x7fff_ffff
        guard absolute < 0x7f80_0000 else { return }
        let text = try FloatText.render(.f32(bits))
        guard case .f32(let roundtrip) = try FloatText.parse(text, type: .f32) else { return XCTFail("wrong width", file: file, line: line) }
        XCTAssertEqual(roundtrip, bits, text.description, file: file, line: line)
        if absolute == 0 { return }
        let rendered = try SQLDecimal.parse(text.description)
        let positive = SQLDecimal(negative: false, magnitude: rendered.magnitude, scale: rendered.scale)
        let doubled = positive.addingUncapped(positive)
        let value = try BinaryFloatDecimal.exact(bits32: absolute)
        let previous = try BinaryFloatDecimal.exact(bits32: absolute - 1)
        XCTAssertGreaterThan(doubled.compare(to: value.addingUncapped(previous)), 0, text.description, file: file, line: line)
        let next = absolute == 0x7f7f_ffff ? value.addingUncapped(value).addingUncapped(previous.negated()) : try BinaryFloatDecimal.exact(bits32: absolute + 1)
        XCTAssertLessThan(doubled.compare(to: value.addingUncapped(next)), 0, text.description, file: file, line: line)
    }
    func testFloatTextRoundtripAndStrictIntervals() throws {
        for exponent in [0, 1, 2, 100, 500, 998, 1022, 1023, 1024, 1075, 1500, 2046] {
            let center = UInt64(exponent) << 52
            for bits in [center, center + 1, center | 0x000f_ffff_ffff_ffff] {
                try verify64(bits); try verify64(bits | 0x8000_0000_0000_0000)
            }
        }
        for exponent in [0, 1, 2, 50, 100, 126, 127, 128, 150, 200, 254] {
            let center = UInt32(exponent) << 23
            for bits in [center, center + 1, center | 0x007f_ffff] {
                try verify32(bits); try verify32(bits | 0x8000_0000)
            }
        }
        var state: UInt64 = 0xc265_6ab0_18e3_284f
        for _ in 0..<80 {
            state = state &* 6_364_136_223_846_793_005 &+ 1_442_695_040_888_963_407
            try verify64(state); try verify32(UInt32(truncatingIfNeeded: state >> 32))
        }
    }
    func testBinary32InputDoesNotRoundThroughBinary64() throws {
        for exponent in [1, 50, 100, 126, 127, 128, 200, 253] {
            let lowBits = UInt32(exponent) << 23
            let low = try BinaryFloatDecimal.exact(bits32: lowBits)
            let high = try BinaryFloatDecimal.exact(bits32: lowBits + 1)
            let sum = low.addingUncapped(high)
            let midpoint = SQLDecimal(negative: false, magnitude: sum.magnitude.multiplying(5), scale: sum.scale + 1)
            let epsilon = SQLDecimal(negative: false, magnitude: BigMagnitude(1), scale: max(midpoint.scale + 20, 100))
            let above = midpoint.addingUncapped(epsilon)
            let below = midpoint.addingUncapped(epsilon.negated())
            guard case .f32(let up) = try FloatText.fromDecimal(above, type: .f32),
                  case .f32(let down) = try FloatText.fromDecimal(below, type: .f32),
                  case .f32(let tie) = try FloatText.fromDecimal(midpoint, type: .f32) else { return XCTFail("wrong width") }
            XCTAssertEqual(up, lowBits + 1)
            XCTAssertEqual(down, lowBits)
            XCTAssertEqual(tie, lowBits)
        }
    }
    func testWidthAndSignBitPreservation() throws {
        let zeros: [SQLValue] = [.f32(0), .f32(0x8000_0000), .f64(0), .f64(0x8000_0000_0000_0000)]
        for value in zeros {
            let twice = try FloatFunctions.unary(op: "-", value: FloatFunctions.unary(op: "-", value: value))
            XCTAssertEqual(try FloatText.render(twice), try FloatText.render(value))
            let magnitude = try FloatFunctions.call(name: "abs", args: [value])
            switch (value, magnitude) {
            case (.f32, .f32(let bits)): XCTAssertEqual(bits, 0)
            case (.f64, .f64(let bits)): XCTAssertEqual(bits, 0)
            default: XCTFail("abs changed width")
            }
        }
    }
}
