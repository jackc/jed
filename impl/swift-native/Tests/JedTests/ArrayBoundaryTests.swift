import XCTest
@testable import Jed

final class ArrayBoundaryTests: XCTestCase {
    private let text = TypeDescriptor(.scalar(.text))
    func testReplacementAdmissionPrecedesConstructionAndCostWins() throws {
        let original = SQLValue.array(try SQLArray(elements: [.text(SQLText("x")), .text(SQLText("x")), .null]))
        let replacement = SQLValue.text(try SQLText(String(repeating: "y", count: 32)))
        let outputSize = GeneratedSpec.memoryArrayDimension + 3 * GeneratedSpec.memoryValue + 64
        let inputSize = MemorySizes.payload(original)
        let resources = StatementResources(scalarLimit: outputSize - 1)
        let rejected = CostMeter(resources: resources)
        XCTAssertThrowsError(try ArrayFunctions.call(name: "array_replace", args: [original, .text(SQLText("x")), replacement],
                                                    element: text, context: .init(meter: rejected))) {
            XCTAssertEqual(($0 as? JedError)?.code, "54P04")
        }
        XCTAssertEqual(rejected.accrued, inputSize + outputSize); XCTAssertEqual(resources.scalarUsed, 0)
        let cost = CostMeter(limit: inputSize + outputSize, resources: resources)
        XCTAssertThrowsError(try ArrayFunctions.call(name: "array_replace", args: [original, .text(SQLText("x")), replacement],
                                                    element: text, context: .init(meter: cost))) {
            XCTAssertEqual(($0 as? JedError)?.code, "54P01")
        }
        XCTAssertEqual(resources.scalarUsed, 0)
        let accepted = CostMeter(resources: StatementResources(scalarLimit: outputSize))
        let output = try ArrayFunctions.call(name: "array_replace", args: [original, .text(SQLText("x")), replacement],
                                            element: text, context: .init(meter: accepted), constant: true)
        XCTAssertEqual(MemorySizes.payload(output), outputSize)
        XCTAssertEqual(accepted.resources.scalarUsed, outputSize)
        XCTAssertEqual(accepted.accrued, inputSize + outputSize)
    }
    func testArrayJoinAmplifierRetainsOnlyAdmittedOutput() throws {
        let original = SQLValue.array(try SQLArray(elements: [.null, .null, .null]))
        let nullString = SQLValue.text(try SQLText(String(repeating: "n", count: 100)))
        let args: [SQLValue] = [original, .text(try SQLText("|")), nullString]
        let meter = CostMeter(resources: StatementResources(scalarLimit: 301))
        XCTAssertThrowsError(try ArrayFunctions.call(name: "array_to_string", args: args, element: text, context: .init(meter: meter))) {
            XCTAssertEqual(($0 as? JedError)?.code, "54P04")
        }
        XCTAssertEqual(meter.accrued, MemorySizes.payload(original) + 302)
        XCTAssertEqual(meter.resources.scalarUsed, 0)
        let accepted = CostMeter(resources: StatementResources(scalarLimit: 302))
        let output = try ArrayFunctions.call(name: "array_to_string", args: args, element: text, context: .init(meter: accepted))
        XCTAssertEqual(MemorySizes.payload(output), 302); XCTAssertEqual(accepted.resources.scalarUsed, 302)
    }
    func testUnnestChargesOnlyRequestedRowsAndClosesOnFailure() throws {
        let value = SQLValue.array(try SQLArray(elements: [.integer(1), .null, .integer(3)]))
        let meter = CostMeter()
        let rows = try ArrayFunctions.Rows(value, meter: meter)
        XCTAssertEqual(meter.accrued, 0)
        XCTAssertNotNil(try rows.next()); XCTAssertEqual(meter.accrued, 1)
        XCTAssertTrue(try XCTUnwrap(rows.next()).isNull); XCTAssertEqual(meter.accrued, 2)
        rows.close(); XCTAssertNil(try rows.next()); XCTAssertEqual(meter.accrued, 2)
        let limited = CostMeter(limit: 1), failing = try ArrayFunctions.Rows(value, meter: limited)
        XCTAssertThrowsError(try failing.next()) { XCTAssertEqual(($0 as? JedError)?.code, "54P01") }
        XCTAssertNil(try failing.next()); XCTAssertEqual(limited.accrued, 1)
        XCTAssertEqual(meter.resources.scalarUsed, 0)
    }
    func testConstructorRetainsBaselineDeepShapeAndChargesGrowth() throws {
        var value = SQLValue.array(try SQLArray(elements: [.integer(1)]))
        for _ in 1..<7 { value = try ArrayFunctions.construct([value], nested: true, constant: true) }
        guard case .array(let array) = value else { return XCTFail("expected array") }
        XCTAssertEqual(array.dimensions, Array(repeating: 1, count: 7))
        let type = TypeDescriptor(.array(.scalar(.i64)))
        let bytes = try ValueCodec.encode(value, type: type)
        XCTAssertEqual(try ValueCodec.encode(ValueCodec.decode(bytes, type: type), type: type), bytes)
        let meter = CostMeter()
        let grown = try ArrayFunctions.construct([value], nested: true, meter: meter)
        XCTAssertEqual(meter.accrued, MemorySizes.payload(grown)); XCTAssertEqual(meter.resources.scalarUsed, 0)
    }
}
