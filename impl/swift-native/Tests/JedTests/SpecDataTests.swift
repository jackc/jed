import Foundation
import XCTest
@testable import Jed

/// Shared metadata and byte vectors; no duplicated SQL semantic tests.
final class SpecDataTests: XCTestCase {
    private func snapshot() throws -> [String: Any] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        return try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
    }

    private func documents() throws -> [String: [String: Any]] {
        try XCTUnwrap(snapshot()["documents"] as? [String: [String: Any]])
    }

    private func rows(_ path: String, _ key: String) throws -> [[String: Any]] {
        try XCTUnwrap(documents()[path]?[key] as? [[String: Any]])
    }

    private func specValue(_ value: Any) throws -> SpecDataValue {
        if let text = value as? String { return .text(text) }
        if let number = value as? NSNumber {
            // JSONSerialization represents booleans as CFBoolean/NSNumber. Testing
            // its Objective-C type preserves Bool versus small integer metadata.
            if String(cString: number.objCType) == "c" { return .boolean(number.boolValue) }
            return .integer(number.int64Value)
        }
        if let values = value as? [Any] { return .array(try values.map(specValue)) }
        if let fields = value as? [String: Any] {
            return .table(try fields.mapValues(specValue))
        }
        throw JedError("XX000", "unsupported JSON spec fixture value")
    }

    func testPinnedMetadataAndScalarInventory() throws {
        let fixture = try snapshot()
        XCTAssertEqual(fixture["baseline"] as? String, GeneratedSpec.baseline)
        XCTAssertEqual(fixture["fingerprint"] as? String, GeneratedSpec.fingerprint)
        XCTAssertEqual(fixture["source_hashes"] as? [String: String], GeneratedSpec.sourceHashes)
        XCTAssertEqual(GeneratedSpec.baseline, "b1a6affca7725e0b07a4f734eddb1c569ef76bcf")
        let scalars = try rows("spec/types/scalars.toml", "type")
        XCTAssertEqual(GeneratedSpec.scalars.count, scalars.count)
        XCTAssertEqual(Set(ScalarType.allCases.map(\.name)), Set(GeneratedSpec.scalars.map(\.id)))
        for (fact, row) in zip(GeneratedSpec.scalars, scalars) {
            XCTAssertEqual(fact.id, row["id"] as? String)
            XCTAssertEqual(fact.aliases, row["aliases"] as? [String])
            XCTAssertEqual(fact.family, row["family"] as? String)
            XCTAssertEqual(fact.storable, row["storable"] as? Bool)
            XCTAssertEqual(.table(fact.metadata), try specValue(row))
        }
    }

    func testCompleteCatalogAndErrorRegistry() throws {
        let catalog = try XCTUnwrap(documents()["spec/functions/catalog.toml"])
        let categories = ["operator", "aggregate", "window", "set_returning"]
        var position = 0
        for category in categories {
            let expected = try XCTUnwrap(catalog[category] as? [[String: Any]])
            for row in expected {
                let fact = try XCTUnwrap(GeneratedSpec.functions.indices.contains(position) ? GeneratedSpec.functions[position] : nil)
                XCTAssertEqual(fact.category, category)
                XCTAssertEqual(fact.name, row["name"] as? String)
                XCTAssertEqual(fact.kind, row["kind"] as? String)
                XCTAssertEqual(.table(fact.metadata), try specValue(row))
                position += 1
            }
        }
        XCTAssertEqual(position, GeneratedSpec.functions.count)
        let errors = try rows("spec/errors/registry.toml", "error")
        XCTAssertEqual(errors.count, GeneratedSpec.errors.count)
        XCTAssertEqual(Set(GeneratedSpec.errors.map(\.code)).count, errors.count)
        for (fact, row) in zip(GeneratedSpec.errors, errors) {
            XCTAssertEqual(fact.code, row["code"] as? String)
            XCTAssertEqual(fact.name, row["name"] as? String)
            XCTAssertEqual(fact.errorClass, row["class"] as? String)
            XCTAssertEqual(fact.template, row["template"] as? String)
        }
    }

    func testCostAndMemorySchedule() throws {
        let schedule = try XCTUnwrap(documents()["spec/cost/schedule.toml"])
        let costs = try XCTUnwrap(schedule["unit"] as? [[String: Any]])
        XCTAssertEqual(costs.count, GeneratedSpec.costs.count)
        XCTAssertEqual(Set(GeneratedSpec.costs.map(\.id)).count, costs.count)
        for (fact, row) in zip(GeneratedSpec.costs, costs) {
            XCTAssertEqual(fact.id, row["id"] as? String)
            XCTAssertEqual(fact.weight, (row["weight"] as? NSNumber)?.int64Value)
            XCTAssertEqual(fact.event, row["event"] as? String)
        }
        let memory = try XCTUnwrap(schedule["memory"] as? [String: NSNumber])
        XCTAssertEqual(GeneratedSpec.memoryRow, memory["row"]?.int64Value)
        XCTAssertEqual(GeneratedSpec.memoryValue, memory["value"]?.int64Value)
        XCTAssertEqual(GeneratedSpec.memoryArrayDimension, memory["array_dim"]?.int64Value)
        XCTAssertEqual(GeneratedSpec.memoryDecimalGroup, memory["decimal_group"]?.int64Value)
        XCTAssertEqual(GeneratedSpec.memoryEntry, memory["entry"]?.int64Value)
        let limits = try XCTUnwrap(schedule["limits"] as? [String: NSNumber])
        XCTAssertEqual(GeneratedSpec.defaultScalarBytes, limits["default_scalar_bytes"]?.int64Value)
        let reclamation = try XCTUnwrap(schedule["reclamation"] as? [String: NSNumber])
        XCTAssertEqual(GeneratedSpec.compactMinimumPages, reclamation["compact_min_pages"]?.int64Value)
        XCTAssertEqual(GeneratedSpec.compactGrowth, reclamation["compact_growth"]?.int64Value)
    }

    func testRangesAndSupplementaryFacts() throws {
        let ranges = try rows("spec/types/ranges.toml", "range")
        XCTAssertEqual(ranges.count, GeneratedSpec.ranges.count)
        for (fact, row) in zip(GeneratedSpec.ranges, ranges) {
            XCTAssertEqual(fact.id, row["id"] as? String)
            XCTAssertEqual(fact.element, row["element"] as? String)
            XCTAssertEqual(fact.aliases, row["aliases"] as? [String])
            XCTAssertEqual(fact.discrete, row["discrete"] as? Bool)
        }
        let expected = try documents()
        let tables: [(String, [String: SpecDataValue])] = [
            ("spec/types/compare.toml", GeneratedSpec.comparison),
            ("spec/types/casts.toml", GeneratedSpec.casts),
            ("spec/cost/estimator.toml", GeneratedSpec.estimator),
            ("spec/types/datetime_units.toml", GeneratedSpec.datetimeUnits),
            ("spec/types/formatting.toml", GeneratedSpec.formatting),
            ("spec/grammar/pg_keywords.toml", GeneratedSpec.keywords),
        ]
        for (path, actual) in tables {
            XCTAssertEqual(.table(actual), try specValue(XCTUnwrap(expected[path])), path)
        }
    }

    private func bytes(_ hex: String) throws -> [UInt8] {
        let digits = Array(hex.utf8)
        guard digits.count.isMultiple(of: 2) else { throw JedError("XX000", "odd fixture hex length") }
        return try stride(from: 0, to: digits.count, by: 2).map { offset in
            try XCTUnwrap(UInt8(String(decoding: digits[offset..<offset + 2], as: UTF8.self), radix: 16))
        }
    }

    func testAllSharedLZ4Vectors() throws {
        let vectors = try rows("spec/fileformat/lz4_vectors.toml", "vector")
        XCTAssertFalse(vectors.isEmpty)
        for row in vectors {
            let name = try XCTUnwrap(row["name"] as? String)
            let input = try bytes(XCTUnwrap(row["input_hex"] as? String))
            let compressed = try bytes(XCTUnwrap(row["compressed_hex"] as? String))
            XCTAssertEqual(LZ4.compress(input), compressed, name)
            XCTAssertEqual(try LZ4.decompress(compressed, rawLength: input.count), input, name)
        }
    }
}
