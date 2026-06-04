// Abstract syntax for the step-1 SQL surface. Boring, explicit shapes (CLAUDE.md §10);
// the hand-written parser produces these. Variants are discriminated unions tagged
// with `kind` (the elidable-subset analogue of Go's one-field-set structs / Rust's
// enums). Integer literals carry a `bigint` so int64 is exact.

// Literal is a literal value as written in SQL. A bare integer literal is an *untyped
// constant* that adapts to its context — the target column on INSERT/UPDATE, a sibling
// operand, the compared column in a WHERE predicate — and traps 22003 if it does not
// fit; with no context it defaults to int64 (spec/design/types.md §6). A boolean literal
// is expression-only this slice (it cannot be stored).
import type { Decimal } from "./decimal.ts";

export type Literal =
  | { kind: "null" }
  | { kind: "int"; int: bigint }
  | { kind: "bool"; value: boolean }
  // A single-quoted text literal (decoded content). Its type is always text (collation C);
  // it does not adapt to context like an integer literal does (spec/design/types.md §11).
  | { kind: "text"; text: string }
  // A decimal literal (carries the constructed value, sign folded). An untyped decimal
  // constant that adapts to context; caps are checked at resolve (grammar.md §14, decimal.md §6).
  | { kind: "decimal"; dec: Decimal };

// TypeMod is a parsed type modifier: a precision and an optional scale, as written
// (numeric(p) → scale null, numeric(p,s) → scale set). The values are the raw lexed magnitudes;
// range validation (1..=1000, 0..=p; else 22023) is at resolve.
export type TypeMod = { precision: bigint; scale: bigint | null };

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
  // A qualified column reference `rel.col`, where `rel` is a relation label in the FROM clause
  // (its alias, else its table name). Resolved against exactly that one relation, never
  // ambiguous (spec/design/grammar.md §15). Bare "column" stays the unqualified form.
  | { kind: "qualifiedColumn"; qualifier: string; name: string }
  | { kind: "literal"; literal: Literal }
  | { kind: "cast"; inner: Expr; typeName: string; typeMod: TypeMod | null }
  | { kind: "unary"; op: UnaryOp; operand: Expr }
  | { kind: "binary"; op: BinaryOp; lhs: Expr; rhs: Expr }
  | { kind: "isNull"; operand: Expr; negated: boolean }
  // `lhs IS [NOT] DISTINCT FROM rhs` — NULL-safe equality. `negated` carries the NOT
  // keyword: true is `IS NOT DISTINCT FROM` (NULL-safe `=`), false is `IS DISTINCT FROM`
  // (its negation). Always boolean-valued, never unknown (spec/design/functions.md §3).
  | { kind: "isDistinct"; lhs: Expr; rhs: Expr; negated: boolean }
  // `lhs IN (list)` / `lhs NOT IN (list)` — membership over a non-empty value list
  // (spec/design/grammar.md §20). Desugared at resolve into the OR-chain PostgreSQL defines it
  // as (`x IN (a,b)` is `x = a OR x = b`; NOT IN is its negation), inheriting the three-valued
  // NULL semantics and per-element operand typing from `=`/OR/NOT. The parser guarantees `list`
  // is non-empty (`IN ()` is 42601).
  | { kind: "in"; lhs: Expr; list: Expr[]; negated: boolean }
  // `lhs BETWEEN lo AND hi` / `lhs NOT BETWEEN lo AND hi` — a range test
  // (spec/design/grammar.md §21). Desugared at resolve into `lhs >= lo AND lhs <= hi` (NOT
  // BETWEEN negates), inheriting the three-valued NULL semantics from the comparisons and the
  // Kleene AND. The bounds parse at the additive level so the structural `AND` is not the
  // logical connective.
  | { kind: "between"; lhs: Expr; lo: Expr; hi: Expr; negated: boolean }
  // An aggregate function call — the engine's first function-call syntax (grammar.md §17).
  // `name` is the spelling as written (resolved case-insensitively against the aggregate
  // catalog; an unknown name is 42883). `star` is the COUNT(*) row-count form (then `arg` is
  // null); otherwise `arg` is the single argument expression. DISTINCT inside the parens is
  // rejected at parse (42601). Only aggregates resolve this slice; an aggregate in WHERE/ON
  // or nested in another aggregate is 42803 (spec/design/aggregates.md).
  | { kind: "funcCall"; name: string; arg: Expr | null; star: boolean };

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
export type OrderKey = {
  // An optional relation qualifier (`ORDER BY t.a`); null is a bare column.
  qualifier: string | null;
  column: string;
  descending: boolean;
  nullsFirst: boolean;
};

// ColumnDef is a column definition in a CREATE TABLE. typeName is kept as written and
// resolved during analysis (the catalog owns the type lattice). notNull is an explicit
// NOT NULL constraint; a PRIMARY KEY column is implicitly NOT NULL regardless, so the
// executor ORs the two (spec/design/constraints.md).
export type ColumnDef = {
  name: string;
  typeName: string;
  typeMod: TypeMod | null;
  primaryKey: boolean;
  notNull: boolean;
  // An optional DEFAULT <literal> — the value for this column when a row omits it (or uses the
  // DEFAULT keyword). Literal-only this slice; evaluated + type-coerced once at CREATE TABLE
  // (spec/design/constraints.md §2). null = no default.
  default: Literal | null;
};

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

// Insert is an INSERT ... [(col, ..)] VALUES with one or more rows, each value either a
// literal or the DEFAULT keyword. A multi-row INSERT is two-phase / all-or-nothing — every
// row is validated before any is stored (spec/design/grammar.md §12). `rows` is non-empty.
// `columns` is the optional explicit column list (`INSERT INTO t (a, c) VALUES ...`); null is
// the positional form (every column, in declaration order). Names resolve at execution time
// (unknown → 42703, duplicate → 42701); an unlisted column takes its default else NULL.
export type Insert = {
  kind: "insert";
  table: string;
  columns: string[] | null;
  rows: InsertValue[][];
};

// InsertValue is one value slot in an INSERT VALUES row: a literal, or the DEFAULT keyword —
// which substitutes the target column's declared default (or NULL if it has none). The DEFAULT
// keyword is not reserved (spec/design/grammar.md §3). See spec/design/constraints.md §2.
export type InsertValue = { kind: "lit"; lit: Literal } | { kind: "default" };

// TableRef is a table reference in a FROM clause: a table name with an optional alias
// (`orders o` or `orders AS o`). The alias, or the table name when there is none, is the
// relation's LABEL — it qualifies columns (o.col) and must be distinct within one query (a
// self-join needs aliases; a duplicate label is 42712). See spec/design/grammar.md §15.
export type TableRef = { name: string; alias: string | null };

// JoinKind is the kind of a join. "inner"/"cross" execute this slice; the "left"/"right"/"full"
// outer kinds parse and are carried in the AST but executing one is a documented 0A000
// narrowing (the OUTER family is a fast-follow — spec/design/grammar.md §15).
export type JoinKind = "inner" | "cross" | "left" | "right" | "full";

// JoinClause is one JOIN step in the left-deep FROM chain: the join kind, the right-hand
// table reference, and the optional ON predicate (null for CROSS JOIN; set for INNER/outer,
// which require an ON). See spec/design/grammar.md §15.
export type JoinClause = { kind: JoinKind; table: TableRef; on: Expr | null };

// Select is a SELECT. The FROM clause is a left-deep chain: `from` followed by zero or more
// `joins` (empty = single-table). limit caps the result at `limit` rows; offset skips the
// first `offset` rows. Both are non-negative counts, applied after ORDER BY, before projection
// (grammar.md §9); null means the clause is absent.
export type Select = {
  kind: "select";
  // SELECT DISTINCT — deduplicate the projected output rows (NULL-safe), applied after
  // ORDER BY and before LIMIT/OFFSET (spec/design/grammar.md §11).
  distinct: boolean;
  items: SelectItems;
  from: TableRef;
  // The left-deep JOINs after `from` (empty = a single-table SELECT). grammar.md §15.
  joins: JoinClause[];
  filter: Expr | null;
  // GROUP BY keys — bare or qualified table columns (never expressions/aliases/ordinals);
  // empty means no GROUP BY. Each is a "column" or "qualifiedColumn" (the parser restricts it
  // to column_ref). With keys present the query groups (spec/design/grammar.md §18).
  groupBy: Expr[];
  // The HAVING predicate (a boolean filter over the grouped rows), or null. May reference
  // aggregates and grouping keys; evaluated after aggregation, before ORDER BY. HAVING makes a
  // query an aggregate query even with no GROUP BY (spec/design/grammar.md §19).
  having: Expr | null;
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
