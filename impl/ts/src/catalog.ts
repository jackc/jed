// Table metadata: column definitions and lookups. Data-shaped (a type + free
// functions) to match the boring/explicit style (CLAUDE.md §10).

import type { Expr } from "./ast.ts";
import { type DecimalTypmod, type ScalarType, type Type } from "./types.ts";
import type { Value } from "./value.ts";

// Column is a column definition: name, declared type, nullability, primary-key flag, default.
// notNull is implied true for a PRIMARY KEY column.
export type Column = {
  name: string;
  // The column's declared type — a built-in scalar or a user-defined composite
  // (spec/design/composite.md). The open `Type` wrapper (CLAUDE.md §4): scalar-only call sites
  // read `typeScalar(col.type)`; the value codec / resolver branch on isCompositeType.
  type: Type;
  // The numeric(p,s) typmod for a decimal column, or null for a non-decimal column OR an
  // unconstrained numeric (spec/design/decimal.md §2). A constrained decimal column coerces
  // stored values to this precision/scale.
  decimal: DecimalTypmod | null;
  primaryKey: boolean;
  notNull: boolean;
  // The column's CONSTANT DEFAULT value, pre-evaluated and type-coerced at CREATE TABLE, or
  // null if it has no default or an EXPRESSION default (defaultExpr). A `{ kind: "null" }`
  // value is an explicit DEFAULT NULL. Applied for an omitted column or a DEFAULT keyword at
  // INSERT (spec/design/constraints.md §2).
  default: Value | null;
  // The column's EXPRESSION DEFAULT (a non-constant default like uuidv7() or 1 + 1), or null if
  // it has no default or a constant default (default). Mutually exclusive with default. Stored
  // as expression text (re-rendered verbatim at every commit, like a CHECK —
  // spec/fileformat/format.md) plus the parsed expression the write paths resolve and evaluate
  // per row (spec/design/constraints.md §2).
  defaultExpr: DefaultExpr | null;
};

// DefaultExpr is a column's EXPRESSION DEFAULT (spec/design/constraints.md §2): its persisted
// expression text — written back verbatim at every commit so the catalog bytes are stable
// (spec/fileformat/format.md "Check-expression text") — and the parsed expression the write
// paths resolve (against an empty scope, no columns) and evaluate per inserted row. Modeled on
// CheckConstraint.
export type DefaultExpr = { exprText: string; expr: Expr };

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
  // The table's FOREIGN KEY constraints in ascending lowercased-name order (the catalog's
  // on-disk order and the child-side evaluation order — spec/design/constraints.md §6.9).
  // Empty for a table with none.
  fks: ForeignKey[];
};

// FkAction is the persisted referential action for a foreign key's `ON DELETE` / `ON UPDATE`
// (spec/design/constraints.md §6.6). Only "noAction" (the default) and "restrict" are supported —
// they are identical in jed (no deferrable constraints). The write-actions (CASCADE / SET NULL /
// SET DEFAULT) are rejected 0A000 at CREATE TABLE, so never reach here; the on-disk encoding
// reserves codes for them (format.md).
export type FkAction = "noAction" | "restrict";

// ForeignKey is one resolved FOREIGN KEY constraint of a table (spec/design/constraints.md §6):
// its (per-table constraint-namespace) name, the referencing column ordinals into THIS table in
// list order, the referenced (parent) table name, the referenced column ordinals into the PARENT
// in list order (same length as columns), and the referential actions. An FK owns no B-tree;
// enforcement probes the parent's PK store or a unique index (§6.4). Held in ascending
// lowercased-name order on the table (the catalog's on-disk order and the child-side evaluation
// order — §6.9).
export type ForeignKey = {
  name: string;
  columns: number[];
  refTable: string;
  refColumns: number[];
  onDelete: FkAction;
  onUpdate: FkAction;
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

// CompositeType is a user-defined COMPOSITE (row) type (spec/design/composite.md): a named,
// ordered list of typed fields, living in the database's type catalog (a database-level object,
// not per-table). Created by `CREATE TYPE name AS (field type, …)`, referenced by name from a
// column's Type. Recursive — a field's type may itself be a composite (a nested composite,
// persisted by name; spec/fileformat/format.md *Composite-type entry*).
export type CompositeType = {
  // The type name (original case — round-trips what the user typed); looked up case-insensitively.
  name: string;
  // The fields in declaration order (>= 1).
  fields: CompositeField[];
};

// CompositeField is one field of a composite type: its name, type, decimal typmod, and declared
// nullability (mirrors Column).
export type CompositeField = {
  name: string;
  type: Type;
  // The decimal numeric(p,s) typmod when type is decimal, else null (mirrors Column).
  decimal: DecimalTypmod | null;
  // Whether the field was declared NOT NULL.
  notNull: boolean;
};

// SequenceDef is a SEQUENCE (spec/design/sequences.md): a named, persisted, monotonic i64
// generator — the third database-level catalog-object kind (after tables and composite types). The
// definition fields (increment/minValue/maxValue/start/cache/cycle) are immutable; lastValue +
// isCalled are the mutable counter state a nextval advances. The whole struct lives in the snapshot
// catalog, so the counter is transactional by construction (sequences.md §5). The i64 fields are
// bigint (the TS core's exact-i64 representation).
export type SequenceDef = {
  // The sequence name (original case; looked up case-insensitively).
  name: string;
  // The step per nextval (non-zero). Positive = ascending, negative = descending.
  increment: bigint;
  // The inclusive lower bound.
  minValue: bigint;
  // The inclusive upper bound.
  maxValue: bigint;
  // The first value nextval returns (on a fresh sequence, lastValue === start).
  start: bigint;
  // The PostgreSQL CACHE size — stored for fidelity but behaves as 1 (sequences.md §7).
  cache: bigint;
  // Whether nextval wraps at a bound (CYCLE) instead of raising 2200H.
  cycle: boolean;
  // The mutable counter: the most recent value produced (or start before the first call).
  lastValue: bigint;
  // Whether nextval has been called: false ⇒ the next call returns lastValue (= start) without
  // incrementing; true ⇒ it adds increment (PostgreSQL's is_called).
  isCalled: boolean;
};

// I64_MAX is the i64 maximum (2^63-1), the default ascending MAXVALUE / descending floor base.
export const I64_MAX = 9223372036854775807n;

// defaultSequenceBounds is the type defaults for an ascending (increment > 0) vs descending sequence,
// before any explicit MIN/MAX/START override (PostgreSQL): ascending ⇒ [1, i64::MAX], start = MIN;
// descending ⇒ [-(i64::MAX), -1], start = MAX. (i64::MIN is reserved out of the default descending
// floor, matching PG's -9223372036854775807 default MINVALUE.) Returns [min, max].
export function defaultSequenceBounds(increment: bigint): [bigint, bigint] {
  if (increment < 0n) return [-I64_MAX, -1n];
  return [1n, I64_MAX];
}

// ColType is a fully-resolved storage/codec column type (spec/design/composite.md §4): a scalar,
// or a composite resolved to the codec/coercion tree of its fields. Built ONCE from a catalog
// Type against the snapshot's composite-type definitions (resolveColType) and held by the
// TableStore, so the value codec and store-coercion never re-walk the type catalog on every row.
// Recursive — a composite field may itself be composite. The codec reads only the scalar / field
// structure; the field typmod / notNull are consulted by store-coercion (executor). Modeled as a
// discriminated union (keyed on `kind`, like Type/Value), with free-function helpers — never
// methods on the union (CLAUDE.md §10).
export type ColType =
  | { kind: "scalar"; scalar: ScalarType }
  // A composite type's resolved fields, in declaration order. `name` is the (original-case) type
  // name, used in store-coercion error messages.
  | { kind: "composite"; name: string; fields: ColField[] }
  // An array's resolved element type (spec/design/array.md §3). Structural — the element type is
  // carried inline, recursively; v1 element types are scalars.
  | { kind: "array"; elem: ColType };

// ColField is one resolved field of a composite ColType — its name, recursively-resolved type, the
// decimal typmod (when the field is decimal), and declared nullability (mirrors CompositeField, but
// with the type fully resolved for the codec/coercion path).
export type ColField = { name: string; type: ColType; typmod: DecimalTypmod | null; notNull: boolean };

// resolveColType resolves a catalog Type into a self-contained ColType against the database's
// composite definitions (keyed by lowercased name, the Snapshot.types map). A composite reference
// is looked up case-insensitively and recursively resolved; the lookup is guaranteed to succeed
// because validateCompositeTypes (the two-pass load / CREATE TYPE gate) proved every reference
// exists and the graph is acyclic before any store is built (spec/design/composite.md §3).
export function resolveColType(ty: Type, types: Map<string, CompositeType>): ColType {
  if (ty.kind === "scalar") return { kind: "scalar", scalar: ty.scalar };
  if (ty.kind === "array") return { kind: "array", elem: resolveColType(ty.elem, types) };
  const def = types.get(ty.name.toLowerCase());
  if (def === undefined) {
    throw new Error("composite type reference resolved by validateCompositeTypes");
  }
  return {
    kind: "composite",
    name: def.name,
    fields: def.fields.map((f) => ({
      name: f.name,
      type: resolveColType(f.type, types),
      typmod: f.decimal,
      notNull: f.notNull,
    })),
  };
}

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
