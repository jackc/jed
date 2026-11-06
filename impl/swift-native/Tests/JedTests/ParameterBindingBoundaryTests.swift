import XCTest
@testable import Jed

final class ParameterBindingBoundaryTests: XCTestCase {
    func testHostAdmissionUsesStorageFamiliesBeforeExpressionEvaluation() throws {
        func bind(_ value: SQLValue, _ type: SQLType) throws -> SQLValue {
            try ParameterBinding.bind([value], types: [TypeDescriptor(type)], timezone: .utc)[0]
        }
        for value: SQLValue in [.text(try SQLText("2")), .decimal(try SQLDecimal.parse("2.0")), .f64(2.0.bitPattern), .boolean(true)] {
            XCTAssertThrowsError(try bind(value, .scalar(.i64))) { XCTAssertEqual(($0 as? JedError)?.code, "42804") }
        }
        guard case .decimal(let exact) = try bind(.integer(2), .scalar(.decimal)) else { return XCTFail("integer must widen to exact decimal") }
        XCTAssertEqual(exact, SQLDecimal(2))
        guard case .f32(let narrow) = try bind(.decimal(SQLDecimal.parse("1.5")), .scalar(.f32)) else { return XCTFail("exact numeric must adapt at floating width") }
        XCTAssertEqual(narrow, Float(1.5).bitPattern)
        guard case .f64(let wide) = try bind(.f32(Float(1.5).bitPattern), .scalar(.f64)) else { return XCTFail("lossless host float widening expected") }
        XCTAssertEqual(wide, Double(1.5).bitPattern)
        XCTAssertThrowsError(try bind(.f64(Double(1.5).bitPattern), .scalar(.f32))) { XCTAssertEqual(($0 as? JedError)?.code, "42804") }
        XCTAssertThrowsError(try bind(.timestamp(0), .scalar(.date))) { XCTAssertEqual(($0 as? JedError)?.code, "42804") }
        guard case .uuid = try bind(.text(SQLText("00000000-0000-0000-0000-000000000000")), .scalar(.uuid)) else { return XCTFail("documented text input family expected") }
        XCTAssertThrowsError(try bind(.integer(32_768), .scalar(.i16))) { XCTAssertEqual(($0 as? JedError)?.code, "22003") }
    }
    func testRecursiveHostAdmissionPreservesShapeAndChecksEveryElement() throws {
        let array = try SQLArray(dimensions: [2], lowerBounds: [-2], elements: [.integer(1), .integer(2)])
        let values = try ParameterBinding.bind([.array(array)], types: [TypeDescriptor(.array(.scalar(.decimal)))], timezone: .utc)
        guard case .array(let converted) = values[0] else { return XCTFail("array expected") }
        XCTAssertEqual(converted.dimensions, array.dimensions); XCTAssertEqual(converted.lowerBounds, array.lowerBounds)
        guard case .decimal(let first) = converted.elements[0] else { return XCTFail("element coercion expected") }
        XCTAssertEqual(first, SQLDecimal(1))
        let wrong = SQLValue.array(try SQLArray(elements: [.integer(1), .text(SQLText("2"))]))
        XCTAssertThrowsError(try ParameterBinding.bind([wrong], types: [TypeDescriptor(.array(.scalar(.i64)))], timezone: .utc)) {
            XCTAssertEqual(($0 as? JedError)?.code, "42804")
        }
        // Text-form containers use their input parser; already-structured host
        // containers use each element's strict family admission above.
        guard case .array(let parsed) = try ParameterBinding.bind([.text(SQLText("{1,2}"))], types: [TypeDescriptor(.array(.scalar(.i64)))], timezone: .utc)[0] else { return XCTFail("array input expected") }
        XCTAssertEqual(parsed.elements.count, 2)
    }
}
