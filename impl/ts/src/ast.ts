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
  // A bind parameter $N (1-based index). Like an adaptable literal it takes its type from
  // context at resolve; the host binds a value at execute (spec/design/api.md §5).
  | { kind: "param"; index: number }
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
  // `lhs LIKE rhs` / `lhs NOT LIKE rhs` — text pattern match (spec/design/grammar.md §22). `%`
  // matches any run of characters, `_` one code point, with the default `\` escape. Both
  // operands must be text; NULL propagates. A genuine operator (not desugared) with a
  // hand-written matcher. `negated` carries the NOT keyword.
  | { kind: "like"; lhs: Expr; rhs: Expr; negated: boolean }
  // A CASE expression (spec/design/grammar.md §23). Searched form: `operand` is null and each
  // `whens` condition must be boolean. Simple form: `operand` is non-null and each branch matches
  // when `operand = cond`. `whens` has ≥1 entry; `els` is the ELSE result, or null for an implicit
  // `ELSE NULL`. Lazily evaluated: the first TRUE branch wins; result-arm types unify.
  | { kind: "case"; operand: Expr | null; whens: { cond: Expr; result: Expr }[]; els: Expr | null }
  // A function call — the shared aggregate/scalar call syntax (grammar.md §17). `name` is the
  // spelling as written, resolved case-insensitively: an aggregate (COUNT/SUM/MIN/MAX/AVG), a
  // scalar function (abs/round, kind = "function", spec/design/functions.md §9), or 42883. `star`
  // is the COUNT(*) row-count form (then `args` is empty); otherwise `args` is the comma-separated
  // argument list — aggregates and abs take one, round one or two. DISTINCT inside the parens is
  // rejected at parse (42601). An aggregate in WHERE/ON or nested in another aggregate is 42803
  // (spec/design/aggregates.md); a scalar function is legal anywhere an expression is.
  | { kind: "funcCall"; name: string; args: Expr[]; star: boolean }
  // A scalar subquery `( query_expr )` in expression position (spec/design/grammar.md §26). resolve
  // plans it once against the scope chain; an uncorrelated one is then folded to a constant, a
  // correlated one is re-executed per outer row. A `$N` inside is a 0A000.
  | { kind: "scalarSubquery"; query: QueryExpr }
  // `EXISTS ( query_expr )` (a leading NOT is the ordinary unary connective). grammar.md §26.
  | { kind: "exists"; query: QueryExpr }
  // `lhs [NOT] IN ( query_expr )` (spec/design/grammar.md §26) — membership of lhs in the
  // subquery's single output column (three-valued, like a literal IN).
  | { kind: "inSubquery"; lhs: Expr; query: QueryExpr; negated: boolean };

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

// CreateTable is a CREATE TABLE statement. tablePks is the table-level
// `PRIMARY KEY (a, b, ...)` constraints, each a list of member column names in key order
// (spec/design/grammar.md §28). The parser collects every one it sees; CREATE TABLE's
// execution resolves them (42703/42701) and rejects more than one primary key across both
// forms (42P16) — spec/design/constraints.md §3.
export type CreateTable = {
  kind: "createTable";
  name: string;
  columns: ColumnDef[];
  tablePks: string[][];
  // Every `[CONSTRAINT name] CHECK ( expr )` of the statement — column-level and
  // table-level forms are semantically identical, so both collect here, in TEXTUAL
  // DEFINITION ORDER (it drives validation and naming — spec/design/constraints.md §4).
  // CREATE TABLE's execution validates each (0A000/42803/42P02/42703/42804) and names the
  // unnamed ones (42710 on a collision).
  checks: CheckDef[];
  // Every `[CONSTRAINT name] UNIQUE [(cols)]` of the statement — the column-level form
  // collects as a one-member list — in TEXTUAL DEFINITION ORDER (it drives member
  // resolution, the dedup/PK fold, and naming — spec/design/constraints.md §5). Each
  // survivor becomes a unique secondary index (spec/design/indexes.md §8).
  uniques: UniqueDef[];
};

// CheckDef is one parsed CHECK constraint (spec/design/grammar.md §29): the optional
// explicit CONSTRAINT name (null = unnamed), the expression, and the expression's
// persisted text — the source token sequence between the parentheses re-rendered per the
// closed table in spec/fileformat/format.md "Check-expression text".
export type CheckDef = { name: string | null; expr: Expr; text: string };

// UniqueDef is one parsed UNIQUE constraint (spec/design/grammar.md §31): the optional
// explicit CONSTRAINT name (null = unnamed; it names the backing index) and the member
// column names in list order. Execution resolves the members (42703/42701/0A000) and
// names the index (42P07/42710) — spec/design/constraints.md §5.
export type UniqueDef = { name: string | null; columns: string[] };

// DropTable is a DROP TABLE statement. Removes a table — its definition and all its
// rows — from the catalog. Dropping a table that does not exist is an error (42P01);
// there is no IF EXISTS this slice. Single table only; no CASCADE/RESTRICT (no dependent
// objects exist yet). See spec/design/grammar.md §13.
export type DropTable = { kind: "dropTable"; name: string };

// CreateIndex is a CREATE [UNIQUE] INDEX [name] ON <table> ( col [, col]* ) statement —
// a secondary index (spec/design/indexes.md, grammar.md §30). name === null is the
// unnamed form; the executor derives PostgreSQL's auto-name. Key columns are bare names
// (no expression/ordered/partial keys this slice); a column may repeat (PG allows it).
// Execution validates in PG's order: table 42P01, columns 42703/0A000, name collision
// 42P07. A unique index additionally verifies the existing rows at build (23505) and
// enforces uniqueness thereafter (spec/design/indexes.md §8).
export type CreateIndex = {
  kind: "createIndex";
  name: string | null;
  table: string;
  columns: string[];
  unique: boolean;
};

// DropIndex is a DROP INDEX <name> statement — remove one secondary index
// (spec/design/indexes.md §2). Missing → 42704; a table's name → 42809.
export type DropIndex = { kind: "dropIndex"; name: string };

// Insert is an INSERT ... [(col, ..)] whose rows come from EITHER a VALUES list (each value a
// literal or the DEFAULT keyword) OR a SELECT (INSERT ... SELECT — spec/design/grammar.md §24).
// An INSERT is two-phase / all-or-nothing — every row is validated before any is stored
// (spec/design/grammar.md §12).
// `columns` is the optional explicit column list (`INSERT INTO t (a, c) VALUES ...` /
// `... SELECT ...`); null is the positional form (every column, in declaration order). Names
// resolve at execution time (unknown → 42703, duplicate → 42701); an unlisted column takes its
// default else NULL.
// `source` is the VALUES list (rows, non-empty) or the SELECT whose result rows are inserted.
export type Insert = {
  kind: "insert";
  table: string;
  columns: string[] | null;
  source:
    | { kind: "values"; rows: InsertValue[][] }
    | { kind: "select"; select: Select };
  // The optional terminal RETURNING clause (spec/design/grammar.md §32): project each stored
  // row, turning the statement into a query result. Null = no clause.
  returning: SelectItems | null;
};

// InsertValue is one value slot in an INSERT VALUES row: a literal, or the DEFAULT keyword —
// which substitutes the target column's declared default (or NULL if it has none). The DEFAULT
// keyword is not reserved (spec/design/grammar.md §3). See spec/design/constraints.md §2.
export type InsertValue =
  | { kind: "lit"; lit: Literal }
  // A bind parameter $N (1-based), bound at execute — spec/design/api.md §5.
  | { kind: "param"; index: number }
  | { kind: "default" };

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
  // The first table reference of the FROM clause, or null for a FROM-less SELECT — the
  // select list evaluates over one virtual zero-column row (spec/design/grammar.md §34).
  from: TableRef | null;
  // The left-deep JOINs after `from` (empty = a single-table SELECT; always empty when
  // `from` is null — joins exist only inside a FROM clause). grammar.md §15.
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
  // The optional terminal RETURNING clause (spec/design/grammar.md §32): project each matched
  // row's NEW (post-assignment) values. Null = no clause.
  returning: SelectItems | null;
};

// Delete is `DELETE FROM <table> [WHERE ...]`. No WHERE deletes every row; the WHERE
// expression must resolve to boolean.
// `returning` is the optional terminal RETURNING clause (spec/design/grammar.md §32):
// project each deleted row's OLD values. Null = no clause.
export type Delete = {
  kind: "delete";
  table: string;
  filter: Expr | null;
  returning: SelectItems | null;
};

// Begin/Commit/Rollback are the explicit transaction-control statements (grammar.md §27,
// transactions.md §4.2). Begin's `writable` is the access mode: true is READ WRITE (the default),
// false READ ONLY (a write inside → 25006). A nested BEGIN is 25001; a COMMIT/ROLLBACK with no
// open block is a no-op success.
export type Begin = { kind: "begin"; writable: boolean };
export type Commit = { kind: "commit" };
export type Rollback = { kind: "rollback" };

// SetOpKind is the set operator (spec/design/grammar.md §25).
export type SetOpKind = "union" | "intersect" | "except";

// QueryExpr is the operand of a set operation (spec/design/grammar.md §25): either a single SELECT
// core or a nested set operation, so a chain like `a UNION b INTERSECT c` forms a tree.
export type QueryExpr = Select | SetOp;

// SetOp combines two query expressions (spec/design/grammar.md §25). `all` is the ALL (multiset)
// flag — false is the deduplicating default. The optional trailing ORDER BY / LIMIT / OFFSET apply
// to the WHOLE combined result and live on the outermost node only (an operand carries none — a
// deferred narrowing); orderBy keys resolve against the output column names (the left operand's).
// Precedence is handled by the parser: INTERSECT binds tighter than UNION/EXCEPT (left-associative).
export type SetOp = {
  kind: "setOp";
  op: SetOpKind;
  all: boolean;
  lhs: QueryExpr;
  rhs: QueryExpr;
  orderBy: OrderKey[];
  limit: bigint | null;
  offset: bigint | null;
};

// Statement is a parsed top-level statement. A lone SELECT stays `Select`; `SetOp` appears only
// when at least one set operator is present, so the plain-query path and host API are untouched.
export type Statement =
  | CreateTable
  | DropTable
  | CreateIndex
  | DropIndex
  | Insert
  | Select
  | SetOp
  | Update
  | Delete
  | Begin
  | Commit
  | Rollback;
