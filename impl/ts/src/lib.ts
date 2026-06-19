// Public entry point of the TypeScript core (CLAUDE.md §2): a downstream consumer of
// /spec, the canonical source of truth. Runs natively on modern Node via type-stripping
// — no build step. i64 is exact (uniform bigint); the on-disk format is byte-identical
// to the Rust and Go cores (CLAUDE.md §8).

import { Database } from "./executor.ts";
import type { Outcome } from "./executor.ts";
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
  // FOREIGN KEY constraints — referential integrity enforced at every write, 23503; referenced
  // columns must be a parent PK/UNIQUE set (42830); persisted (format_version 11)
  // (constraints.md §6, grammar.md §43).
  "ddl.foreign_key",
  // DROP TABLE — remove a table (definition + rows) from the catalog (grammar.md §13).
  "ddl.drop_table",
  // CREATE INDEX / DROP INDEX — non-unique secondary indexes, maintained on every write
  // and used to bound SELECT scans (spec/design/indexes.md, grammar.md §30).
  "ddl.secondary_index",
  "ddl.unique",
  // GIN inverted indexes — CREATE INDEX ... USING gin over an integer-element array column;
  // built + maintained on every write, persisted (format_version 13). The query-side planner
  // bound is query.gin_scan. spec/design/gin.md
  "ddl.gin_index",
  // Composite (row) types — CREATE TYPE / DROP TYPE, persisted (format_version 9); composite
  // columns/values are a later slice (spec/design/composite.md).
  "types.composite",
  "types.array",
  // Array element subscript a[i] — 1-based, OOB/NULL → NULL, non-array base 42804 (array.md §6).
  "expr.array_subscript",
  // Multidimensional array values + custom lower bounds (array.md §12).
  "types.array_multidim",
  // Array slices a[m:n] (array.md §6) — sub-array reads, renumbered to lower bound 1.
  "expr.array_slice",
  // Array-of-composite element types (array.md §12 AC1) — a composite is a first-class array
  // element type (addr[]); the per-element compare routes through the composite total order.
  "types.array_composite",
  // A composite type with an array-typed field (array.md §12 — the mirror nesting) — the catalog
  // composite-type entry gains a code-15 array field; the codec/comparison/text-I/O recurse.
  "types.composite_array_field",
  // Array function/operator surface AF1 (array-functions.md): the polymorphic anyarray/anyelement
  // resolution + introspection (array_ndims/length/lower/upper/cardinality/dims) + builders
  // (array_append/prepend/cat).
  "func.array",
  // Array function surface AF3 (array-functions.md §9): the polymorphic SRF unnest(anyarray) — a
  // FROM-clause row source expanding an array into one row per element (functions.md §10).
  "func.unnest",
  // Array function surface AF4 (array-functions.md §10): the containment/overlap operators
  // @>/<@/&& — polymorphic `anyarray <op> anyarray → boolean`, strict element equality.
  "func.array_containment",
  // Array function surface AF5 (array-functions.md §11): the ANY/ALL/SOME quantified array
  // comparisons (x = ANY(arr), x op ALL(arr)) — the array spelling of IN, three-valued.
  "func.array_quantified",
  // Array function surface AF6 (array-functions.md §12): the VARIADIC call syntax + variadic
  // overload resolution — the num_nulls/num_nonnulls built-ins (spread or VARIADIC-array form).
  "func.variadic",
  // Array function surface AF7 (array-functions.md §13): the whole AF1–AF6 surface over a
  // COMPOSITE element type + unnest(composite[]) — the quantifiers use the composite total order.
  "func.array_composite",
  // Sequences — CREATE SEQUENCE / DROP SEQUENCE, a persisted i64 generator (format_version 12),
  // and the value functions nextval/currval (transactional advance — sequences.md §5).
  "ddl.sequence",
  "func.sequence",
  // serial / bigserial / smallserial CREATE TABLE pseudo-types — an owned sequence + DEFAULT
  // nextval(...) + NOT NULL; DROP TABLE auto-drops it; format_version 13 (sequences.md §12).
  "ddl.serial",
  // NOT NULL column constraint — storing NULL traps 23502 (spec/design/constraints.md §1).
  "ddl.not_null",
  // DEFAULT <literal> column constraint, evaluated + coerced at CREATE (constraints.md §2).
  "ddl.column_default",
  // DEFAULT <expression> column constraint — a non-constant default (e.g. uuidv7(), 1 + 1)
  // evaluated per row at INSERT (spec/design/constraints.md §2).
  "ddl.column_default_expr",
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
  // The old./new. row-version qualifiers in a RETURNING list (PG 18 semantics): old.col =
  // the pre-statement value, new.col = the post-statement value, the absent side the
  // all-NULL row (grammar.md §32).
  "dml.returning_old_new",
  // SELECT, WHERE (=, ordering), ORDER BY, IS [NOT] NULL, 3VL, casts, cross-type
  // comparison via the promotion tower, and all three integer types.
  "query.select",
  "query.where_eq",
  "query.comparison_order",
  "query.point_lookup",
  "query.limit_short_circuit",
  "query.correlated_pushdown",
  "query.join_pushdown",
  // GIN-bounded scan — `col @> const` / `col && const` over a GIN-indexed array column narrows
  // the SELECT scan to candidate rows (term gather → intersect/union → residual filter); the
  // result is identical to the full scan (spec/design/gin.md §6).
  "query.gin_scan",
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
  // FROM-less SELECT: one virtual zero-column row, no scan cost (grammar.md §34).
  "query.select_no_from",
  // Set-returning functions in FROM: generate_series, a computed row source (functions.md §10).
  "query.set_returning",
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
  // Quantified comparison over a subquery — x op ANY/SOME/ALL (SELECT …), the subquery spelling
  // of IN (array-functions.md §11.6).
  "query.subquery_quantified",
  // Non-recursive common table expressions — WITH name [(cols)] AS [NOT] MATERIALIZED (query)
  // [, ...] <query> (spec/design/cte.md).
  "query.cte",
  // Derived tables — FROM ( query_expr ) AS t: a parenthesized subquery as a FROM relation, the
  // parser surface over the CTE inline seam (an anonymous always-inlined single-ref CTE) —
  // spec/design/grammar.md §42.
  "query.derived_table",
  // VALUES-body derived table — FROM (VALUES (e…),(e…)) [AS] v(c…): a parenthesized VALUES list as
  // a FROM relation, a computed relation of literal rows (general constant expressions, per-column
  // type unification across rows) — spec/design/grammar.md §42.
  "query.values",
  // LATERAL joins — a FROM item (LATERAL derived table / VALUES, or an implicitly-lateral table
  // function) whose body / args reference the EARLIER FROM relations, a dependent join re-evaluated
  // per left-hand row — spec/design/grammar.md §44.
  "query.lateral",
  // Scalar functions abs / round (per-row, valid anywhere an expression is) —
  // spec/design/functions.md §9.
  "func.abs",
  "func.round",
  // Named-argument notation + DEFAULT parameter values, via make_interval — functions.md §11.
  "func.named_arguments",
  // Pure uuid extractors (uuid_extract_version/_timestamp) — functions.md §12.
  "func.uuid_extract",
  // Volatile uuid generators (uuidv4/uuidv7) on the entropy+clock seam — entropy.md.
  "func.uuid_generate",
  // Current-time functions on the clock seam — now()/current_timestamp (STABLE) and
  // clock_timestamp() (VOLATILE) — entropy.md §5, functions.md §12.
  "func.now",
  "func.clock_timestamp",
  "null.three_valued",
  "compare.promotion",
  "cast.explicit",
  // Typed string literals — `type 'string'` and CAST(<string literal> AS type) coerce the
  // literal to the named type at resolve (spec/design/grammar.md §36, types.md §5).
  "cast.string_literal",
  // The postfix `::` cast operator — `expr :: type` desugars to CAST(expr AS type), sharing
  // its whole machinery; binds tighter than unary minus (spec/design/grammar.md §37).
  "cast.operator",
  "types.i16",
  "types.i32",
  "types.i64",
  // text scalar type (variable-width UTF-8, collation C): storage, literals, and
  // comparison/ordering. Non-key column only this slice (text PRIMARY KEY → 0A000).
  "types.text",
  // Storable boolean column: CREATE/INSERT/SELECT of false/true/NULL, boolean×boolean
  // comparison and ORDER BY. boolean is also keyable — a boolean PRIMARY KEY / index uses the
  // bool-byte key encoding (the second non-integer key after uuid, encoding.md §2.9); casts
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
  // timestamp / timestamptz datetime types (i64 microseconds, instant model, no time zone
  // db): storage, literals (offset→UTC for tz), comparison/ordering, infinity, and a
  // timestamp PRIMARY KEY (key encoding = i64). spec/design/timestamp.md.
  "types.timestamp",
  "types.timestamptz",
  // interval scalar type (a span — months/days/micros): the "unit + time" input subset, PG
  // render, and comparison/ordering/dedup by the canonical 128-bit span. Non-key column only.
  // spec/design/interval.md.
  "types.interval",
  // f64/f32 (IEEE binary): storage, total order, kernel, casts, canonical-fold
  // SUM/AVG; exempt from cross-core identity for computed/rendered values (R tag). float.md.
  "types.f64",
  "types.f32",
  // date scalar type (a calendar date — i32 days since 1970-01-01): ISO literals, BC era,
  // infinity sentinels, comparison/ordering, a date PRIMARY KEY (key encoding = i32). A
  // strict island — no compare/cast to timestamp this slice. spec/design/date.md.
  "types.date",
  // interval ± interval → interval and unary minus (interval.md §5).
  "expr.interval_arithmetic",
  // interval ×÷ number → interval (the exact field-scaling cascade — interval.md §5).
  "expr.interval_scale",
  // timestamp[tz] ± interval and timestamp[tz] − timestamp[tz] → interval (interval.md §5).
  "expr.timestamp_arithmetic",
  // General expression substrate — integer arithmetic, the boolean type, and the
  // AND/OR/NOT Kleene connectives (the `expression` profile).
  "types.boolean",
  "expr.arithmetic",
  "expr.unary_minus",
  "expr.parens",
  "expr.precedence",
  "expr.comparison_value",
  "expr.not_equal",
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
  // Nesting-depth limit — a fixed MAX_EXPR_DEPTH checked in the parser aborts deeply-nested input
  // with 54001 before it can overflow the native stack (CLAUDE.md §13; cost.md §7).
  "resource.depth_limit",
  // Input-size limit — a per-handle max_sql_length (default 1 MiB, 0 = unlimited) aborts an
  // over-long statement with 54000 at parse entry, before lexing; the `# max_sql_length:`
  // directive runs a record under a small cap (CLAUDE.md §13; cost.md §7, api.md §8).
  "resource.sql_length_limit",
  // Identifier-length limit — a fixed MAX_IDENTIFIER_LENGTH (63 bytes) checked at the lexer's
  // identifier production aborts an over-long name with 42622, on every parse path (cost.md §7).
  "resource.identifier_length_limit",
  // Composite-type nesting-depth limit — a fixed MAX_COMPOSITE_DEPTH (32) bounds the depth of a
  // composite-type chain at the producer: CREATE TYPE rejects an over-deep type with 54001, and a
  // loaded catalog that exceeds it is XX001, keeping every derived recursive walk (codec,
  // comparator, record_out/in, resolveColType) stack-safe (CLAUDE.md §13; cost.md §7b).
  "resource.composite_depth_limit",
  // Pure built-in surface — no function/operator or statement reaches the host (filesystem,
  // network, process, environment) or adds nondeterminism outside the entropy seam; escape-hatch
  // calls are 42883 and escape-hatch statements 42601 (CLAUDE.md §13; functions.md §13).
  "resource.pure_builtins",
  // Phase 5 — explicit transactions: BEGIN/COMMIT/ROLLBACK, READ ONLY/READ WRITE access modes,
  // failed-block poisoning (spec/design/transactions.md §4, grammar.md §27).
  "txn.explicit",
  "txn.read_only",
  "txn.failed_state",
  // Shared-handle concurrency — the SharedDb schedule format (spec/design/concurrency-testing.md
  // §4). Declared because this core implements SharedDb/ReadHandle/WriteHandle + the watermark
  // (shared.ts); a core lacking them skips suites/concurrency files via the capability gate. This
  // core runs the schedule stepped-sequentially only (JS has no shared-memory threads for live
  // objects), which still defines + verifies the canonical, timing-free result (§4.3).
  "txn.shared",
  "txn.read_handle",
  "txn.watermark",
  // Layer 2 — the write-gate `blocks` annotation (spec/design/concurrency-testing.md §5). Declared
  // because this core defers a queued writer-open to the gate-releasing step (the canonical,
  // timing-free result). It has no stepped-threaded mode (JS cannot block one thread — write() while
  // a writer is open throws 25001, shared.ts), so it models the block by queuing rather than truly
  // blocking, which still defines + verifies the canonical result the threaded cores reproduce.
  "txn.gate_blocking",
];

// execute parses and executes one SQL statement against db (no bind parameters).
export function execute(db: Database, sql: string): Outcome {
  return db.executeStmt(db.parse(sql));
}

// executeParams parses and executes one SQL statement against db, binding params to its $N
// placeholders (spec/design/api.md §5). A count mismatch is 42601; a parameter whose type cannot
// be inferred is 42P18; a bound value out of range / of the wrong family fails like a literal.
export function executeParams(db: Database, sql: string, params: Value[]): Outcome {
  return db.executeStmtParams(db.parse(sql), params);
}

// --- public surface (re-exports) ---
export {
  Database,
  DEFAULT_MAX_SQL_LENGTH,
  DEFAULT_PAGE_SIZE,
  Snapshot,
} from "./executor.ts";
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
export { advancingClock, fixedClock, seededRandomSource } from "./seam.ts";
export type { ClockFunc, RandomFill } from "./seam.ts";
