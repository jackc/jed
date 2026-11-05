import Foundation
import XCTest
@testable import Jed

final class ValueCodecTests: XCTestCase {
    private func documents() throws -> [String: [String: Any]] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        let root = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        return try XCTUnwrap(root["documents"] as? [String: [String: Any]])
    }
    private func hex(_ string: String) -> [UInt8] {
        let chars = Array(string.utf8)
        func digit(_ byte: UInt8) -> UInt8 { byte <= 57 ? byte - 48 : byte - 87 }
        return stride(from: 0, to: chars.count, by: 2).map { digit(chars[$0]) * 16 + digit(chars[$0 + 1]) }
    }
    private func code(_ expected: String, _ operation: () throws -> Void,
                      file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertThrowsError(try operation(), file: file, line: line) {
            XCTAssertEqual(($0 as? JedError)?.code, expected, "\($0)", file: file, line: line)
        }
    }
    private func descriptor(_ name: String) throws -> TypeDescriptor {
        if let scalar = ScalarType.allCases.first(where: { $0.name == name }) { return TypeDescriptor(.scalar(scalar)) }
        if name == "addr" || name.hasPrefix("addr (") {
            return TypeDescriptor(.composite(CompositeType(catalogID: 1, id: 1, name: try SQLText("addr"), fields: [
                TypeField(name: try SQLText("street"), descriptor: TypeDescriptor(.scalar(.text))),
                TypeField(name: try SQLText("zip"), descriptor: TypeDescriptor(.scalar(.i32))),
            ])))
        }
        if name == "line" {
            return TypeDescriptor(.composite(CompositeType(catalogID: 1, id: 2, name: try SQLText("line"), fields: [
                TypeField(name: try SQLText("a"), descriptor: try descriptor("addr")),
                TypeField(name: try SQLText("b"), descriptor: try descriptor("addr")),
            ])))
        }
        if name.hasSuffix("[]") { return TypeDescriptor(.array(try descriptor(String(name.dropLast(2))).type)) }
        let elements = ["i32range": "i32", "i64range": "i64", "numrange": "decimal", "daterange": "date", "tsrange": "timestamp", "tstzrange": "timestamptz"]
        if let element = elements[name] { return TypeDescriptor(.range(try descriptor(element).type)) }
        throw JedError("XX000", "unknown fixture type: \(name)")
    }
    private func scalar(_ raw: Any, type: TypeDescriptor) throws -> SQLValue {
        switch type.type {
        case .scalar(.i16), .scalar(.i32), .scalar(.i64): return .integer(try XCTUnwrap(raw as? NSNumber).int64Value)
        case .scalar(.timestamp): return .timestamp(try XCTUnwrap(raw as? NSNumber).int64Value)
        case .scalar(.timestamptz): return .timestamptz(try XCTUnwrap(raw as? NSNumber).int64Value)
        case .scalar(.date): return .date(try XCTUnwrap(raw as? NSNumber).int32Value)
        case .scalar(.boolean): return .boolean(try XCTUnwrap(raw as? Bool))
        case .scalar(.text): return .text(SQLText(validatedUTF8: Array(try XCTUnwrap(raw as? String).utf8)))
        case .scalar(.bytea): return .bytea(hex(try XCTUnwrap(raw as? String)))
        case .scalar(.uuid): return .uuid(try SQLUUID(bytes: hex(XCTUnwrap(raw as? String).replacingOccurrences(of: "-", with: ""))))
        case .scalar(.decimal): return .decimal(try SQLDecimal.parse(XCTUnwrap(raw as? String)))
        case .scalar(.f64):
            let text = try XCTUnwrap(raw as? String)
            let value = text == "NaN" ? Double.nan : text == "Infinity" ? .infinity : text == "-Infinity" ? -.infinity : try XCTUnwrap(Double(text))
            return .f64(value.bitPattern)
        case .scalar(.f32):
            let text = try XCTUnwrap(raw as? String)
            let value = text == "NaN" ? Float.nan : text == "Infinity" ? .infinity : text == "-Infinity" ? -.infinity : try XCTUnwrap(Float(text))
            return .f32(value.bitPattern)
        default: throw JedError("XX000", "fixture value is not scalar")
        }
    }
    private func value(_ row: [String: Any], type: TypeDescriptor) throws -> SQLValue {
        if row["null"] as? Bool == true { return .null }
        switch type.type {
        case .composite(let composite):
            let fields = try XCTUnwrap((row["fields"] ?? row["composite"]) as? [[String: Any]])
            return .composite(try zip(fields, composite.fields).map { try tagged($0.0, type: $0.1.descriptor) })
        case .range(let element):
            let element = TypeDescriptor(element)
            return .range(SQLRange(empty: row["empty"] as? Bool ?? false,
                lower: try row["lower"].map { try scalar($0, type: element) },
                upper: try row["upper"].map { try scalar($0, type: element) },
                lowerInclusive: row["lower_inc"] as? Bool ?? false, upperInclusive: row["upper_inc"] as? Bool ?? false))
        case .array(let element):
            let elements = try XCTUnwrap(row["elements"] as? [Any])
            let values = try elements.map { raw -> SQLValue in
                if let field = raw as? [String: Any], field["null"] as? Bool == true { return .null }
                return try scalar(raw, type: TypeDescriptor(element))
            }
            return .array(try SQLArray(elements: values))
        case .scalar(.interval):
            return .interval(SQLInterval(months: try XCTUnwrap(row["months"] as? NSNumber).int32Value,
                days: try XCTUnwrap(row["days"] as? NSNumber).int32Value, microseconds: try XCTUnwrap(row["micros"] as? NSNumber).int64Value))
        case .scalar(.bytea): return try scalar(XCTUnwrap(row["hex"]), type: type)
        default: return try scalar(XCTUnwrap(row["value"]), type: type)
        }
    }
    private func tagged(_ row: [String: Any], type: TypeDescriptor) throws -> SQLValue {
        if row["null"] as? Bool == true { return .null }
        if let container = row["array"] as? [String: Any] { return try value(container, type: type) }
        if let container = row["range"] as? [String: Any] { return try value(container, type: type) }
        if row["composite"] != nil { return try value(row, type: type) }
        if let fields = row["interval"] as? [NSNumber] {
            return .interval(SQLInterval(months: fields[0].int32Value, days: fields[1].int32Value, microseconds: fields[2].int64Value))
        }
        return try scalar(XCTUnwrap(row[type.type.name]), type: type)
    }

    func testAllSharedBareNullableAndDescendingKeyVectors() throws {
        let docs = try documents()
        for name in ["integers", "text", "bytea", "decimal", "float", "interval", "range", "composite"] {
            let document = try XCTUnwrap(docs["spec/encoding/\(name).toml"])
            for kind in ["bare", "nullable", "descending"] {
                for group in document[kind] as? [[String: Any]] ?? [] {
                    let type = try descriptor(XCTUnwrap(group["type"] as? String))
                    var previous: [UInt8]?
                    for row in try XCTUnwrap(group["cases"] as? [[String: Any]]) {
                        let input = try value(row, type: type)
                        let actual = kind == "bare" ? try KeyEncoding.encode(input, type: type)
                            : try KeyEncoding.encodeSlot(input, type: type, descending: kind == "descending")
                        XCTAssertEqual(actual, hex(try XCTUnwrap(row["bytes"] as? String)), "\(name)/\(kind): \(row)")
                        if let previous { XCTAssertTrue(previous.lexicographicallyPrecedes(actual), "\(name)/\(kind)") }
                        previous = actual
                    }
                }
            }
        }
    }

    func testAllSharedOrderedSlots() throws {
        let document = try XCTUnwrap(documents()["spec/encoding/ordered.toml"])
        let orders = [("asc_nulls_last", false, false), ("asc_nulls_first", false, true),
                      ("desc_nulls_first", true, true), ("desc_nulls_last", true, false)]
        for group in try XCTUnwrap(document["group"] as? [[String: Any]]) {
            let name = try XCTUnwrap(group["type"] as? String)
            for row in try XCTUnwrap(group["cases"] as? [[String: Any]]) {
                for (field, descending, nullsFirst) in orders {
                    let actual: [UInt8]
                    if name.hasPrefix("collated text") {
                        let fields = row["value"] as? [String: String]
                        actual = KeyEncoding.slot(body: fields?["collated"].map(hex), descending: descending, nullsFirst: nullsFirst)
                    } else {
                        let type = try descriptor(name)
                        let input = row["null"] as? Bool == true ? SQLValue.null : try tagged(XCTUnwrap(row["value"] as? [String: Any]), type: type)
                        actual = try KeyEncoding.encodeSlot(input, type: type, descending: descending, nullsFirst: nullsFirst)
                    }
                    XCTAssertEqual(actual, hex(try XCTUnwrap(row[field] as? String)), "\(name)/\(field): \(row)")
                }
            }
        }
    }

    func testScalarBodiesPreserveBitsAndExactBytes() throws {
        let cases: [(String, SQLValue, String)] = [
            ("i16", .integer(-1), "007fff"), ("i32", .integer(1), "0080000001"),
            ("i64", .integer(Int64.min), "000000000000000000"), ("boolean", .boolean(true), "0001"),
            ("text", .text(try SQLText("é")), "000002c3a9"), ("bytea", .bytea([0, 255]), "00000200ff"),
            ("decimal", .decimal(try SQLDecimal.parse("1.50")), "0000000200010096"),
            ("date", .date(-1), "007fffffff"), ("timestamp", .timestamp(0), "008000000000000000"),
            ("timestamptz", .timestamptz(Int64.max), "00ffffffffffffffff"),
            ("f32", .f32(0x8000_0000), "0080000000"), ("f64", .f64(0x8000_0000_0000_0000), "008000000000000000"),
            ("interval", .interval(SQLInterval(months: -1, days: 2, microseconds: -3)), "00ffffffff00000002fffffffffffffffd"),
        ]
        for (name, value, expected) in cases {
            let type = try descriptor(name), bytes = hex(expected)
            XCTAssertEqual(try ValueCodec.encode(value, type: type), bytes, name)
            XCTAssertEqual(try ValueCodec.encode(ValueCodec.decode(bytes, type: type), type: type), bytes, name)
            for end in bytes.indices { code("XX001") { _ = try ValueCodec.decode(Array(bytes.prefix(end)), type: type) } }
            code("XX001") { _ = try ValueCodec.decode(bytes + [0], type: type) }
            XCTAssertEqual(try ValueCodec.encode(.null, type: type), [1])
        }
        XCTAssertEqual(try ValueCodec.encode(.f32(0xff80_0001), type: descriptor("f32")), hex("007fc00000"))
        XCTAssertEqual(try ValueCodec.encode(.f64(0xfff0_0000_0000_0001), type: descriptor("f64")), hex("007ff8000000000000"))
        XCTAssertEqual(try KeyEncoding.encode(.f64(0x8000_0000_0000_0000), type: descriptor("f64")),
                       try KeyEncoding.encode(.f64(0), type: descriptor("f64")))
    }

    func testRecursiveBodyExamplesAndCorruption() throws {
        let addr = try descriptor("addr")
        let address = SQLValue.composite([.text(try SQLText("Main")), .integer(90210)])
        XCTAssertEqual(try ValueCodec.encode(address, type: addr), hex("000000044d61696e80016062"))
        XCTAssertEqual(try ValueCodec.encode(.composite([.text(try SQLText("Main")), .null]), type: addr), hex("004000044d61696e"))
        let arrayType = try descriptor("i32[]"), array = SQLValue.array(try SQLArray(elements: [.integer(1), .null, .integer(3)]))
        let encoded = hex("0001010000000300000001408000000180000003")
        XCTAssertEqual(try ValueCodec.encode(array, type: arrayType), encoded)
        XCTAssertEqual(try ValueCodec.encode(ValueCodec.decode(encoded, type: arrayType), type: arrayType), encoded)
        for end in encoded.indices { code("XX001") { _ = try ValueCodec.decode(Array(encoded.prefix(end)), type: arrayType) } }
        code("XX001") { _ = try ValueCodec.decode(hex("000101ffffffff00000001"), type: arrayType) }
        code("XX001") { _ = try ValueCodec.decode(hex("000007"), type: arrayType) }
        code("XX001") { _ = try ValueCodec.decode(hex("00ff"), type: addr) }
        let nestedType = TypeDescriptor(.array(addr.type))
        let nested = SQLValue.array(try SQLArray(dimensions: [1, 2], lowerBounds: [-2, 1], elements: [address, .null]))
        let nestedBytes = try ValueCodec.encode(nested, type: nestedType)
        XCTAssertEqual(try ValueCodec.encode(ValueCodec.decode(nestedBytes, type: nestedType), type: nestedType), nestedBytes)
        code("0A000") { _ = try KeyEncoding.encode(nested, type: nestedType) }
        code("XX001") { _ = try ValueCodec.decode([0, 2], type: descriptor("boolean")) }
        code("XX001") { _ = try ValueCodec.decode([0, 0, 1, 0xff], type: descriptor("text")) }
        code("0A000") { _ = try ValueCodec.encode(.bytea(Array(repeating: 0, count: 65_536)), type: descriptor("bytea")) }
    }

    func testJSONBCanonicalBodiesAndMalformedCounts() throws {
        let type = TypeDescriptor(.scalar(.jsonb))
        let node = SQLJSONValue.object([
            SQLJSONMember(key: try SQLText("aa"), value: .number(SQLDecimal(2))),
            SQLJSONMember(key: try SQLText("b"), value: .null),
            SQLJSONMember(key: try SQLText("b"), value: .number(SQLDecimal(1))),
        ])
        let expected = hex("0007020401620300000000010001040261610300000000010002")
        XCTAssertEqual(try ValueCodec.encode(.jsonb(node), type: type), expected)
        XCTAssertEqual(try ValueCodec.encode(ValueCodec.decode(expected, type: type), type: type), expected)
        for bytes in [[UInt8(0), 5], [0, 0x10], [0, 6, 255, 255, 255, 255, 127], [0, 7, 1, 0, 0],
                      [0, 4, 128, 128, 128, 128, 128, 128, 128, 128, 128, 2]] {
            code("XX001") { _ = try ValueCodec.decode(bytes, type: type) }
        }
        var nested = SQLJSONValue.null
        for _ in 0..<256 { nested = .array([nested]) }
        let bytes = try ValueCodec.encode(.jsonb(nested), type: type)
        _ = try ValueCodec.decode(bytes, type: type)
        code("54001") { _ = try ValueCodec.encode(.jsonb(.array([nested])), type: type) }
        code("XX001") { _ = try ValueCodec.decode([0, 6, 1] + bytes.dropFirst(), type: type) }
    }

    func testCanonicalRangeAndCollationBoundaries() throws {
        let type = try descriptor("i32range")
        let range = SQLValue.range(SQLRange(lower: .integer(1), upper: .integer(5)))
        XCTAssertEqual(try ValueCodec.encode(range, type: type), hex("00088000000180000005"))
        code("XX001") { _ = try ValueCodec.decode(hex("00008000000580000001"), type: type) }
        code("XX001") { _ = try ValueCodec.decode(hex("001a80000005"), type: type) }
        code("XX001") { _ = try ValueCodec.decode(hex("0003"), type: type) }
        var collated = try descriptor("text"); collated.collation = try SQLText("dev-root")
        let text = SQLValue.text(try SQLText("a"))
        code("XX002") { _ = try KeyEncoding.encode(text, type: collated) }
        let known = hex("1c4700000020000000020000610001")
        XCTAssertEqual(try KeyEncoding.encode(text, type: collated, collatedKey: { _, _ in known }), known)
        code("0A000") { _ = try KeyEncoding.encode(.jsonb(.null), type: TypeDescriptor(.scalar(.jsonb))) }
    }

    func testRawOverflowPayloadOmitsOnlyTopLevelShortLength() throws {
        let textType = try descriptor("text")
        let value = SQLValue.text(try SQLText(String(repeating: "a", count: 70_000)))
        code("0A000") { _ = try ValueCodec.encode(value, type: textType) }
        let raw = try ValueCodec.encodeRawBody(value, type: textType)
        XCTAssertEqual(raw.count, 70_000)
        XCTAssertEqual(try ValueCodec.encodeRawBody(ValueCodec.decodeRawBody(raw, type: textType), type: textType), raw)
        let decimalType = try descriptor("decimal"), decimal = SQLValue.decimal(try SQLDecimal.parse("1.50"))
        XCTAssertEqual(try ValueCodec.encodeRawBody(decimal, type: decimalType), try ValueCodec.encodeBody(decimal, type: decimalType))
        code("XX001") { _ = try ValueCodec.decodeRawBody([255], type: textType) }
        XCTAssertNil(ValueCodec.fixedWidth(.array(.scalar(.i32))))
        XCTAssertEqual(ValueCodec.fixedWidth(.scalar(.interval)), 16)
    }
}
