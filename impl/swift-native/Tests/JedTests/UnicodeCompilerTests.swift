import Foundation
import XCTest
@testable import Jed
import JedUnicodeCompiler

final class UnicodeCompilerTests: XCTestCase {
    private func fixture(_ name: String) throws -> [UInt8] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: nil, subdirectory: "Fixtures"))
        return Array(try Data(contentsOf: url))
    }
    private func source(_ name: String) throws -> String { try XCTUnwrap(String(bytes: fixture(name), encoding: .utf8)) }
    private func vectors(_ name: String, table: String) throws -> [[String: Any]] {
        let root = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(try fixture("SpecSnapshot.json"))) as? [String: Any])
        let documents = try XCTUnwrap(root["documents"] as? [String: [String: Any]])
        return try XCTUnwrap(documents["spec/collation/vectors/\(name).toml"]?[table] as? [[String: Any]])
    }
    private func hex(_ row: [String: Any], _ key: String) throws -> [UInt8] { try ScalarCasts.decodeHex(Array(XCTUnwrap(row[key] as? String).utf8)) }
    private func compile(_ name: String, files: [String]) throws -> [UInt8] {
        try UnicodeCompiler.compile(name: name, definitions: files.map { try source(($0 as NSString).lastPathComponent) })
    }
    func testAllSharedCompilerAndBundleGoldenBytes() throws {
        for row in try vectors("compiler", table: "compiler") {
            let artifact = try compile(XCTUnwrap(row["coll_name"] as? String), files: XCTUnwrap(row["def_files"] as? [String]))
            XCTAssertEqual(artifact, try hex(row, "artifact_hex"))
            XCTAssertEqual(try SQLCollation(artifact: artifact).table.encoded(), try hex(row, "table_hex"))
        }
        for row in try vectors("bundle", table: "bundle") {
            let root = try compile(XCTUnwrap(row["root_name"] as? String), files: XCTUnwrap(row["root_def_files"] as? [String]))
            let names = try XCTUnwrap(row["tailoring_names"] as? [String]), definitions = try XCTUnwrap(row["tailoring_def_files"] as? [String])
            let tailored = try names.indices.map { try compile(names[$0], files: definitions[$0].split(separator: "|").map(String.init)) }
            XCTAssertEqual(try UnicodeCompiler.bundle(root: root, tailorings: tailored, description: XCTUnwrap(row["description"] as? String)), try hex(row, "bundle_hex"))
        }
    }
    func testFrozenProductionSourcesRebuildEveryArtifact() throws {
        let definition = try source("root.allkeys")
        let root = try UnicodeCompiler.compile(name: "unicode", definitions: [definition])
        let spanish = try UnicodeCompiler.compile(name: "es", definitions: [definition, source("es.ldml")])
        XCTAssertEqual(root, try fixture("unicode.coll")); XCTAssertEqual(spanish, try fixture("es.coll"))
        let casing = try UnicodeCompiler.compileCasing(source("casing.txt"))
        let frozen = try fixture("unicode.jucd"), bundle = try UnicodeBundle(frozen)
        XCTAssertEqual(casing.version, bundle.unicodeVersion.description)
        XCTAssertEqual(casing.payload, bundle.sections.first(where: { $0.kind == 0 })?.payload)
        XCTAssertEqual(try UnicodeCompiler.bundle(root: root, tailorings: [spanish], casing: casing), frozen)
        let only = try UnicodeBundle(UnicodeCompiler.bundle(root: nil, casing: casing))
        XCTAssertEqual(only.sections.count, 1); XCTAssertTrue(only.collations.isEmpty)
        XCTAssertEqual(only.properties?.encoded(), casing.payload)
    }
    func testDefinitionOrderCRLFAndUnsupportedWeightBoundaries() throws {
        let entries = ["0061 ; [.1000.0020.0002]", "0062 ; [.2000.0020.0002]", "0063 0068 ; [.1800.0020.0002.10ffff]"]
        let a = try UnicodeCompiler.compile(name: "test", definitions: [entries.joined(separator: "\n")])
        let b = try UnicodeCompiler.compile(name: "test", definitions: [entries.reversed().joined(separator: "\r\n")])
        XCTAssertEqual(a, b)
        let artifact = try UnicodeCompiler.compile(name: "test", definitions: [entries.joined(separator: "\n"), "&a < ä << é <<< É = E"])
        let table = try SQLCollation(artifact: artifact).table
        let last = try XCTUnwrap(table.singles.first(where: { $0.codepoint == 69 }))
        XCTAssertEqual(last.elements, [CollationElement(flags: 0, primary: 0x1400, secondary: 0x40, tertiary: 8)])
        for malformed in ["0061", "D800 ; [.1000.0020.0002]", entries[0] + "\n" + entries[0], entries[0] + "\n&a <<<< b"] {
            XCTAssertThrowsError(try UnicodeCompiler.compile(name: "test", definitions: [malformed])) { XCTAssertEqual(($0 as? JedError)?.code, "42601") }
        }
        XCTAssertThrowsError(try UnicodeCompiler.compile(name: "test", definitions: ["0061 ; [.1000.0020.0002]\n0062 ; [.1001.0020.0002]\n&a < c"])) {
            XCTAssertEqual(($0 as? JedError)?.code, "0A000")
        }
        XCTAssertThrowsError(try UnicodeCompiler.compile(name: "test", definitions: ["0061 ; " + String(repeating: "[.1000.0020.0002]", count: 256)])) {
            XCTAssertEqual(($0 as? JedError)?.code, "54000")
        }
    }
    func testBundleRejectsVersionMismatchAndUnrepresentableDeletion() throws {
        let root = try UnicodeCompiler.compile(name: "root", definitions: ["@version one\n0061 ; [.1000.0020.0002]"])
        let empty = try UnicodeCompiler.compile(name: "empty", definitions: ["@version one"])
        XCTAssertThrowsError(try UnicodeCompiler.bundle(root: root, tailorings: [empty])) { XCTAssertEqual(($0 as? JedError)?.code, "0A000") }
        XCTAssertThrowsError(try UnicodeCompiler.bundle(root: nil, tailorings: [root])) { XCTAssertEqual(($0 as? JedError)?.code, "22023") }
        XCTAssertThrowsError(try UnicodeCompiler.bundle(root: root, tailorings: [root])) { XCTAssertEqual(($0 as? JedError)?.code, "22023") }
        let casing = try UnicodeCompiler.compileCasing("@version two\n0041 ; - ; 0061 ; -")
        XCTAssertThrowsError(try UnicodeCompiler.bundle(root: root, casing: casing)) { XCTAssertEqual(($0 as? JedError)?.code, "22023") }
    }
}
