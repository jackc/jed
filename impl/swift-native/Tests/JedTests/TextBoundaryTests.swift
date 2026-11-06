import XCTest
@testable import Jed

final class TextBoundaryTests: XCTestCase {
    private func text(_ value: String) throws -> SQLValue { .text(try SQLText(value)) }
    private func call(_ name: String, _ values: [SQLValue], meter: CostMeter) throws -> SQLValue {
        let types = values.map { value -> TypeDescriptor in
            if case .integer = value { return TypeDescriptor(.scalar(.i64)) }; return TypeDescriptor(.scalar(.text))
        }
        return try TextFunctions.call(name: name, args: values, types: types, constant: true, meter: meter) { value, _, _ in try ScalarCasts.render(value) }
    }
    func testAmplifierMeasuresUTF8AndReservesBeforeAllocation() throws {
        let resources = StatementResources(scalarLimit: 10), meter = CostMeter(resources: resources)
        guard case .text(let padded) = try call("lpad", [text("é"), .integer(3), text("😀")], meter: meter) else { return XCTFail("text expected") }
        XCTAssertEqual(padded.utf8.count, 10)
        XCTAssertEqual(meter.accrued, 16) // 2+4 input bytes, 10 output bytes
        XCTAssertEqual(resources.scalarUsed, 10)
        XCTAssertThrowsError(try call("repeat", [text("x"), .integer(1)], meter: meter)) { XCTAssertEqual(($0 as? JedError)?.code, "54P04") }
        XCTAssertEqual(resources.scalarUsed, 10)
        let bounded = CostMeter(limit: 5, resources: StatementResources(scalarLimit: 1))
        XCTAssertThrowsError(try call("repeat", [text("ab"), .integer(2)], meter: bounded)) { XCTAssertEqual(($0 as? JedError)?.code, "54P01") }
        XCTAssertEqual(bounded.resources.scalarUsed, 0)
    }
    func testAmplifierStructuralLimitsAndNonpositiveInputsAvoidLargeAllocation() throws {
        let meter = CostMeter()
        XCTAssertThrowsError(try call("lpad", [text(""), .integer(Int64.max), text("")], meter: meter)) { XCTAssertEqual(($0 as? JedError)?.code, "54000") }
        XCTAssertEqual(meter.accrued, 0)
        _ = try call("repeat", [text("abc"), .integer(Int64.min)], meter: meter)
        XCTAssertEqual(meter.accrued, 0)
        XCTAssertThrowsError(try call("format", [text("%2147483647s"), text("x")], meter: meter)) { XCTAssertEqual(($0 as? JedError)?.code, "54000") }
        XCTAssertEqual(meter.resources.scalarUsed, 0)
    }
    func testEveryByteRoundTripsEachHandwrittenEncodingAndWrapBoundary() throws {
        let input = (0...255).map(UInt8.init)
        for mode in ["hex", "base64", "escape"] {
            let encoded = ByteaFunctions.encode(input, mode: mode)
            let decoded = try ByteaFunctions.call(name: "decode", args: [.text(encoded), text(mode)], constant: true, meter: CostMeter())
            guard case .bytea(let bytes) = decoded else { return XCTFail("bytea expected") }; XCTAssertEqual(bytes, input)
        }
        for groups in 1...4 {
            let encoded = ByteaFunctions.encode(Array(repeating: 97, count: 57 * groups), mode: "base64")
            XCTAssertEqual(encoded.utf8.count, 77 * groups)
            XCTAssertEqual(encoded.utf8.last, 10)
        }
    }
}
