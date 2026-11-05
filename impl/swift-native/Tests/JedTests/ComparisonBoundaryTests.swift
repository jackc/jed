import XCTest
@testable import Jed

final class ComparisonBoundaryTests: XCTestCase {
    /// The runtime total comparator and durable key order are independent
    /// algorithms. Their agreement is an internal invariant beyond SQL output.
    func testRecursiveTotalOrderAgreesWithEncodedKeyOrder() throws {
        let field = TypeField(name: try SQLText("x"), descriptor: TypeDescriptor(.scalar(.i32)))
        let composite = CompositeType(catalogID: 1, id: 1, name: try SQLText("pair"), fields: [field,
            TypeField(name: try SQLText("xs"), descriptor: TypeDescriptor(.array(.scalar(.i32))))])
        let arrays = try [SQLArray(elements: []), SQLArray(elements: [.integer(1)]),
            SQLArray(dimensions: [1], lowerBounds: [0], elements: [.integer(1)]), SQLArray(elements: [.integer(1), .null])]
        let cases: [(SQLType, [SQLValue])] = [
            (.scalar(.f64), [.f64(Double.nan.bitPattern), .f64((-Double.infinity).bitPattern), .f64(Double.infinity.bitPattern), .f64((-0.0).bitPattern), .f64(0.0.bitPattern), .f64((-3.5).bitPattern)]),
            (.scalar(.decimal), [.decimal(try SQLDecimal.parse("1.00")), .decimal(try SQLDecimal.parse("-0.01")), .decimal(try SQLDecimal.parse("0.1")), .decimal(try SQLDecimal.parse("1"))]),
            (.array(.scalar(.i32)), arrays.map(SQLValue.array)),
            (.range(.scalar(.i32)), [.range(SQLRange(empty: true)), .range(SQLRange(lower: nil, upper: .integer(2))), .range(SQLRange(lower: .integer(1), upper: .integer(2))), .range(SQLRange(lower: .integer(1), upper: nil))]),
            (.composite(composite), [.composite([.null, .array(arrays[0])]), .composite([.integer(0), .array(arrays[1])]), .composite([.integer(0), .array(arrays[3])])])
        ]
        for (type, present) in cases {
            let descriptor = TypeDescriptor(type), values = present + [.null]
            for left in values { for right in values {
                let a = try KeyEncoding.encodeSlot(left, type: descriptor), b = try KeyEncoding.encodeSlot(right, type: descriptor)
                let byteOrder = a == b ? 0 : a.lexicographicallyPrecedes(b) ? -1 : 1
                XCTAssertEqual(try ValueComparison.total(left, right, type: descriptor), byteOrder, type.name)
            } }
        }
    }
}
