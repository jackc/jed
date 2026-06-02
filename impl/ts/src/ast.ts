// Abstract syntax for the step-1 SQL surface. Boring, explicit shapes (CLAUDE.md §10);
// the hand-written parser produces these. Variants are discriminated unions tagged
// with `kind` (the elidable-subset analogue of Go's one-field-set structs / Rust's
// enums). Integer literals carry a `bigint` so int64 is exact.

// Literal is a literal value as written in SQL. A bare integer literal is an *untyped
// constant* that adapts to its context — the target column on INSERT/UPDATE, a sibling
// operand, the compared column in a WHERE predicate — and traps 22003 if it does not
// fit; with no context it defaults to int64 (spec/design/types.md §6). A boolean literal
// is expression-only this slice (it cannot be stored).
export type Literal =
  | { kind: "null" }
  | { kind: "int"; int: bigint }
  | { kind: "bool"; value: boolean };

// UnaryOp: arithmetic negation `-x` or logical negation `NOT x`.
export type UnaryOp = "neg" | "not";

// BinaryOp: arithmetic (integer→promoted), comparison (integer→boolean), or logical
// (boolean→boolean, Kleene).
export type BinaryOp =
  | "add"
  | "sub"
  | "mul"
  | "div"
  | "mod"
  | "eq"
  | "lt"
  | "gt"
  | "le"
  | "ge"
  | "and"
  | "or";

// Expr is a general expression, shared by the SELECT list, WHERE, and UPDATE ... SET.
// The parser builds it via a precedence ladder (spec/grammar/grammar.ebnf `expr`). A
// comparison/logical/null-test node is boolean-valued; arithmetic and
// columns/integer-literals are integer-valued.
export type Expr =
  | { kind: "column"; name: string }
  | { kind: "literal"; literal: Literal }
  | { kind: "cast"; inner: Expr; typeName: string }
  | { kind: "unary"; op: UnaryOp; operand: Expr }
  | { kind: "binary"; op: BinaryOp; lhs: Expr; rhs: Expr }
  | { kind: "isNull"; operand: Expr; negated: boolean };

// SelectItems is either all columns (*) or a list of projected expressions.
export type SelectItems =
  | { kind: "all" }
  | { kind: "list"; items: Expr[] };

// OrderBy is an ORDER BY clause. Step-1 corpus uses ascending only; descending is
// reserved for later.
export type OrderBy = { column: string; descending: boolean };

// ColumnDef is a column definition in a CREATE TABLE. typeName is kept as written and
// resolved during analysis (the catalog owns the type lattice).
export type ColumnDef = { name: string; typeName: string; primaryKey: boolean };

// Assignment is one `SET <column> = <value>` clause; value is a general expression
// evaluated against the pre-update row (so `SET a = b, b = a` swaps).
export type Assignment = { column: string; value: Expr };

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
  filter: Expr | null;
  orderBy: OrderBy | null;
};

// Update is `UPDATE <table> SET ... [WHERE ...]`. Assigning a PRIMARY KEY column is
// rejected this slice (the storage key must not change — see the executor). The WHERE
// expression must resolve to boolean.
export type Update = {
  kind: "update";
  table: string;
  assignments: Assignment[];
  filter: Expr | null;
};

// Delete is `DELETE FROM <table> [WHERE ...]`. No WHERE deletes every row; the WHERE
// expression must resolve to boolean.
export type Delete = { kind: "delete"; table: string; filter: Expr | null };

// Statement is a parsed top-level statement.
export type Statement = CreateTable | Insert | Select | Update | Delete;
