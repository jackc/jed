import XCTest
@testable import Jed

final class ContainerBoundaryTests: XCTestCase {
    private func composite(_ fields: [TypeField]) throws -> TypeDescriptor {
        TypeDescriptor(.composite(CompositeType(catalogID: 1, id: 1, name: try SQLText("test_record"), fields: fields)))
    }
    func testRecursiveRenderChargesOnlyOuterGrowthAndEveryZoneLookup() throws {
        let arrayType = TypeDescriptor(.array(.scalar(.text)))
        let descriptor = try composite([
            TypeField(name: SQLText("items"), descriptor: arrayType),
            TypeField(name: SQLText("instant"), descriptor: TypeDescriptor(.scalar(.timestamptz)))
        ])
        let value = SQLValue.composite([
            .array(try SQLArray(elements: [.text(SQLText("a\"b\\c")), .null])), .timestamptz(0)
        ])
        let ordinary = CostMeter()
        let text = try ContainerText.render(value, type: descriptor, context: .init(meter: ordinary))
        XCTAssertEqual(ordinary.accrued, 1)
        let grown = CostMeter()
        let copy = try ContainerText.render(value, type: descriptor, context: .init(meter: grown), growth: true)
        XCTAssertEqual(copy, text)
        XCTAssertEqual(grown.accrued, Int64(text.utf8.count) + 1)
        XCTAssertEqual(grown.resources.scalarUsed, 0)
        let denied = CostMeter(limit: Int64(text.utf8.count) + 1)
        XCTAssertThrowsError(try ContainerText.render(value, type: descriptor, context: .init(meter: denied), growth: true)) {
            XCTAssertEqual(($0 as? JedError)?.code, "54P01")
        }
        XCTAssertEqual(denied.resources.scalarUsed, 0)
    }
    func testArrayByteaCastHasOneWholeArrayGrowthCharge() throws {
        let source = TypeDescriptor(.array(.scalar(.bytea))), target = TypeDescriptor(.array(.scalar(.text)))
        let value = SQLValue.array(try SQLArray(dimensions: [3], lowerBounds: [-2], elements: [.bytea([0, 255]), .null, .bytea([])]))
        let meter = CostMeter()
        let output = try ContainerCasts.cast(value, from: source, to: target, context: .init(meter: meter))
        XCTAssertEqual(meter.accrued, MemorySizes.payload(output))
        XCTAssertEqual(meter.resources.scalarUsed, 0)
        let constant = CostMeter()
        _ = try ContainerCasts.cast(value, from: source, to: target, context: .init(meter: constant), growth: false)
        XCTAssertEqual(constant.accrued, 0)
        guard case .array(let array) = output else { return XCTFail("expected array") }
        XCTAssertEqual(array.dimensions, [3]); XCTAssertEqual(array.lowerBounds, [-2])
    }
    func testContainerHostInputHasNoClockAndAssignmentFieldModifiers() throws {
        let context = ScalarCasts.Context(statementMicros: 0)
        XCTAssertThrowsError(try ContainerText.input(SQLText("{today}"), to: TypeDescriptor(.array(.scalar(.date))), context: context)) {
            XCTAssertEqual(($0 as? JedError)?.code, "22007")
        }
        var limited = TypeDescriptor(.scalar(.text)); limited.textLimit = 2
        let descriptor = try composite([TypeField(name: SQLText("name"), descriptor: limited)])
        XCTAssertThrowsError(try ContainerText.input(SQLText("(abc)"), to: descriptor, explicit: true)) {
            XCTAssertEqual(($0 as? JedError)?.code, "22001")
        }
        let invalidInteger = try composite([TypeField(name: SQLText("number"), descriptor: TypeDescriptor(.scalar(.i32)))])
        XCTAssertThrowsError(try ContainerText.input(SQLText("(\"b\\ad\")"), to: invalidInteger)) {
            guard let error = $0 as? JedError else { return XCTFail("expected JedError") }
            XCTAssertEqual(error.code, "22P02"); XCTAssertTrue(error.message.contains("\"bad\""))
        }
    }
    func testHostRecursiveStoreRejectsWrongTagsAndChecksNarrowDomains() throws {
        let type = TypeDescriptor(.array(.scalar(.i16)))
        XCTAssertThrowsError(try ContainerCasts.store(.array(SQLArray(elements: [.integer(32_768)])), to: type)) {
            XCTAssertEqual(($0 as? JedError)?.code, "22003")
        }
        XCTAssertThrowsError(try ContainerCasts.store(.array(SQLArray(elements: [.text(SQLText("1"))])), to: type)) {
            XCTAssertEqual(($0 as? JedError)?.code, "42804")
        }
        let range = TypeDescriptor(.range(.scalar(.i32)))
        XCTAssertThrowsError(try ContainerCasts.store(.range(SQLRange(upper: .integer(Int64.max))), to: range)) {
            XCTAssertEqual(($0 as? JedError)?.code, "22003")
        }
        let nested = SQLText(validatedUTF8: Array(repeating: 123, count: 20_000))
        XCTAssertThrowsError(try ContainerText.input(nested, to: type)) { XCTAssertEqual(($0 as? JedError)?.code, "22P02") }
    }
    func testArrayRepresentationPreservesWireDimensionsAndPrependUpperException() throws {
        let type = TypeDescriptor(.array(.scalar(.i32)))
        for dimensions in [7, 255] {
            let value = SQLValue.array(try SQLArray(dimensions: Array(repeating: 1, count: dimensions),
                                                   lowerBounds: Array(repeating: 1, count: dimensions), elements: [.integer(1)]))
            let encoded = try ValueCodec.encode(value, type: type)
            XCTAssertEqual(encoded[1], UInt8(dimensions))
            let decoded = try ValueCodec.decode(encoded, type: type)
            XCTAssertEqual(try ValueCodec.encode(decoded, type: type), encoded)
        }
        XCTAssertThrowsError(try SQLArray(dimensions: Array(repeating: 1, count: 256),
                                         lowerBounds: Array(repeating: 1, count: 256), elements: [.integer(1)])) {
            XCTAssertEqual(($0 as? JedError)?.code, "54000")
        }
        let value = SQLValue.array(try SQLArray(dimensions: [3], lowerBounds: [Int32.max - 2], elements: [.integer(0), .integer(1), .integer(2)]))
        let bytes = try ValueCodec.encode(value, type: type)
        let decoded = try ValueCodec.decode(bytes, type: type)
        XCTAssertEqual(try ValueCodec.encode(ContainerCasts.store(decoded, to: type), type: type), bytes)
        let cast = try ContainerCasts.cast(decoded, from: type, to: TypeDescriptor(.array(.scalar(.i64))))
        guard case .array(let array) = cast else { return XCTFail("expected array") }
        XCTAssertEqual(array.lowerBounds, [Int32.max - 2]); XCTAssertEqual(array.dimensions, [3])
        XCTAssertThrowsError(try ContainerText.input(SQLText("[2147483645:2147483647]={0,1,2}"), to: type)) {
            XCTAssertEqual(($0 as? JedError)?.code, "54000")
        }
    }
}
