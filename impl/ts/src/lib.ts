// Public entry point of the TypeScript core (CLAUDE.md §2): a downstream consumer of
// /spec, the canonical source of truth. Runs natively on modern Node via type-stripping
// — no build step. int64 is exact (uniform bigint); the on-disk format is byte-identical
// to the Rust and Go cores (CLAUDE.md §8).

import { Database } from "./executor.ts";
import type { Outcome } from "./executor.ts";
import { parseSQL } from "./parser.ts";

// SUPPORTED_CAPABILITIES lists the capabilities this core implements (spec/conformance:
// the gating axis). The harness runs a corpus file iff every capability in the file's
// `# requires:` header is in this set. Identical to the Rust/Go cores — full parity.
export const SUPPORTED_CAPABILITIES: readonly string[] = [
  // CREATE TABLE with typed columns + single-column PRIMARY KEY.
  "ddl.create_table",
  "ddl.primary_key",
  // DROP TABLE — remove a table (definition + rows) from the catalog (grammar.md §13).
  "ddl.drop_table",
  // INSERT ... VALUES with positional type-checking + overflow trap.
  "dml.insert",
  // Multi-row INSERT ... VALUES (..),(..) — two-phase / all-or-nothing (grammar.md §12).
  "dml.insert_multi_row",
  "error.overflow_trap",
  // Row mutation: UPDATE (in-place) + DELETE.
  "dml.update",
  "dml.delete",
  // SELECT, WHERE (=, ordering), ORDER BY, IS [NOT] NULL, 3VL, casts, cross-type
  // comparison via the promotion tower, and all three integer types.
  "query.select",
  "query.where_eq",
  "query.comparison_order",
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
  // Cost-accounting seam — the harness asserts the deterministic, cross-core-identical
  // accrued cost via the `# cost:` directive (CLAUDE.md §13).
  "resource.cost_metering",
];

// execute parses and executes one SQL statement against db.
export function execute(db: Database, sql: string): Outcome {
  return db.executeStmt(parseSQL(sql));
}

// --- public surface (re-exports) ---
export { Database } from "./executor.ts";
export type { Outcome } from "./executor.ts";
export { parseSQL } from "./parser.ts";
export { EngineError, sqlStateCode } from "./errors.ts";
export type { SqlState } from "./errors.ts";
export { intValue, nullValue, render } from "./value.ts";
export type { ThreeValued, Value } from "./value.ts";
export { loadDatabase, toImage } from "./format.ts";
export type { Statement } from "./ast.ts";
