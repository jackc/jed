import Foundation
import XCTest
@testable import Jed

final class CoercionCaptureTests: XCTestCase {
    func testEveryFrozenCoercionSelectionThroughResolver() throws {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        let root = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let documents = try XCTUnwrap(root["documents"] as? [String: [String: Any]])
        let rows = try XCTUnwrap(documents["spec/functions/coercion_snapshot.toml"]?["selects"] as? [String: String])
        XCTAssertEqual(rows.count, 246)
        for signature in rows.keys.sorted(by: { $0.utf8.lexicographicallyPrecedes($1.utf8) }) {
            do {
                let open = try XCTUnwrap(signature.firstIndex(of: "("))
                guard signature.last == ")" else { throw SQLCorpusError("malformed capture signature") }
                let sources = signature[signature.index(after: open)..<signature.index(before: signature.endIndex)]
                    .split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }
                let arguments = try sources.map { source -> String in
                    if source == "integer_literal" { return "1" }
                    if source == "decimal_literal" { return "1.0" }
                    guard let type = ScalarType.allCases.first(where: { $0.name == source }),
                          ["integer", "decimal", "float"].contains(TypeRules.family(type)) else {
                        throw SQLCorpusError("unknown capture source: \(source)")
                    }
                    return "1::" + source
                }
                let sql = String(signature[..<open]) + "(" + arguments.joined(separator: ",") + ")"
                let resolver = ExpressionResolver(), syntax = try SQLParser(sql).parseExpressionOnly()
                guard case .call(let call) = syntax.kind else { throw SQLCorpusError("capture call expected") }
                var actual: String
                do {
                    let function = try FunctionResolver.resolve(call) { try resolver.resolve($0, expected: $1) }
                    actual = function.name + "(" + FunctionResolver.strings(function.metadata.metadata, "arg_families").joined(separator: ", ") + ")"
                    XCTAssertNil(function.hostID, signature)
                } catch let error as JedError { actual = error.code }
                XCTAssertEqual(actual, rows[signature], signature)
            } catch { XCTFail("\(signature): \(error)") }
        }
    }
}
