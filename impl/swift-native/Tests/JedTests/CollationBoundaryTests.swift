import Foundation
import XCTest
@testable import Jed

final class CollationBoundaryTests: XCTestCase {
    private func fixture(_ name: String) throws -> [UInt8] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: nil, subdirectory: "Fixtures"))
        return Array(try Data(contentsOf: url))
    }
    private func vectors(_ name: String, table: String) throws -> [[String: Any]] {
        let root = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(try fixture("SpecSnapshot.json"))) as? [String: Any])
        let documents = try XCTUnwrap(root["documents"] as? [String: [String: Any]])
        return try XCTUnwrap(documents["spec/collation/vectors/\(name).toml"]?[table] as? [[String: Any]])
    }
    private func bytes(_ row: [String: Any], _ key: String) throws -> [UInt8] {
        try ScalarCasts.decodeHex(Array(XCTUnwrap(row[key] as? String).utf8))
    }
    private func devCollations() throws -> [SQLCollation] {
        try vectors("compiler", table: "compiler").map { try SQLCollation(artifact: bytes($0, "artifact_hex")) }
    }
    func testSharedCompiledTableAndArtifactBytes() throws {
        for row in try vectors("compiler", table: "compiler") {
            let artifact = try bytes(row, "artifact_hex"), table = try bytes(row, "table_hex")
            let collation = try SQLCollation(artifact: artifact)
            XCTAssertEqual(collation.table.encoded(), table)
            XCTAssertEqual(collation.originalArtifact, artifact)
            XCTAssertEqual(collation.contentHash, Checksums.crc32(table))
            XCTAssertEqual(collation.table, try CollationTable(table))
            for length in [0, 1, 6, 8, 20, artifact.count - 1] {
                XCTAssertThrowsError(try SQLCollation(artifact: Array(artifact.prefix(length)))) { XCTAssertEqual(($0 as? JedError)?.code, "XX001") }
            }
        }
    }
    func testSharedBundleMergeIdentityAndSortKeys() throws {
        let dev = try devCollations()
        for row in try vectors("bundle", table: "bundle") {
            let encoded = try bytes(row, "bundle_hex"), bundle = try UnicodeBundle(encoded)
            XCTAssertEqual(bundle.encoded(), encoded)
            for collation in bundle.collations {
                let expected = try XCTUnwrap(dev.first(where: { $0.name == collation.name }))
                XCTAssertEqual(collation.table.encoded(), expected.table.encoded())
                XCTAssertEqual(collation.contentHash, expected.contentHash)
            }
        }
        let production = try UnicodeBundle(fixture("unicode.jucd"))
        for name in ["unicode", "es"] {
            let full = try SQLCollation(artifact: fixture("\(name).coll"))
            XCTAssertEqual(production.collations.first(where: { $0.name.description == name })?.table, full.table)
        }
        let collations = dev + production.collations
        var previous: [SQLText: [UInt8]] = [:]
        for row in try vectors("sortkey", table: "sortkey") {
            let name = try SQLText(XCTUnwrap(row["coll_name"] as? String)), source = try SQLText(XCTUnwrap(row["string"] as? String))
            let collation = try XCTUnwrap(collations.first(where: { $0.name == name }))
            let key = try collation.sortKey(source)
            XCTAssertEqual(key, try bytes(row, "sortkey_hex"), "\(name) / \(source)")
            if let prior = previous[name] { XCTAssertTrue(prior.lexicographicallyPrecedes(key), "\(name) / \(source)") }
            previous[name] = key
        }
    }
    func testLoadedCasingAndIndependentBareEnvironment() throws {
        let bare = UnicodeDatabase()
        var builder = UnicodeDatabase(); try builder.load(fixture("unicode.jucd"))
        let loaded = builder
        XCTAssertEqual(loaded.properties?.simple.count, 2933)
        XCTAssertEqual(loaded.properties?.special.count, 103)
        XCTAssertEqual(bare.upper(try SQLText("café ß")).description, "CAFé ß")
        XCTAssertEqual(loaded.upper(try SQLText("café ß")).description, "CAFÉ SS")
        XCTAssertEqual(loaded.lower(try SQLText("İ")).utf8, Array("i\u{307}".utf8))
        XCTAssertEqual(loaded.simpleLower(0x130), 0x69)
        XCTAssertEqual(bare.simpleLower(0x130), 0x130)
        let program = try RegexProgram.compile(pattern: SQLText("İ"), options: RegexOptions(ignoreCase: true), fold: { loaded.simpleLower($0) })
        XCTAssertTrue(try program.isMatch(SQLText("i"), meter: CostMeter()))
        XCTAssertTrue(bare.loadedCollations.isEmpty)
        XCTAssertNil(try bare.resolve(SQLText("C")))
        XCTAssertThrowsError(try bare.resolve(SQLText("unicode"))) { XCTAssertEqual(($0 as? JedError)?.code, "42704") }
        let property = try XCTUnwrap(loaded.properties)
        let encoded = property.encoded(), decoded = try UnicodeProperties(encoded, version: property.unicodeVersion)
        XCTAssertEqual(decoded.encoded(), encoded)
        XCTAssertEqual(decoded.simple, property.simple); XCTAssertEqual(decoded.special, property.special)
        try builder.load(fixture("unicode.jucd"))
        XCTAssertEqual(builder.loadedCollations.count, loaded.loadedCollations.count)
        var corrupt = try fixture("unicode.jucd"); corrupt[corrupt.count - 1] ^= 1
        XCTAssertThrowsError(try builder.load(corrupt)) { XCTAssertEqual(($0 as? JedError)?.code, "XX001") }
        XCTAssertEqual(builder.loadedCollations.map(\.contentHash), loaded.loadedCollations.map(\.contentHash))
    }
}
