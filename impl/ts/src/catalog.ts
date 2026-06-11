// Table metadata: column definitions and lookups. Data-shaped (a type + free
// functions) to match the boring/explicit style (CLAUDE.md §10).

import type { Expr } from "./ast.ts";
import type { DecimalTypmod, ScalarType } from "./types.ts";
import type { Value } from "./value.ts";

// Column is a column definition: name, declared type, nullability, primary-key flag, default.
// notNull is implied true for a PRIMARY KEY column.
export type Column = {
  name: string;
  type: ScalarType;
  // The numeric(p,s) typmod for a decimal column, or null for a non-decimal column OR an
  // unconstrained numeric (spec/design/decimal.md §2). A constrained decimal column coerces
  // stored values to this precision/scale.
  decimal: DecimalTypmod | null;
  primaryKey: boolean;
  notNull: boolean;
  // The column's DEFAULT value, pre-evaluated and type-coerced at CREATE TABLE, or null if it
  // has no default. A `{ kind: "null" }` value is an explicit DEFAULT NULL. Applied for an
  // omitted column or a DEFAULT keyword at INSERT (spec/design/constraints.md §2).
  default: Value | null;
};

// Table is a table definition.
export type Table = {
  name: string;
  columns: Column[];
  // The primary-key member column ordinals in KEY order (which may differ from
  // declaration order — constraints.md §3; the v5 catalog persists this list). Empty =
  // no primary key (synthetic rowid keys). The per-column primaryKey flag is derived
  // membership convenience; this list is the authority for order.
  pk: number[];
  // The table's CHECK constraints in EVALUATION ORDER — ascending byte order of the
  // lowercased name (spec/design/constraints.md §4.4); the on-disk catalog stores them in
  // this same order. Empty for an unchecked table.
  checks: CheckConstraint[];
  // The table's secondary indexes in ascending lowercased-name order (the catalog's
  // on-disk order and the planner's tie-break order — spec/design/indexes.md).
  indexes: IndexDef[];
};

// IndexDef is one secondary index of a table (spec/design/indexes.md): its
// (relation-namespace) name and the indexed column ordinals in index-key order
// (duplicates allowed — PG). The index's B-tree lives in the snapshot's index-store map,
// keyed by the lowercased name. A unique index enforces uniqueness over its key tuple
// (NULLS DISTINCT — spec/design/indexes.md §8); it is what backs a UNIQUE constraint
// (spec/design/constraints.md §5).
export type IndexDef = { name: string; columns: number[]; unique: boolean };

// CheckConstraint is one CHECK constraint: its (resolved, unique-per-table) name, its
// persisted expression text — written back verbatim at every commit so the catalog bytes
// are stable (spec/fileformat/format.md "Check-expression text") — and the parsed
// expression the write paths resolve and evaluate per candidate row (constraints.md §4).
export type CheckConstraint = { name: string; exprText: string; expr: Expr };

// columnIndex returns the index of the named column (case-insensitive), or -1.
export function columnIndex(t: Table, name: string): number {
  const lower = name.toLowerCase();
  for (let i = 0; i < t.columns.length; i++) {
    if (t.columns[i]!.name.toLowerCase() === lower) return i;
  }
  return -1;
}

// pkIndices returns the primary-key member columns' indices in KEY order (the explicit
// pk list — the v5 catalog persists key order independent of declaration order). Empty =
// the table has no primary key (synthetic rowid keys).
export function pkIndices(t: Table): number[] {
  return t.pk;
}

// primaryKeyIndex returns the primary-key column's index iff the key is SINGLE-column,
// else -1. The PK pushdown (point lookup / range bound) recognizes single-column keys
// only — a composite-PK table full-scans this slice (spec/design/constraints.md §3) — so
// every pushdown site routes through this accessor and stays sound by construction.
export function primaryKeyIndex(t: Table): number {
  const idxs = pkIndices(t);
  return idxs.length === 1 ? idxs[0]! : -1;
}
