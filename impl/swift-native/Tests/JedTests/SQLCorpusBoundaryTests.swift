import Foundation
import XCTest
@testable import Jed

final class SQLCorpusBoundaryTests: XCTestCase {
    /// Explicit whole-file admission while the SQL pipeline is being connected.
    /// Every selected file's setup, successful query, and expected error runs.
    /// This is a subset report, not a declaration of corpus capability parity.
    static let selectedFiles = [
        "query/integers_basic.test",
        "ddl/not_null.test",
        "ddl/column_default.test",
        "dml/insert_column_list.test",
        "dml/insert_multi_row.test",
        "mutation/delete.test",
        "ddl/create_type.test",
        "ddl/composite_pk.test",
        "transactions/commit.test",
        "transactions/rollback.test",
        "transactions/nested.test",
        "transactions/failed.test",
        "transactions/parse_error_failed.test",
        "transactions/read_only.test",
        "transactions/syntax.test",
        "params/inference.test",
        "types/text.test",
        "types/boolean.test",
        "expr/arithmetic.test",
        "expr/between.test",
        "expr/boolean.test",
        "expr/case.test",
        "expr/like.test",
        "query/order_by.test",
        "query/order_by_nulls_types.test",
        "query/order_by_pk_scan.test",
        "query/comments.test",
        "query/point_lookup.test",
        "expr/at_time_zone.test",
        "expr/sequence_value.test",
        "ddl/sequence.test",
        "ddl/serial.test",
        "ddl/identity.test",
        "ddl/sequence_as_type.test",
        "ddl/alter_sequence.test",
        "dml/insert_select.test",
        "dml/insert_select_container.test",
        "aggregates/count.test",
        "aggregates/sum.test",
        "aggregates/avg.test",
        "aggregates/min_max.test",
        "aggregates/whole_table.test",
        "aggregates/group_by.test",
        "aggregates/having.test",
        "aggregates/distinct.test",
        "json/json_agg.test",
        "json/json_object_agg.test",
        "aggregates/group_by_expr.test",
        "aggregates/group_by_types.test",
        "query/distinct.test",
        "setops/union.test",
        "setops/intersect.test",
        "setops/except.test",
        "setops/types.test",
        "setops/precedence.test",
        "setops/errors.test",
    ]

    func testWholeFrozenSQLFilesThroughDatabaseSession() throws {
        let root = try XCTUnwrap(Bundle.module.resourceURL?.appendingPathComponent("Fixtures/Corpus"))
        let enumerator = try XCTUnwrap(FileManager.default.enumerator(at: root, includingPropertiesForKeys: nil))
        let totalFiles = enumerator.compactMap { $0 as? URL }.filter { $0.pathExtension == "test" }.count
        var records = 0, queries = 0, statements = 0, cells = 0, costs = 0, names = 0, types = 0, failures = 0
        for name in Self.selectedFiles {
            let source = try String(contentsOf: root.appendingPathComponent(name), encoding: .utf8)
            let file = try SQLCorpusFile(source)
            let runner = try SQLCorpusRunner(file: file) { name, ext in
                let path = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: ext, subdirectory: "Fixtures"))
                return Array(try Data(contentsOf: path))
            }
            let report = runner.run(file)
            try runner.close()
            if !report.failures.isEmpty { print("Whole SQL file \(name): \(report.records) records, \(report.failures.count) failures; first: \(report.failures[0])") }
            records += report.records; queries += report.queries; statements += report.statements; cells += report.cells
            costs += report.costs; names += report.names; types += report.types
            for failure in report.failures {
                failures += 1
                if failures <= 40 { XCTFail("\(name):\(failure)") }
            }
        }
        if failures > 40 { XCTFail("\(failures - 40) additional SQL corpus failures omitted") }
        print("Whole SQL corpus: \(Self.selectedFiles.count)/\(totalFiles) complete files, \(records) records (\(statements) statements, \(queries) queries), \(cells) result cells, \(costs) cost / \(names) name / \(types) type assertions; \(totalFiles - Self.selectedFiles.count) files not yet admitted; \(failures) failures")
    }

    func testRecordAdapterRetainsSetupAndConsumesDirectives() throws {
        let file = try SQLCorpusFile("""
        # requires: harness.params
        # lifetime_max_cost: 10
        statement ok
        CREATE TABLE t (id i32)

        # params: 2
        # names: id
        # types: i32
        # cost: 4
        query I rowsort
        SELECT $1 AS id
        ----
        2

        statement error 22003 informational text is not the code
        INSERT INTO t VALUES (2147483648)

        """)
        XCTAssertEqual(file.records.count, 3)
        XCTAssertEqual(file.records[0].directives.map(\.name), ["lifetime_max_cost"])
        XCTAssertEqual(file.records[1].directives.map(\.name), ["params", "names", "types", "cost"])
        XCTAssertTrue(file.records[2].directives.isEmpty)
        guard case .statement(let code) = file.records[2].kind else { return XCTFail("lost expected error") }
        XCTAssertEqual(code, "22003")
        XCTAssertThrowsError(try SQLCorpusFile("# requires: harness.fixture_open\n# upgrade-collations:\n"))
    }

    func testRowSortingPreservesColumnsAndByteIdentity() {
        XCTAssertTrue(SQLCorpusRunner.match(["a", "2", "b", "1"], ["b", "1", "a", "2"], tags: [84, 73], sort: .rowsort))
        XCTAssertFalse(SQLCorpusRunner.match(["a", "2", "b", "1"], ["a", "1", "b", "2"], tags: [84, 73], sort: .rowsort))
        XCTAssertFalse(SQLCorpusRunner.match(["é"], ["e\u{301}"], tags: [84], sort: .nosort))
        XCTAssertTrue(SQLCorpusRunner.match(["1e0", "NaN", "-0"], ["1.0000000001", "NaN", "0"], tags: [82], sort: .nosort))
    }
}
