// Table metadata: column definitions and lookups. Data-shaped (a type + free
// functions) to match the boring/explicit style (CLAUDE.md §10).

import type { DecimalTypmod, ScalarType } from "./types.ts";

// Column is a column definition: name, declared type, nullability, primary-key flag.
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

// primaryKeyIndex returns the primary-key column's index, or -1. Step-1 supports at
// most a single-column primary key.
export function primaryKeyIndex(t: Table): number {
  for (let i = 0; i < t.columns.length; i++) {
    if (t.columns[i]!.primaryKey) return i;
  }
  return -1;
}
