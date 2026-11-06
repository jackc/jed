import Foundation
import XCTest
@testable import Jed

final class EstimatorVectorTests: XCTestCase {
    private func vectors() throws -> [String: Any] {
        let url = try XCTUnwrap(Bundle.module.url(forResource: "SpecSnapshot", withExtension: "json", subdirectory: "Fixtures"))
        let root = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let documents = try XCTUnwrap(root["documents"] as? [String: [String: Any]])
        return try XCTUnwrap(documents["spec/cost/estimator_vectors.toml"])
    }
    private func integer(_ row: [String: Any], _ key: String) throws -> Int64 {
        try XCTUnwrap(row[key] as? NSNumber, key).int64Value
    }
    func testSharedSaturatingArithmetic() throws {
        let rows = try XCTUnwrap(vectors()["arithmetic"] as? [[String: Any]])
        XCTAssertEqual(rows.count, 4)
        for row in rows {
            let a = try integer(row, "a"), b = try integer(row, "b"), actual: Int64
            switch row["op"] as? String {
            case "sat_add": actual = saturatingAdd(a, b)
            case "sat_mul": actual = QueryEstimate.multiply(a, b)
            case "scale_ceil": actual = QueryEstimate.scale(a, numerator: b, denominator: try integer(row, "c"))
            default: throw SQLCorpusError("unknown estimator arithmetic operation")
            }
            XCTAssertEqual(actual, try integer(row, "expected"), row["id"] as? String ?? "")
        }
    }
    func testSharedPredicatePrograms() throws {
        let rows = try XCTUnwrap(vectors()["predicate"] as? [[String: Any]])
        XCTAssertEqual(rows.count, 9)
        let boolean = TypeDescriptor(.scalar(.boolean)), integer = TypeDescriptor(.scalar(.i32))
        let columns = [ExpressionColumn(name: try SQLText("i"), qualifier: nil, type: integer, index: 0),
                       ExpressionColumn(name: try SQLText("j"), qualifier: nil, type: integer, index: 1),
                       ExpressionColumn(name: try SQLText("b"), qualifier: nil, type: boolean, index: 2)]
        let leaves = ["all": "true", "zero": "false", "unique": "i = 1", "equality": "i = 1",
                      "inequality": "j < 10", "paired_range": "j > 1 AND j < 10", "boolean": "b"]
        for row in rows {
            let tokens = try XCTUnwrap(row["tokens"] as? [String]), population = try self.integer(row, "n")
            let resolver = ExpressionResolver(columns: columns)
            var pending: [ResolvedExpression] = []
            for token in tokens {
                switch token {
                case "not":
                    let child = try XCTUnwrap(pending.popLast())
                    pending.append(ResolvedExpression(type: boolean, node: .unary("not", child), position: 0))
                case "and", "or":
                    let right = try XCTUnwrap(pending.popLast()), left = try XCTUnwrap(pending.popLast())
                    pending.append(ResolvedExpression(type: boolean, node: .binary(token, left, right, boolean), position: 0))
                default:
                    let sql = try XCTUnwrap(leaves[token], "unknown estimator leaf: \(token)")
                    pending.append(try resolver.resolve(SQLParser(sql).parseExpressionOnly()))
                }
            }
            XCTAssertEqual(pending.count, 1)
            let table = CatalogTable(name: try SQLText("t"), columns: [
                CatalogColumn(name: try SQLText("i"), type: CatalogTypeUse(.scalar(.i32))),
                CatalogColumn(name: try SQLText("j"), type: CatalogTypeUse(.scalar(.i32))),
                CatalogColumn(name: try SQLText("b"), type: CatalogTypeUse(.scalar(.boolean))),
            ], primaryKey: tokens.contains("unique") ? [0] : [], root: PageReference(0), rowCount: population)
            let model = PlanCardinality(table: table, snapshot: nil)
            XCTAssertEqual(try model.rows(XCTUnwrap(pending.first), input: population),
                           try self.integer(row, "expected"), row["id"] as? String ?? "")
        }
    }
    func testEverySharedCandidateUnitAndTieKey() throws {
        let rows = try XCTUnwrap(vectors()["candidate"] as? [[String: Any]])
        XCTAssertEqual(rows.count, 10)
        for row in rows {
            let kind = try XCTUnwrap(CandidateEstimate.Kind(rawValue: XCTUnwrap(row["kind"] as? String)))
            let actual = CandidateEstimate(kind: kind, indexName: try XCTUnwrap(row["index_name"] as? String),
                scanRows: try integer(row, "scan_rows"), outputRows: try integer(row, "output_rows"),
                accessPages: try integer(row, "access_pages"), tableHeight: try integer(row, "table_height"),
                filterNodes: try integer(row, "filter_nodes"), accessWork: try integer(row, "access_work"),
                producesRows: try XCTUnwrap(row["produces_rows"] as? Bool))
            let label = try XCTUnwrap(row["id"] as? String)
            XCTAssertEqual(actual.rows, try integer(row, "est_rows"), label)
            XCTAssertEqual(actual.cost, try integer(row, "est_cost"), label)
            XCTAssertEqual(actual.tieKey, row["tie_key"] as? String, label)
            let expected = try XCTUnwrap(row["units"] as? [String: NSNumber])
            let known = Set(GeneratedSpec.costs.map(\.id))
            XCTAssertTrue(Set(expected.keys).isSubset(of: known), label)
            XCTAssertTrue(Set(actual.units.keys).isSubset(of: known), label)
            for unit in GeneratedSpec.costs {
                XCTAssertEqual(actual.units[unit.id] ?? 0, expected[unit.id]?.int64Value ?? 0, "\(label)/\(unit.id)")
            }
        }
    }
}
