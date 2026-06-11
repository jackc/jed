// Table metadata: column definitions and lookups. Data-shaped (a type + free
// functions) to match the boring/explicit style (CLAUDE.md §10).

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
export type Table = { name: string; columns: Column[] };

// columnIndex returns the index of the named column (case-insensitive), or -1.
export function columnIndex(t: Table, name: string): number {
  const lower = name.toLowerCase();
  for (let i = 0; i < t.columns.length; i++) {
    if (t.columns[i]!.name.toLowerCase() === lower) return i;
  }
  return -1;
}

// pkIndices returns the primary-key member columns' indices in KEY order. Key order is
// the flagged columns in declaration order — CREATE TABLE requires the constraint's list
// order to match (the documented 0A000 narrowing, spec/design/constraints.md §3), so the
// flag bits alone reconstruct the key. Empty = the table has no primary key (synthetic
// rowid keys).
export function pkIndices(t: Table): number[] {
  const idxs: number[] = [];
  for (let i = 0; i < t.columns.length; i++) {
    if (t.columns[i]!.primaryKey) idxs.push(i);
  }
  return idxs;
}

// primaryKeyIndex returns the primary-key column's index iff the key is SINGLE-column,
// else -1. The PK pushdown (point lookup / range bound) recognizes single-column keys
// only — a composite-PK table full-scans this slice (spec/design/constraints.md §3) — so
// every pushdown site routes through this accessor and stays sound by construction.
export function primaryKeyIndex(t: Table): number {
  const idxs = pkIndices(t);
  return idxs.length === 1 ? idxs[0]! : -1;
}
