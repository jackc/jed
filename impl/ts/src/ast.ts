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
  | { kind: "isNull"; operand: Expr; negated: boolean }
  // `lhs IS [NOT] DISTINCT FROM rhs` — NULL-safe equality. `negated` carries the NOT
  // keyword: true is `IS NOT DISTINCT FROM` (NULL-safe `=`), false is `IS DISTINCT FROM`
  // (its negation). Always boolean-valued, never unknown (spec/design/functions.md §3).
  | { kind: "isDistinct"; lhs: Expr; rhs: Expr; negated: boolean };

// SelectItem is one select-list expression with its optional output-name alias
// (expr AS name). The alias is an output label only — it never enters resolution
// (spec/design/grammar.md §8). When alias is null the output name is derived by the
// resolver: a bare column's canonical name, or the fixed "?column?" otherwise.
export type SelectItem = { expr: Expr; alias: string | null };

// SelectItems is either all columns (*) or a list of projected expressions.
export type SelectItems =
  | { kind: "all" }
  | { kind: "list"; items: SelectItem[] };

// OrderKey is one ORDER BY sort key: a bare table column, a sort direction, and a resolved
// NULL placement. nullsFirst is resolved at parse time — an explicit NULLS FIRST|LAST, else
// the direction default (descending: ASC -> last, DESC -> first, the PostgreSQL model where
// NULL is the largest value) — and is applied independently of the descending value flip
// (spec/design/grammar.md §10).
export type OrderKey = { column: string; descending: boolean; nullsFirst: boolean };

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

// DropTable is a DROP TABLE statement. Removes a table — its definition and all its
// rows — from the catalog. Dropping a table that does not exist is an error (42P01);
// there is no IF EXISTS this slice. Single table only; no CASCADE/RESTRICT (no dependent
// objects exist yet). See spec/design/grammar.md §13.
export type DropTable = { kind: "dropTable"; name: string };

// Insert is an INSERT ... VALUES with one or more rows of literals, each in column
// order. A multi-row INSERT is two-phase / all-or-nothing — every row is validated
// before any is stored (spec/design/grammar.md §12). `rows` is always non-empty (the
// parser requires ≥1 row).
export type Insert = { kind: "insert"; table: string; rows: Literal[][] };

// Select is a single-table SELECT. limit caps the result at `limit` rows; offset skips
// the first `offset` rows. Both are non-negative counts, applied after ORDER BY, before
// projection (grammar.md §9); null means the clause is absent.
export type Select = {
  kind: "select";
  // SELECT DISTINCT — deduplicate the projected output rows (NULL-safe), applied after
  // ORDER BY and before LIMIT/OFFSET (spec/design/grammar.md §11).
  distinct: boolean;
  items: SelectItems;
  from: string;
  filter: Expr | null;
  // ORDER BY sort keys, applied left to right; empty means no ORDER BY (grammar.md §10).
  orderBy: OrderKey[];
  limit: bigint | null;
  offset: bigint | null;
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
export type Statement = CreateTable | DropTable | Insert | Select | Update | Delete;
