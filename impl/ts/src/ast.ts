// Abstract syntax for the step-1 SQL surface. Boring, explicit shapes (CLAUDE.md §10);
// the hand-written parser produces these. Variants are discriminated unions tagged
// with `kind` (the elidable-subset analogue of Go's one-field-set structs / Rust's
// enums). Integer literals carry a `bigint` so int64 is exact.

// Literal is a literal value as written in SQL. The type of a bare integer literal is
// intentionally not committed here (spec/design/conformance.md §7); resolved by context.
export type Literal = { kind: "null" } | { kind: "int"; int: bigint };

// Operand is a comparison's / assignment's right-hand side: a column ref or a literal.
export type Operand =
  | { kind: "column"; name: string }
  | { kind: "literal"; literal: Literal };

// CompareOp is a comparison operator.
export type CompareOp = "eq" | "lt" | "gt" | "le" | "ge";

// Predicate is a WHERE predicate. Step-1 has single predicates only (no AND/OR —
// boolean type deferred): either a comparison or a NULL test.
export type Predicate =
  | { kind: "compare"; column: string; op: CompareOp; rhs: Operand }
  | { kind: "isNull"; column: string; negated: boolean };

// SelectExpr is a projected expression: a column reference, a literal, or a cast (which
// nests via inner).
export type SelectExpr =
  | { kind: "column"; name: string }
  | { kind: "literal"; literal: Literal }
  | { kind: "cast"; inner: SelectExpr; typeName: string };

// SelectItems is either all columns (*) or a list of projected expressions.
export type SelectItems =
  | { kind: "all" }
  | { kind: "list"; items: SelectExpr[] };

// OrderBy is an ORDER BY clause. Step-1 corpus uses ascending only; descending is
// reserved for later.
export type OrderBy = { column: string; descending: boolean };

// ColumnDef is a column definition in a CREATE TABLE. typeName is kept as written and
// resolved during analysis (the catalog owns the type lattice).
export type ColumnDef = { name: string; typeName: string; primaryKey: boolean };

// Assignment is one `SET <column> = <value>` clause; value is read against the
// pre-update row (so `SET a = b, b = a` swaps).
export type Assignment = { column: string; value: Operand };

// CreateTable is a CREATE TABLE statement.
export type CreateTable = {
  kind: "createTable";
  name: string;
  columns: ColumnDef[];
};

// Insert is an INSERT ... VALUES with one row of literals, in column order.
export type Insert = { kind: "insert"; table: string; values: Literal[] };

// Select is a single-table SELECT.
export type Select = {
  kind: "select";
  items: SelectItems;
  from: string;
  filter: Predicate | null;
  orderBy: OrderBy | null;
};

// Update is `UPDATE <table> SET ... [WHERE ...]`. Assigning a PRIMARY KEY column is
// rejected this slice (the storage key must not change — see the executor).
export type Update = {
  kind: "update";
  table: string;
  assignments: Assignment[];
  filter: Predicate | null;
};

// Delete is `DELETE FROM <table> [WHERE ...]`. No WHERE deletes every row.
export type Delete = { kind: "delete"; table: string; filter: Predicate | null };

// Statement is a parsed top-level statement.
export type Statement = CreateTable | Insert | Select | Update | Delete;
