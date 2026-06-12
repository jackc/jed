// Public entry point of the TypeScript core (CLAUDE.md §2): a downstream consumer of
// /spec, the canonical source of truth. Runs natively on modern Node via type-stripping
// — no build step. int64 is exact (uniform bigint); the on-disk format is byte-identical
// to the Rust and Go cores (CLAUDE.md §8).

import { Database } from "./executor.ts";
import type { Outcome } from "./executor.ts";
import { parseSQL } from "./parser.ts";
import type { Value } from "./value.ts";

// SUPPORTED_CAPABILITIES lists the capabilities this core implements (spec/conformance:
// the gating axis). The harness runs a corpus file iff every capability in the file's
// `# requires:` header is in this set. Identical to the Rust/Go cores — full parity.
export const SUPPORTED_CAPABILITIES: readonly string[] = [
  // CREATE TABLE with typed columns + single-column PRIMARY KEY.
  "ddl.create_table",
  "ddl.primary_key",
  // Table-level PRIMARY KEY (a, b, ...) — composite keys (constraints.md §3).
  "ddl.composite_primary_key",
  "ddl.check",
  // DROP TABLE — remove a table (definition + rows) from the catalog (grammar.md §13).
  "ddl.drop_table",
  // CREATE INDEX / DROP INDEX — non-unique secondary indexes, maintained on every write
  // and used to bound SELECT scans (spec/design/indexes.md, grammar.md §30).
  "ddl.secondary_index",
  "ddl.unique",
  // NOT NULL column constraint — storing NULL traps 23502 (spec/design/constraints.md §1).
  "ddl.not_null",
  // DEFAULT <literal> column constraint, evaluated + coerced at CREATE (constraints.md §2).
  "ddl.column_default",
  // INSERT with an explicit column list + the DEFAULT keyword (grammar.md §12).
  "dml.insert_column_list",
  // INSERT ... VALUES with positional type-checking + overflow trap.
  "dml.insert",
  // Multi-row INSERT ... VALUES (..),(..) — two-phase / all-or-nothing (grammar.md §12).
  "dml.insert_multi_row",
  // INSERT ... SELECT — insert the rows a query produces; up-front arity (42601) +
  // type-assignability (42804) gates, then the same two-phase validation (grammar.md §24).
  "dml.insert_select",
  "error.overflow_trap",
  // Row mutation: UPDATE (in-place) + DELETE.
  "dml.update",
  "dml.delete",
  // The RETURNING clause on INSERT/UPDATE/DELETE — the statement becomes a query result
  // projecting each affected row (grammar.md §32, cost.md §3).
  "dml.returning",
  // SELECT, WHERE (=, ordering), ORDER BY, IS [NOT] NULL, 3VL, casts, cross-type
  // comparison via the promotion tower, and all three integer types.
  "query.select",
  "query.where_eq",
  "query.comparison_order",
  "query.point_lookup",
  "query.limit_short_circuit",
  "query.correlated_pushdown",
  "query.join_pushdown",
  "query.is_null",
  "query.order_by",
  // Richer ORDER BY — multiple keys, per-key ASC/DESC, per-key NULLS FIRST|LAST (grammar.md §10).
  "query.order_by_keys",
  // Select-list output naming: SELECT *, AS aliases, and the ?column? rule (grammar.md §8).
  "query.select_star",
  "query.column_alias",
  // LIMIT / OFFSET row windowing, applied after ORDER BY, before projection (grammar.md §9).
  "query.limit",
  "query.offset",
  // SELECT DISTINCT: deduplicate projected output rows, NULL-safe (grammar.md §11).
  "query.distinct",
  // Phase 4 — multi-table FROM: INNER/CROSS/OUTER JOIN, table aliases, qualified columns
  // (grammar.md §15).
  "query.join_inner",
  "query.cross_join",
  "query.join_left",
  "query.join_right",
  "query.join_full",
  "query.table_alias",
  "query.qualified_column",
  // Scalar aggregates COUNT/SUM/MIN/MAX/AVG over the whole table (spec/design/aggregates.md).
  "query.aggregates",
  // GROUP BY: one row per grouping-key combination + the grouping-error rule + ORDER BY over
  // grouping keys (spec/design/aggregates.md §5-6, grammar.md §18).
  "query.group_by",
  // HAVING: a boolean filter over grouped rows, after aggregation, before ORDER BY
  // (spec/design/aggregates.md §8, grammar.md §19).
  "query.having",
  // Set operations UNION / INTERSECT / EXCEPT (each [ALL]) — spec/design/grammar.md §25.
  "query.union",
  "query.intersect",
  "query.except",
  // Subqueries: scalar / IN / EXISTS, both uncorrelated (folded once) and correlated
  // (re-executed per outer row, any depth) — spec/design/grammar.md §26.
  "query.subquery_scalar",
  "query.subquery_in",
  "query.subquery_exists",
  "query.subquery_correlated",
  // Scalar functions abs / round (per-row, valid anywhere an expression is) —
  // spec/design/functions.md §9.
  "func.abs",
  "func.round",
  "null.three_valued",
  "compare.promotion",
  "cast.explicit",
  "types.int16",
  "types.int32",
  "types.int64",
  // text scalar type (variable-width UTF-8, collation C): storage, literals, and
  // comparison/ordering. Non-key column only this slice (text PRIMARY KEY → 0A000).
  "types.text",
  // Storable boolean column: CREATE/INSERT/SELECT of false/true/NULL, boolean×boolean
  // comparison and ORDER BY. Non-key column only (boolean PRIMARY KEY → 0A000); casts
  // deferred (spec/design/types.md §9).
  "types.boolean_storable",
  // decimal / numeric scalar type — exact base-10, the first parameterized type
  // (numeric(p,s)), comparison/ordering/casts/storage + arithmetic. Non-key column this
  // slice (decimal PRIMARY KEY → 0A000).
  "types.decimal",
  "expr.decimal_arithmetic",
  // bytea scalar type (variable-width raw bytes): storage, hex-input literals, and
  // unsigned-byte comparison/ordering. Non-key column only this slice (bytea PK → 0A000).
  "types.bytea",
  // uuid scalar type (fixed 16-byte RFC 4122): storage, PG-flexible input literals, and
  // unsigned-byte comparison/ordering. The FIRST non-integer type usable as a PRIMARY KEY.
  "types.uuid",
  // timestamp / timestamptz datetime types (int64 microseconds, instant model, no time zone
  // db): storage, literals (offset→UTC for tz), comparison/ordering, infinity, and a
  // timestamp PRIMARY KEY (key encoding = int64). spec/design/timestamp.md.
  "types.timestamp",
  "types.timestamptz",
  // General expression substrate — integer arithmetic, the boolean type, and the
  // AND/OR/NOT Kleene connectives (the `expression` profile).
  "types.boolean",
  "expr.arithmetic",
  "expr.unary_minus",
  "expr.parens",
  "expr.precedence",
  "expr.comparison_value",
  "query.logical_connectives",
  "query.is_distinct_from",
  "error.division_by_zero",
  // Predicate forms (Phase 2, spec/design/grammar.md §20-§23).
  "expr.in_list",
  "expr.between",
  "expr.like",
  "expr.case",
  // Cost-accounting seam — the harness asserts the deterministic, cross-core-identical
  // accrued cost via the `# cost:` directive (CLAUDE.md §13).
  "resource.cost_metering",
  // Cost ceiling — a caller-set `max_cost` aborts a query (54P01) the instant accrued cost
  // reaches it; the `# max_cost:` directive runs a record under a ceiling (cost.md §6).
  "resource.cost_limit",
  // Phase 5 — explicit transactions: BEGIN/COMMIT/ROLLBACK, READ ONLY/READ WRITE access modes,
  // failed-block poisoning (spec/design/transactions.md §4, grammar.md §27).
  "txn.explicit",
  "txn.read_only",
  "txn.failed_state",
];

// execute parses and executes one SQL statement against db (no bind parameters).
export function execute(db: Database, sql: string): Outcome {
  return db.executeStmt(parseSQL(sql));
}

// executeParams parses and executes one SQL statement against db, binding params to its $N
// placeholders (spec/design/api.md §5). A count mismatch is 42601; a parameter whose type cannot
// be inferred is 42P18; a bound value out of range / of the wrong family fails like a literal.
export function executeParams(db: Database, sql: string, params: Value[]): Outcome {
  return db.executeStmtParams(parseSQL(sql), params);
}

// --- public surface (re-exports) ---
export { Database, DEFAULT_PAGE_SIZE, Snapshot } from "./executor.ts";
export type { Outcome } from "./executor.ts";
export { parseSQL } from "./parser.ts";
export { EngineError, sqlStateCode } from "./errors.ts";
export type { SqlState } from "./errors.ts";
export { intValue, nullValue, render } from "./value.ts";
export type { ThreeValued, Value } from "./value.ts";
export { loadDatabase, toImage } from "./format.ts";
export type { Statement } from "./ast.ts";
export { PreparedStatement, Rows, Transaction, prepare, query, querySql, begin, view, update } from "./api.ts";
export { create, open, commit, rollback, close, residentLeaves } from "./file.ts";
export type { DatabaseOptions, OpenOptions } from "./file.ts";
export { ReadHandle, SharedDb, WriteHandle } from "./shared.ts";
