import Foundation
import XCTest
@testable import Jed

final class SQLCorpusBoundaryTests: XCTestCase {
    /// Explicit whole-file admission while the SQL pipeline is being connected.
    /// Every selected file's setup, successful query, and expected error runs.
    /// This is a subset report, not a declaration of corpus capability parity.
    static let selectedFiles = [
        "dml/merge.test",
        "dml/merge_jed.test",
        "dml/merge_params.test",
        "dml/merge_explain.test",
        "dml/merge_view.test",
        "dml/merge_constraints.test",
        "dml/merge_join_source.test",
        "dml/merge_ambiguous_alias.test",
        "dml/merge_errors.test",
        "dml/merge_hash.test",
        "ddl/create_index.test",
        "ddl/expression_index.test",
        "ddl/partial_index.test",
        "query/index_scan.test",
        "query/index_scan_vartail.test",
        "query/index_prefix.test",
        "query/index_range.test",
        "query/index_mutation.test",
        "query/index_partial_full_scan.test",
        "query/index_partial_implication.test",
        "query/order_by_index_scan.test",
        "query/order_by_index_key_order.test",
        "query/order_by_index_key_order_explain.test",
        "collation/collated_index_desc.test",
        "collation/skew.test",
        "collation/skew_full.test",
        "collation/collation_upgrade.test",
        "collation/collated_index_vartail.test",
        "resource/param_index_limit.test",
        "ddl/create_gin_index.test",
        "ddl/create_gin_jsonb_index.test",
        "ddl/create_gin_multi_column_index.test",
        "ddl/create_index_if_not_exists.test",
        "ddl/index_key_order.test",
        "ddl/alter_column_type_index_gate.test",
        "query/gin_mutation.test",
        "query/gin_scan.test",
        "query/gin_element_types.test",
        "query/gin_text_elements.test",
        "query/gin_text_elements_explain.test",
        "query/gin_decimal_elements.test",
        "query/gin_bytea_elements.test",
        "query/gin_decimal_bytea_explain.test",
        "query/gin_float_elements.test",
        "query/gin_float_elements_explain.test",
        "query/gin_interval_elements.test",
        "query/gin_interval_elements_explain.test",
        "query/gin_any_eq.test",
        "query/gin_array_eq.test",
        "query/gin_jsonb.test",
        "query/gin_jsonb_explain.test",
        "query/gin_jsonb_jsonpath.test",
        "query/gin_jsonb_jsonpath_explain.test",
        "query/gin_skip_scan.test",
        "query/gin_multi_column.test",
        "query/gin_multi_column_explain.test",
        "query/gin_disjunction.test",
        "query/gin_disjunction_explain.test",
        "query/bounded_limit_streaming.test",
        "query/integers_basic.test",
        "ddl/views_recursive.test",
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
        "aggregates/ordered_set.test",
        "aggregates/ordered_set_interval.test",
        "aggregates/ordered_set_array_fraction.test",
        "aggregates/ordered_set_nonconstant_fraction.test",
        "aggregates/ordered_set_collation.test",
        "aggregates/hypothetical_set.test",
        "aggregates/percentile_array_shape.test",
        "aggregates/grouping_sets.test",
        "aggregates/group_by_functional_dependency.test",
        "aggregates/group_by_star.test",
        "aggregates/select_distinct.test",
        "aggregates/min_max_types.test",
        "aggregates/filter.test",
        "aggregates/order_by.test",
        "aggregates/array_agg.test",
        "aggregates/string_agg.test",
        "aggregates/bool_and.test",
        "aggregates/bool_or.test",
        "aggregates/every.test",
        "aggregates/bit_and.test",
        "aggregates/bit_or.test",
        "aggregates/bit_xor.test",
        "aggregates/corr.test",
        "aggregates/covar_pop.test",
        "aggregates/covar_samp.test",
        "aggregates/regr_avgx.test",
        "aggregates/regr_avgy.test",
        "aggregates/regr_count.test",
        "aggregates/regr_intercept.test",
        "aggregates/regr_r2.test",
        "aggregates/regr_slope.test",
        "aggregates/regr_sxx.test",
        "aggregates/regr_sxy.test",
        "aggregates/regr_syy.test",
        "aggregates/stddev.test",
        "aggregates/stddev_pop.test",
        "aggregates/stddev_samp.test",
        "aggregates/var_pop.test",
        "aggregates/var_samp.test",
        "aggregates/variance.test",
        "subquery/scalar.test",
        "subquery/exists.test",
        "subquery/in.test",
        "subquery/quantified.test",
        "subquery/errors.test",
        "subquery/correlated.test",
        "subquery/typed.test",
        "subquery/values_body.test",
        "subquery/row_valued.test",
        "subquery/mutation.test",
        "subquery/correlated_pushdown.test",
        "dml/view_write.test",
        "dml/view_write_jed.test",
        "dml/view_check_option.test",
        "dml/view_check_option_jed.test",
        "ddl/views.test",
        "ddl/views_jed.test",
        "ddl/views_defaults.test",
        "ddl/views_options.test",
        "ddl/views_replace.test",
        "collation/view_replace.test",
        "ddl/foreign_key.test",
        "ddl/foreign_key_cross_type.test",
        "ddl/foreign_key_match_full.test",
        "ddl/foreign_key_child_index.test",
        "ddl/foreign_key_actions.test",
        "ddl/foreign_key_keyed_actions.test",
        "ddl/foreign_key_cross_type_keys.test",
        "ddl/foreign_key_lazy_child.test",
        "ddl/foreign_key_large_child.test",
        "ddl/alter_table_constraints.test",
        "ddl/alter_table.test",
        "ddl/alter_table_add_column.test",
        "ddl/alter_table_add_column_determinism.test",
        "ddl/alter_table_deferred.test",
        "ddl/alter_table_drop_column.test",
        "ddl/alter_table_drop_column_dependencies.test",
        "ddl/alter_table_drop_column_foreign_key.test",
        "ddl/alter_table_drop_column_index.test",
        "ddl/alter_table_identity.test",
        "ddl/alter_table_identity_jed.test",
        "ddl/alter_table_identity_tx.test",
        "ddl/alter_table_scope.test",
        "ddl/alter_table_tx.test",
        "ddl/alter_table_type_oracle.test",
        "ddl/alter_table_type_primary_key.test",
        "aggregates/grouping_sets_window.test",
        "window/aggregate.test",
        "window/aggregate_filter.test",
        "window/base_extend.test",
        "window/expr_keys.test",
        "window/frame.test",
        "window/frame_exclude.test",
        "window/frame_invalid.test",
        "window/frame_range.test",
        "window/frame_range_nulls.test",
        "window/frame_sliding.test",
        "window/group_by.test",
        "window/grouped_order_by.test",
        "window/named.test",
        "window/named_rules.test",
        "window/ntile.test",
        "window/offset.test",
        "window/order_by_key.test",
        "window/ranking.test",
        "window/ratio.test",
        "window/row_number.test",
        "window/topn.test",
        "window/window_collation.test",
        "cte/basic.test",
        "cte/shadowing.test",
        "cte/errors.test",
        "cte/nested.test",
        "cte/nested_correlated.test",
        "cte/recursive.test",
        "cte/recursive_errors.test",
        "cte/search_cycle.test",
        "query/generate_series.test",
        "query/unnest.test",
        "query/unnest_composite.test",
        "query/regexp_srf.test",
        "query/srf_column_names.test",
        "json/json_srf.test",
        "json/json_record.test",
        "json/json_populate.test",
        "json/json_populate_base.test",
        "types/record_unnest_coldef.test",
        "joins/lateral.test",
        "joins/using.test",
        "joins/natural.test",
        "resource/query_memory.test",
        "resource/query_memory_eager.test",
        "resource/query_memory_transient.test",
        "resource/query_memory_state.test",
        "resource/key_metering.test",
        "ddl/exclusion_constraint.test",
        "ddl/exclusion_partial.test",
        "ddl/exclusion_btree.test",
        "ddl/exclusion_jed.test",
        "cast/composite_casts.test",
        "ddl/create_table_as.test",
        "ddl/create_table_as_savepoint.test",
        "ddl/select_into.test",
        "ddl/select_into_jed.test",
        "json/json_table.test",
        "json/json_table_columns.test",
        "json/json_unicode_escapes.test",
        "query/distinct_on.test",
        "introspection/attached.test",
        "introspection/jed_columns.test",
        "introspection/jed_columns_identity.test",
        "introspection/jed_constraints.test",
        "introspection/jed_indexes.test",
        "introspection/jed_sequences.test",
        "introspection/jed_sequences_identity.test",
        "introspection/jed_sequences_txn.test",
        "introspection/jed_tables.test",
        "introspection/jed_types.test",
        "introspection/jed_views.test",
        "introspection/jed_views_alter.test",
        "introspection/jed_views_rename.test",
        "introspection/statistics_scope_p9.test",
        "introspection/temp_scope.test",
        "introspection/temporal_typmod.test",
        "introspection/unicode_escapes_stored.test",
        "session/privileges.test",
        "session/privileges_column_reads.test",
        "session/privileges_positions.test",
        "dml/insert_on_conflict.test",
        "dml/insert_on_conflict_decorations.test",
        "dml/insert_on_conflict_default.test",
        "dml/insert_on_conflict_exclusion.test",
        "dml/insert_on_conflict_exclusion_rekey.test",
        "dml/insert_on_conflict_partial.test",
        "dml/insert_on_conflict_partial_decorated.test",
        "dml/insert_on_conflict_rekey.test",
        "dml/insert_on_conflict_rekey_savepoint.test",
        "dml/view_on_conflict.test",
        "cte/data_modifying.test",
        "cte/data_modifying_errors.test",
        "cte/data_modifying_foreign_key.test",
        "cte/data_modifying_foreign_key_exclusion.test",
        "cte/data_modifying_temp.test",
        "cte/with_dml.test",
        "attach/constraint_names.test",
        "attach/create_table_as.test",
        "attach/foreign_key_qualified.test",
        "attach/in_memory.test",
        "attach/views.test",
        "attach/writable_cte.test",
        "ddl/constraint_name_schema_temp.test",
        "ddl/create_table_as_temp.test",
        "ddl/drop_index_foreign_key_temp.test",
        "ddl/qualified_temp.test",
        "ddl/reindex_jed.test",
        "ddl/reserved_names.test",
        "ddl/temp_composite.test",
        "ddl/temp_foreign_key.test",
        "ddl/temp_index.test",
        "ddl/temp_on_commit.test",
        "ddl/temp_on_commit_foreign_key.test",
        "ddl/temp_serial_identity.test",
        "ddl/temp_table.test",
        "ddl/views_followons_jed.test",
        "ddl/views_temp.test",
        "ddl/views_temp_ctas.test",
        "resource/query_memory_pending.test",
        "resource/storage_memory.test",
        "resource/temp_budget.test",
        "transactions/savepoint.test",
        "transactions/savepoint_jed.test",
        "ddl/reindex.test",
        "ddl/reindex_savepoint.test",
        "ddl/exclusion_reindex.test",
        "query/analyze_oracle.test",
        "query/statistics_estimates_p9.test",
        "query/statistics_gin_terms.test",
        "query/statistics_p9.test",
        "transactions/statistics_p9.test",
        "query/explain.test",
        "query/explain_estimates.test",
        "query/explain_shapes.test",
        "query/explain_followon.test",
        "query/explain_analyze.test",
        "query/explain_formatting.test",
        "dml/explain_dml.test",
        "ddl/explain_create_table_as.test",
    ]

    static func selection(environment: [String: String], frozen: [String]) throws -> [String] {
        let single = environment["JED_SWIFT_SQL_FILE"], multiple = environment["JED_SWIFT_SQL_FILES"]
        guard single == nil || multiple == nil else { throw SQLCorpusError("sql-file and sql-files cannot be combined") }
        let selected = single.map { [$0] } ?? multiple.map { $0.components(separatedBy: ",") }
            ?? (environment["JED_SWIFT_ALL_SQL"] == "1" ? frozen : selectedFiles)
        guard !selected.isEmpty, selected.allSatisfy({ !$0.isEmpty }) else { throw SQLCorpusError("SQL filenames must not be empty") }
        guard Set(selected).count == selected.count else { throw SQLCorpusError("duplicate whole-file selection") }
        let known = Set(frozen), unknown = selected.filter { !known.contains($0) }
        guard unknown.isEmpty else { throw SQLCorpusError("unknown frozen SQL filename(s): " + unknown.joined(separator: ", ")) }
        return selected
    }

    func testExactMultiFileSelectionRejectsSilentOmissions() throws {
        let files = ["query/a.test", "resource/b.test"]
        XCTAssertEqual(try Self.selection(environment: ["JED_SWIFT_SQL_FILES": "resource/b.test,query/a.test"], frozen: files), Array(files.reversed()))
        XCTAssertEqual(try Self.selection(environment: ["JED_SWIFT_SQL_FILE": files[0]], frozen: files), [files[0]])
        XCTAssertEqual(try Self.selection(environment: ["JED_SWIFT_ALL_SQL": "1"], frozen: files), files)
        for value in ["", "query/a.test,", ",query/a.test", "query/a.test,,resource/b.test", "query/missing.test", "query/a.test,query/a.test"] {
            XCTAssertThrowsError(try Self.selection(environment: ["JED_SWIFT_SQL_FILES": value], frozen: files))
        }
        XCTAssertThrowsError(try Self.selection(environment: ["JED_SWIFT_SQL_FILE": files[0], "JED_SWIFT_SQL_FILES": files[1]], frozen: files))
    }

    func testStorageModeSelectionRetainsCanonicalFileExclusions() throws {
        let diskOnly = try SQLCorpusFile("# requires: query.select\n# skip: memory file host required\n")
        let memoryOnly = try SQLCorpusFile("# requires: query.select\n# skip: disk retained session required\n")
        XCTAssertEqual(SQLCorpusRunner.Mode.auto.resolved(for: diskOnly), .disk)
        XCTAssertEqual(SQLCorpusRunner.Mode.auto.resolved(for: memoryOnly), .memory)
        XCTAssertEqual(SQLCorpusRunner.Mode.disk.resolved(for: memoryOnly), .disk)
        XCTAssertTrue(memoryOnly.skips("disk"))
        XCTAssertFalse(memoryOnly.skips("memory"))
    }

    func testWholeFrozenSQLFilesThroughDatabaseSession() throws {
        let root = try XCTUnwrap(Bundle.module.resourceURL?.appendingPathComponent("Fixtures/Corpus"))
        let enumerator = try XCTUnwrap(FileManager.default.enumerator(at: root, includingPropertiesForKeys: nil))
        let frozenFiles = enumerator.compactMap { $0 as? URL }.filter { $0.pathExtension == "test" }
            .map { String($0.path.dropFirst(root.path.count + 1)) }.sorted()
        let totalFiles = frozenFiles.count
        let environment = ProcessInfo.processInfo.environment
        let selected = try Self.selection(environment: environment, frozen: frozenFiles)
        let accounting = try SQLCorpusAccounting(environment: environment)
        let variant = try SQLCorpusVariant(environment: environment)
        var temporarySpills = 0
        guard let mode = SQLCorpusRunner.Mode(rawValue: environment["JED_SWIFT_SQL_MODE"] ?? "auto") else {
            throw SQLCorpusError("SQL storage mode must be auto, memory, or disk")
        }
        try variant.validate(mode: mode)
        let service = try SQLCorpusServiceOwner.configured(environment: environment)
        defer { try? service?.close() }
        if service != nil, mode != .disk { throw SQLCorpusError("service corpus requires disk mode") }
        var serviceClients = 0, serviceFixtures = 0
        var records = 0, queries = 0, statements = 0, cells = 0, costs = 0, names = 0, types = 0, hostActions = 0, failures = 0
        var completedFiles = 0, blockedFiles: [String] = [], abortedFiles: [String] = [], excludedFiles: [String] = []
        for name in selected {
            SQLCorpusTrace.write("SQL file begin: \(name)")
            var phase = "fixture read"
            do {
                let source = try String(contentsOf: root.appendingPathComponent(name), encoding: .utf8)
                let report: SQLCorpusRunner.Report
                if ConcurrencyCorpusFile.format(source) == "concurrency" {
                    if mode == .disk {
                        excludedFiles.append(name + " (#format: concurrency uses the memory schedule)")
                        SQLCorpusTrace.write("SQL file excluded by disk mode: \(name)")
                        continue
                    }
                    phase = "concurrency adapter parse"
                    let file = try ConcurrencyCorpusFile(source)
                    phase = "concurrency runner setup"
                    let runner = try ConcurrencyCorpusRunner(file: file)
                    let schedule = runner.run(file)
                    report = schedule.sql
                    phase = "concurrency runner cleanup"
                    try runner.close()
                    print("Concurrency file \(name): \(schedule.steps) steps, \(schedule.assertions) version/watermark assertions, \(schedule.handoffs) queued writer handoffs")
                } else {
                    phase = "adapter parse"
                    let file = try SQLCorpusFile(source)
                    if accounting.temporaryBuffers != nil {
                        let uppercase = source.uppercased()
                        if !uppercase.contains("CREATE TEMP ") && !uppercase.contains("CREATE TEMPORARY ") {
                            excludedFiles.append(name + " (forced TEMP mode selects CREATE TEMP/TEMPORARY files)")
                            continue
                        }
                    }
                    if variant.encrypted, file.skips("encrypted") {
                        excludedFiles.append(name + " (#skip: encrypted)")
                        SQLCorpusTrace.write("SQL file excluded by encrypted mode: \(name)")
                        continue
                    }
                    let resolvedMode = mode.resolved(for: file)
                    if file.skips(resolvedMode.rawValue) {
                        excludedFiles.append(name + " (#skip: " + resolvedMode.rawValue + ")")
                        SQLCorpusTrace.write("SQL file excluded by \(resolvedMode.rawValue) mode: \(name)")
                        continue
                    }
                    if service != nil, file.environment.contains(where: { $0.name == "fixture" }) {
                        SQLCorpusTrace.write("SQL file service fixture-local: \(name)")
                    }
                    phase = "runner setup"
                    let runner = try SQLCorpusRunner(file: file, mode: resolvedMode, name: name, accounting: accounting, variant: variant, service: service) { name, ext in
                        let path = try XCTUnwrap(Bundle.module.url(forResource: name, withExtension: ext, subdirectory: "Fixtures"))
                        return Array(try Data(contentsOf: path))
                    }
                    report = runner.run(file)
                    temporarySpills += runner.temporarySpills
                    phase = "runner cleanup"
                    try runner.close()
                }
                if !report.failures.isEmpty { print("Whole SQL file \(name): \(report.records) records, \(report.failures.count) failures; first: \(report.failures[0])") }
                serviceClients += report.serviceClientRecords; serviceFixtures += report.serviceFixtureRecords
                records += report.records; queries += report.queries; statements += report.statements; cells += report.cells
                costs += report.costs; names += report.names; types += report.types; hostActions += report.hostActions
                for failure in report.failures {
                    failures += 1
                    if failures > 40 { print("SQL failure \(name):\(failure)") }
                    if failures <= 40 { XCTFail("\(name):\(failure)") }
                }
                if report.completed { completedFiles += 1 }
                else { abortedFiles.append(name) }
                SQLCorpusTrace.write("SQL file end: \(name)")
            } catch {
                let failure = "\(name) (\(phase)): \(error)"
                blockedFiles.append(failure)
                print("SQL adapter blocked file: \(failure)")
                XCTFail("SQL adapter blocked file: \(failure)")
            }
        }
        if let service {
            XCTAssertEqual(service.clientRecords, serviceClients)
            XCTAssertEqual(serviceClients + serviceFixtures, records)
            try service.close()
            print("Service corpus: \(serviceClients) records run on a page client; \(serviceFixtures) canonical fixture-local records")
        }
        if accounting.temporaryBuffers != nil {
            XCTAssertGreaterThan(temporarySpills, 0, "forced TEMP mode did not create any real temp scratch files")
            print("Forced TEMP corpus: \(temporarySpills) native temp scratch files created")
        }
        XCTAssertGreaterThan(selected.count - excludedFiles.count, 0, "storage mode excludes every selected file")
        if failures > 40 { XCTFail("\(failures - 40) additional SQL corpus failures omitted") }
        print("Whole SQL corpus: \(completedFiles)/\(selected.count - excludedFiles.count) selected files completed (\(totalFiles) frozen files), \(records) records (\(statements) statements, \(queries) queries), \(cells) result cells, \(costs) cost / \(names) name / \(types) type assertions, \(hostActions) host actions; \(totalFiles - selected.count) files not selected; \(failures) SQL failures; \(blockedFiles.count) adapter-blocked files, \(abortedFiles.count) files aborted by failed host actions or schedule controls; \(excludedFiles.count) explicit storage-mode exclusions (mode \(mode.rawValue))")
        for name in excludedFiles { print("SQL storage-mode exclusion: \(name)") }
        for failure in blockedFiles { print("SQL adapter blocked file summary: \(failure)") }
        for name in abortedFiles { print("SQL execution aborted file: \(name)") }
    }

    func testRecordAdapterRetainsSetupAndConsumesDirectives() throws {
        let file = try SQLCorpusFile("""
        # requires: harness.params
        # attach: Aux
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
        XCTAssertEqual(file.environment.map(\.value), ["Aux"])
        XCTAssertEqual(file.records[0].directives.map(\.name), ["lifetime_max_cost"])
        XCTAssertEqual(file.records[1].directives.map(\.name), ["params", "names", "types", "cost"])
        XCTAssertTrue(file.records[2].directives.isEmpty)
        guard case .statement(let code) = file.records[2].kind else { return XCTFail("lost expected error") }
        XCTAssertEqual(code, "22003")
        let actions = try SQLCorpusFile("# requires: harness.upgrade_collations\n# cost: 7\n# upgrade-collations:\nstatement ok\nSELECT 1\n\n# upgrade-collations:\n")
        XCTAssertEqual(actions.hostActions.map(\.beforeRecord), [0, 1])
        XCTAssertEqual(actions.hostActions.map { $0.directive.name }, ["upgrade-collations", "upgrade-collations"])
        XCTAssertEqual(actions.records.first?.directives.first?.value, "7")
    }

    func testFailedHostActionReportsUnconsumedFileAndTrailingActionRuns() throws {
        let file = try SQLCorpusFile("""
        # requires: harness.upgrade_collations
        statement ok
        BEGIN READ ONLY

        # upgrade-collations:
        query I nosort
        SELECT 1
        ----
        1

        """)
        let runner = try SQLCorpusRunner(file: file) { _, _ in throw SQLCorpusError("unexpected fixture read") }
        defer { try? runner.close() }
        let report = runner.run(file)
        XCTAssertFalse(report.completed)
        XCTAssertEqual(report.records, 1); XCTAssertEqual(report.hostActions, 1)
        XCTAssertEqual(report.queries, 0); XCTAssertEqual(report.failures.count, 1)
        XCTAssertTrue(report.failures.first?.contains("25006") == true)
        let trailing = try SQLCorpusFile("# requires: harness.upgrade_collations\n# upgrade-collations:\n")
        let empty = try SQLCorpusRunner(file: trailing) { _, _ in throw SQLCorpusError("unexpected fixture read") }
        defer { try? empty.close() }
        let completed = empty.run(trailing)
        XCTAssertTrue(completed.completed); XCTAssertEqual(completed.hostActions, 1)
        XCTAssertEqual(completed.records, 0); XCTAssertTrue(completed.failures.isEmpty)
    }

    func testRowSortingPreservesColumnsAndByteIdentity() {
        XCTAssertTrue(SQLCorpusRunner.match(["a", "2", "b", "1"], ["b", "1", "a", "2"], tags: [84, 73], sort: .rowsort))
        XCTAssertFalse(SQLCorpusRunner.match(["a", "2", "b", "1"], ["a", "1", "b", "2"], tags: [84, 73], sort: .rowsort))
        XCTAssertFalse(SQLCorpusRunner.match(["é"], ["e\u{301}"], tags: [84], sort: .nosort))
        XCTAssertTrue(SQLCorpusRunner.match(["1e0", "NaN", "-0"], ["1.0000000001", "NaN", "0"], tags: [82], sort: .nosort))
    }
}
