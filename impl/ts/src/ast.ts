// Abstract syntax for the step-1 SQL surface. Boring, explicit shapes (CLAUDE.md §10);
// the hand-written parser produces these. Variants are discriminated unions tagged
// with `kind` (the elidable-subset analogue of Go's one-field-set structs / Rust's
// enums). Integer literals carry a `bigint` so i64 is exact.

// Literal is a literal value as written in SQL. A bare integer literal is an *untyped
// constant* that adapts to its context — the target column on INSERT/UPDATE, a sibling
// operand, the compared column in a WHERE predicate — and traps 22003 if it does not
// fit; with no context it defaults to i64 (spec/design/types.md §6). A boolean literal
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
  // ne is <> (alias !=): the 3VL negation of eq, propagating NULL like eq.
  | "ne"
  | "lt"
  | "gt"
  | "le"
  | "ge"
  | "and"
  | "or"
  // concat is the `||` array concatenation operator (spec/design/array-functions.md §8):
  // array∥array (array_cat), array∥element (array_append), element∥array (array_prepend).
  | "concat"
  // contains/containedBy/overlaps are the array containment/overlap operators `@>`/`<@`/`&&`
  // (spec/design/array-functions.md §10): each `anyarray <op> anyarray → boolean`, polymorphic.
  // They are SHARED with the range boolean surface (range-functions.md §3): a range operand routes
  // to the range axis (`@>`/`<@` also gain the range/element overloads).
  | "contains"
  | "containedBy"
  | "overlaps"
  // strictlyLeft/strictlyRight/notExtendRight/notExtendLeft/adjacent are the range-ONLY positional
  // and adjacency operators `<<`/`>>`/`&<`/`&>`/`-|-` (spec/design/range-functions.md §3): each
  // `range <op> range → boolean`, a definite boolean. A non-range pair is 42883 (no array overload).
  | "strictlyLeft"
  | "strictlyRight"
  | "notExtendRight"
  | "notExtendLeft"
  | "adjacent"
  // jsonGet/jsonGetText/jsonGetPath/jsonGetPathText are the jsonb accessor operators
  // `->`/`->>`/`#>`/`#>>` (spec/design/json-sql-functions.md §1, J4): `->` get field/element,
  // `->>` get as text, `#>` get at path, `#>>` get at path as text. The result type and the
  // field-vs-index split are decided at resolve from the operand types.
  | "jsonGet"
  | "jsonGetText"
  | "jsonGetPath"
  | "jsonGetPathText"
  // jsonHasKey/jsonHasAnyKey/jsonHasAllKeys are the jsonb key-existence operators `?`/`?|`/`?&`
  // (spec/design/json-sql-functions.md §1, J5): `?` a key exists, `?|` any key of a text[] exists,
  // `?&` all keys exist. `boolean` result.
  | "jsonHasKey"
  | "jsonHasAnyKey"
  | "jsonHasAllKeys";

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
  // A typed string literal `type '...'` (spec/design/grammar.md §36) — PostgreSQL's `type 'string'`,
  // equal to CAST('string' AS type) over a string-literal operand. typeName names the target scalar
  // (resolved by scalarFromName; unknown → 42704) and text is the literal's string; the string is
  // coerced to the type at resolve. The keyword names the type, so the literal carries it in any
  // expression position (`INTERVAL '1 day'`, `INTEGER '42'`).
  | { kind: "typedLiteral"; typeName: string; text: string }
  // A bind parameter $N (1-based index). Like an adaptable literal it takes its type from
  // context at resolve; the host binds a value at execute (spec/design/api.md §5).
  | { kind: "param"; index: number }
  | { kind: "cast"; inner: Expr; typeName: string; typeMod: TypeMod | null }
  // EXTRACT(field FROM source) — the datetime field special form (timezones.md §9.2, grammar.md §50).
  // The field is syntactic (identifier or string literal, lowercased at parse); resolves to numeric.
  | { kind: "extract"; field: string; source: Expr }
  // expr COLLATE "name" — the postfix collation operator (spec/design/collation.md §1). Sets an
  // EXPLICIT collation on a text expression for the surrounding comparison / ORDER BY; binds at the
  // postfix/typecast level (tighter than || and the comparisons — PG precedence). `collation` is a
  // quoted identifier (case-sensitive, e.g. "C", "en-US"). A non-text inner is 42804, an unloaded
  // name 42704, two different explicit collations in one comparison 42P21.
  | { kind: "collate"; inner: Expr; collation: string }
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
  // hand-written matcher. `negated` carries the NOT keyword; `insensitive` carries ILIKE
  // (case-insensitive matching, both sides simple-lowercased under the casing regime — collation.md §16).
  | { kind: "like"; lhs: Expr; rhs: Expr; negated: boolean; insensitive: boolean }
  // `lhs ~ rhs` / `~*` / `!~` / `!~*` — regular-expression match (grammar.md §22b, regex.md). jed's
  // own RE2-able flavor (not PostgreSQL-compatible), matched by a hand-written linear-time Pike VM.
  // UNANCHORED (matches a substring). Both operands must be text; NULL propagates. `negated` carries
  // `!~`/`!~*`; `insensitive` carries `~*`/`!~*` (both sides simple-lowercased like ILIKE).
  | { kind: "regex"; lhs: Expr; rhs: Expr; negated: boolean; insensitive: boolean }
  // A CASE expression (spec/design/grammar.md §23). Searched form: `operand` is null and each
  // `whens` condition must be boolean. Simple form: `operand` is non-null and each branch matches
  // when `operand = cond`. `whens` has ≥1 entry; `els` is the ELSE result, or null for an implicit
  // `ELSE NULL`. Lazily evaluated: the first TRUE branch wins; result-arm types unify.
  | {
      kind: "case";
      operand: Expr | null;
      whens: { cond: Expr; result: Expr }[];
      els: Expr | null;
    }
  // A function call — the shared aggregate/scalar call syntax (grammar.md §17). `name` is the
  // spelling as written, resolved case-insensitively: an aggregate (COUNT/SUM/MIN/MAX/AVG), a
  // scalar function (abs/round, kind = "function", spec/design/functions.md §9), or 42883. `star`
  // is the COUNT(*) row-count form (then `args` is empty); otherwise `args` is the comma-separated
  // argument list — aggregates and abs take one, round one or two. `distinct` carries a leading
  // DISTINCT inside the parens (COUNT(DISTINCT x), aggregates.md §5). An aggregate in WHERE/ON or
  // nested in another aggregate is 42803 (spec/design/aggregates.md); a scalar function is legal
  // anywhere an expression is. `argNames`
  // carries PostgreSQL named notation (name => value, grammar.md §17): empty ⇒ every argument
  // positional (the common case); otherwise it is parallel to `args`, with a string for a named
  // slot and null for a positional one. The parser rejects a positional arg after a named one.
  // `variadic` is true when the final argument was prefixed with the VARIADIC keyword
  // (num_nulls(VARIADIC arr), array-functions.md §12): the array is passed directly to a variadic
  // parameter rather than spreading individual arguments. false for every ordinary call.
  | {
      kind: "funcCall";
      name: string;
      args: Expr[];
      argNames: (string | null)[];
      star: boolean;
      // true when the argument was prefixed with DISTINCT (COUNT(DISTINCT x) — aggregates.md §5):
      // the aggregate folds only the distinct non-NULL argument values. Only an aggregate accepts
      // it — DISTINCT on a scalar function is 42809, on a window function 0A000, and
      // f(DISTINCT *) / f(DISTINCT) is a 42601 syntax error.
      distinct: boolean;
      // The FILTER (WHERE cond) condition when present (SUM(x) FILTER (WHERE y > 0) —
      // aggregates.md §11): the aggregate folds only the input rows for which cond is TRUE.
      // null/undefined for a plain call. Only an aggregate accepts it — FILTER on a scalar function
      // is 42809, on a window function 0A000; an aggregate inside cond is 42803, a non-boolean 42804.
      filter?: Expr | null;
      variadic: boolean;
      // Set when the call carries a trailing `OVER (...)` window clause (a WINDOW-function call —
      // spec/design/window.md). null/undefined for an ordinary scalar/aggregate/SRF call. A
      // window-only function (row_number/…) with no `over` is 42809; an aggregate with `over` set
      // is a window aggregate (S3).
      over?: WindowDef | null;
      // Set (to the window name) when the call is `f(...) OVER name` referencing a named window (the
      // WINDOW clause — spec/design/window.md §5). A desugaring pass replaces it with the named
      // definition (into `over`) before resolution; exactly one of `over`/`overName` is set on a
      // window call. null/undefined for an inline `OVER (...)` or a non-window call.
      overName?: string | null;
    }
  // A scalar subquery `( query_expr )` in expression position (spec/design/grammar.md §26). resolve
  // plans it once against the scope chain; an uncorrelated one is then folded to a constant, a
  // correlated one is re-executed per outer row. A `$N` inside is a 0A000.
  | { kind: "scalarSubquery"; query: QueryExpr }
  // `EXISTS ( query_expr )` (a leading NOT is the ordinary unary connective). grammar.md §26.
  | { kind: "exists"; query: QueryExpr }
  // `lhs [NOT] IN ( query_expr )` (spec/design/grammar.md §26) — membership of lhs in the
  // subquery's single output column (three-valued, like a literal IN).
  | { kind: "inSubquery"; lhs: Expr; query: QueryExpr; negated: boolean }
  // `lhs op ANY/SOME/ALL ( array )` — a quantified array comparison (spec/design/array-functions.md
  // §11), the array spelling of IN. `op` is a comparison (eq/lt/gt/le/ge); `all` is true for ALL,
  // false for ANY/SOME (SOME folds to ANY at parse). The three-valued fold over the array's
  // flattened elements reuses the IN-list membership semantics, generalized to all five comparison
  // operators and both quantifiers.
  | { kind: "quantified"; op: BinaryOp; all: boolean; lhs: Expr; array: Expr }
  // `lhs op ANY/SOME/ALL ( query_expr )` — the SUBQUERY form of the quantified comparison
  // (spec/design/array-functions.md §11.6), the subquery spelling of IN. Parallel to inSubquery: the
  // body's single column (42601 if >1) folds through the SAME three-valued fold as `quantified`.
  // Uncorrelated folds to a constant-array quantified node; correlated re-executes per outer row.
  | {
      kind: "quantifiedSubquery";
      op: BinaryOp;
      all: boolean;
      lhs: Expr;
      query: QueryExpr;
    }
  // A `ROW(e1, e2, …)` composite constructor (spec/design/composite.md §1). Builds a row value from
  // the field expressions; `ROW(x)` is a one-field row, `ROW()` the zero-field row. The bare
  // `(a, b)` form is deferred (0A000); only the keyword form parses.
  | { kind: "row"; fields: Expr[] }
  // An `ARRAY[e1, e2, …]` array constructor (spec/design/array.md §1). Builds a 1-D array value from
  // the element expressions, unified to a common element type at resolve; `ARRAY[]` is the empty
  // array (its element type comes from an enclosing cast/column context).
  | { kind: "array"; elements: Expr[] }
  // Field selection `(expr).field` (spec/design/composite.md §S4) — the value of one named field of
  // a composite `base`. The parser produces this for a `.name` postfix on a parenthesized / ROW(…) /
  // cast / qualified-column base; a bare `a.b` stays qualifiedColumn and only falls back to field
  // access at resolve when `a` is no relation but a composite column (the ambiguity rule — table.column
  // first, then column.field). Field lookup is case-insensitive; an unknown field is 42703, a
  // non-composite base 42809.
  | { kind: "fieldAccess"; base: Expr; field: string }
  // Whole-row expansion `(expr).*` (spec/design/composite.md §S4) — expands a composite `base` into
  // one output column per field, in declaration order. Valid only in a SELECT/RETURNING projection
  // list (where `*` expands); in any scalar expression position it is 0A000.
  | { kind: "fieldStar"; base: Expr }
  // Array subscript `base[..][..]` (spec/design/array.md §6) — one or more bracketed specs applied
  // to an array `base`. Each spec is an index `[i]` or a slice `[m:n]` (with optionally-omitted
  // bounds). All-index access reads a single 1-based element (the element type); if any spec is a
  // slice the access returns a sub-array (the array type), and a scalar index i then means 1:i. An
  // out-of-bounds / NULL subscript yields NULL (PG); a non-array base is 42804 at resolve. The parser
  // collects consecutive `[…]` postfixes into one node (so `a[1][2]` is one access, two specs).
  | { kind: "subscript"; base: Expr; subscripts: SubscriptSpec[] };

// SubscriptSpec is one subscript spec inside a "subscript" expr (spec/design/array.md §6): an index
// `[i]` (isSlice false, index set) or a slice `[m:n]` (isSlice true; lower/upper may be null for an
// omitted bound `[:n]`/`[m:]`/`[:]`).
export type SubscriptSpec =
  | { isSlice: false; index: Expr }
  | { isSlice: true; lower: Expr | null; upper: Expr | null };

// SelectItem is one select-list expression with its optional output-name alias
// (expr AS name). The alias is an output label only — it never enters resolution
// (spec/design/grammar.md §8). When alias is null the output name is derived by the
// resolver: a bare column's canonical name, or the fixed "?column?" otherwise.
export type SelectItem = { expr: Expr; alias: string | null };

// SelectItems is either all columns (*) or a list of projected expressions.
export type SelectItems = { kind: "all" } | { kind: "list"; items: SelectItem[] };

// OrderKey is one ORDER BY sort key: a bare table column, a sort direction, and a resolved
// NULL placement. nullsFirst is resolved at parse time — an explicit NULLS FIRST|LAST, else
// the direction default (descending: ASC -> last, DESC -> first, the PostgreSQL model where
// NULL is the largest value) — and is applied independently of the descending value flip
// (spec/design/grammar.md §10).
export type OrderKey = {
  // An optional relation qualifier (`ORDER BY t.a`); null is a bare column.
  qualifier: string | null;
  column: string;
  // An optional explicit `COLLATE "name"` on this sort key (spec/design/collation.md §1); null means
  // the column's collation (the database default, C, until slice 1d). A non-C name orders this key by
  // that collation's UCA sort key; an unknown name is 42704, a non-text column with a COLLATE is 42804.
  collation: string | null;
  descending: boolean;
  nullsFirst: boolean;
};

// WindowOrderKey is one window ORDER BY sort key (spec/design/window.md §3/§5.1). Unlike the query
// OrderKey (column references only), a window sort key is a general expression (`ORDER BY a + b`,
// `ORDER BY sum(x)` in a grouped query) — the deferred general-expression-key follow-on. A bare
// column resolves to its row slot directly (unchanged); a compound expression is materialized into a
// synthetic window-key column before the window stage. collation / descending / nullsFirst carry the
// same meaning as OrderKey (the latter resolved at parse).
export type WindowOrderKey = {
  expr: Expr;
  // An explicit `COLLATE "name"` on this key; null means the key expression's (text) collation. A
  // COLLATE on a non-text key is 42804; an unknown name is 42704.
  collation: string | null;
  descending: boolean;
  nullsFirst: boolean;
};

// WindowDef is the body of an `OVER (...)` clause (spec/design/window.md §3). Carries an optional
// base-window name, `PARTITION BY`, `ORDER BY`, and a frame clause. Both `partition` and `order` are
// general expressions (`PARTITION BY a + b`, `ORDER BY a % 2`, `ORDER BY sum(x)` in a grouped query —
// spec/design/window.md §5.1); a bare column resolves to its row slot directly, a compound expression
// is materialized into a synthetic window-key column before the window stage.
export type WindowDef = {
  // An optional leading base-window name (`OVER (w ORDER BY …)`, `WINDOW w2 AS (w …)` — §5): the
  // definition extends the named base, inheriting its `PARTITION BY` (and its `ORDER BY` if any) and
  // supplying its own frame. A resolve-time pass (resolveWindowClause / desugarNamedWindows) merges
  // the base in and clears `base` to null, so every definition is inline (`base == null`) at the
  // window stage.
  base?: string | null;
  partition: Expr[];
  order: WindowOrderKey[];
  // An explicit frame clause (`ROWS BETWEEN … AND …`), else null for the default frame
  // (spec/design/window.md §6). S4 supports `ROWS` mode; explicit `RANGE`/`GROUPS` and `EXCLUDE`
  // are parsed but rejected `0A000` at resolve.
  frame?: WindowFrame | null;
};

// A window frame clause (spec/design/window.md §6).
export type WindowFrame = {
  mode: FrameMode;
  start: FrameBound;
  end: FrameBound;
  exclude: FrameExclusion;
};

export type FrameMode = "rows" | "range" | "groups";

// Frame exclusion (EXCLUDE … — spec/design/window.md §6): which rows to drop from the computed
// [lo, hi) frame, per current row. "noOthers" (the default / no EXCLUDE) drops nothing.
export type FrameExclusion = "noOthers" | "currentRow" | "group" | "ties";

// A frame boundary. Preceding/Following carry the offset expression (a non-negative integer
// in ROWS/GROUPS; a value offset in RANGE).
export type FrameBound =
  | { kind: "unboundedPreceding" }
  | { kind: "preceding"; offset: Expr }
  | { kind: "currentRow" }
  | { kind: "following"; offset: Expr }
  | { kind: "unboundedFollowing" };

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
  // An optional DEFAULT <expr> — the value for this column when a row omits it (or uses the
  // DEFAULT keyword). A constant literal is pre-evaluated at CREATE TABLE; any other expression
  // is evaluated per row at INSERT (spec/design/constraints.md §2). null = no default.
  default: DefaultDef | null;
  // An optional `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [( opts )]` constraint
  // (spec/design/sequences.md §13). Desugars like `serial` (an owned sequence + a `nextval` default
  // + NOT NULL) plus the persisted ALWAYS/BY DEFAULT distinction. null = a non-identity column.
  identity: IdentitySpec | null;
  // An optional `COLLATE "name"` column modifier (spec/design/collation.md §1) — a quoted,
  // case-sensitive collation name. Text-only (else 42804); the name must be loaded or "C" (else
  // 42704). null = no clause ⇒ inherit the per-database default. Frozen into the column at CREATE
  // TABLE.
  collation: string | null;
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
  // temp is whether `TEMP` / `TEMPORARY` preceded `TABLE` — a temporary table
  // (spec/design/temp-tables.md). A temp table makes ZERO writes to the database file (it lives
  // outside the serialized snapshot) and is dropped at session / database close. Its DDL is gated by
  // allowTempDdl (session-local) or allowSharedTempDdl (shared) rather than allowDdl (temp-tables.md
  // §5). shared implies temp (a SHARED table is always temporary).
  temp: boolean;
  // shared is whether `SHARED` preceded `TEMP`/`TEMPORARY` — a DATABASE-WIDE shared temporary table
  // (temp-tables.md §4): one set of rows visible to and writable by every session of the open
  // Database, still never written to the file. shared===true always has temp===true (the parser
  // rejects SHARED not followed by TEMP/TEMPORARY as 42601); when false (and temp) the table is
  // session-local.
  shared: boolean;
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
  // Every `FOREIGN KEY (cols) REFERENCES …` of the statement — the column-level
  // `REFERENCES` form collects as a one-member list — in TEXTUAL DEFINITION ORDER (it drives
  // resolution and naming — spec/design/constraints.md §6). CREATE TABLE's execution resolves
  // each (42703/42701/42P01/42830/42804), rejects unsupported actions (0A000), and names the
  // unnamed ones (42710).
  fks: ForeignKeyDef[];
};

// RefAction is a referential action for `ON DELETE` / `ON UPDATE` (spec/design/constraints.md
// §6.6). Only "noAction" (the default) and "restrict" are supported — identical in jed (no
// deferrable constraints); the write-actions parse but are rejected 0A000 at CREATE TABLE.
export type RefAction = "noAction" | "restrict" | "cascade" | "setNull" | "setDefault";

// ForeignKeyDef is one parsed `FOREIGN KEY` / `REFERENCES` constraint (spec/design/grammar.md
// §43): the optional explicit CONSTRAINT name (null = unnamed), the local (referencing) column
// names in list order, the referenced (parent) table name, the optional referenced column names
// (null = the parent's primary key), and the `ON DELETE` / `ON UPDATE` actions. Execution
// resolves it (42703/42701/42P01/42830/42804) and names the unnamed ones (42710) —
// spec/design/constraints.md §6.
export type ForeignKeyDef = {
  name: string | null;
  columns: string[];
  refTable: string;
  refColumns: string[] | null;
  onDelete: RefAction;
  onUpdate: RefAction;
};

// CheckDef is one parsed CHECK constraint (spec/design/grammar.md §29): the optional
// explicit CONSTRAINT name (null = unnamed), the expression, and the expression's
// persisted text — the source token sequence between the parentheses re-rendered per the
// closed table in spec/fileformat/format.md "Check-expression text".
export type CheckDef = { name: string | null; expr: Expr; text: string };

// DefaultDef is a parsed DEFAULT <expr> column constraint (spec/design/constraints.md §2): the
// default expression and its persisted text (the source token sequence re-rendered per the
// closed table in spec/fileformat/format.md "Check-expression text", as a CHECK is). Execution
// classifies it: a bare literal Expr is a constant (pre-evaluated at CREATE TABLE), any other
// expression is stored as text and evaluated per row at INSERT.
export type DefaultDef = { expr: Expr; text: string };

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
  // The `USING <method>` access method as written, or undefined for the default ordered B-tree.
  // Resolved at execution: undefined/"btree" → B-tree, "gin" → GIN, else 42704 (gin.md §3).
  using: string | undefined;
};

// DropIndex is a DROP INDEX <name> statement — remove one secondary index
// (spec/design/indexes.md §2). Missing → 42704; a table's name → 42809.
export type DropIndex = { kind: "dropIndex"; name: string };

// CreateType is a `CREATE TYPE <name> AS ( field type [NOT NULL] [, …] )` statement — a
// user-defined composite (row) type (spec/design/composite.md, grammar.md). Execution resolves
// each field's type (a built-in scalar or a previously-defined composite — 42704 if unknown),
// rejects a duplicate type name (42710), and registers it in the catalog. Named composites only
// this slice; anonymous `record` is not supported.
export type CreateType = {
  kind: "createType";
  name: string;
  fields: TypeFieldDef[];
};

// TypeFieldDef is one field of a CREATE TYPE definition: its name, its type as written (a built-in
// scalar alias or a composite type name), an optional numeric(p,s) modifier, and an explicit
// NOT NULL. Resolved at execution (mirrors ColumnDef).
export type TypeFieldDef = {
  name: string;
  typeName: string;
  typeMod: TypeMod | null;
  notNull: boolean;
};

// DropType is a `DROP TYPE [IF EXISTS] <name> [RESTRICT]` statement — remove a composite type
// (spec/design/composite.md §7). RESTRICT (the default and only behavior this slice) fails with
// 2BP01 if a table column or another composite type still references it; CASCADE is 0A000. A
// missing type without IF EXISTS is 42704.
export type DropType = { kind: "dropType"; name: string; ifExists: boolean };

// SeqOptions is the parsed, order-free sequence-option set shared by CREATE SEQUENCE and an
// IDENTITY column's optional `( seq_options )` (spec/design/sequences.md §13). Each is captured as
// a parsed override, with `null` meaning "use the default" (resolved at execution against the
// INCREMENT sign); execution validates the set (22023). `minValue`/`maxValue` use a nested-override
// form: `{ value: v }` = MINVALUE v; `{ value: null }` = NO MINVALUE (the type default); the outer
// `null` = unset.
export type SeqOptions = {
  // dataType is the `AS <type>` value type as written (the raw type name, e.g. "smallint" /
  // "int4"), resolved to a SeqDataType at execution (spec/design/sequences.md §14); null = the
  // bigint default. A non-integer type is 22023. Inside an IDENTITY column's options a set dataType
  // is 42601 (the column type fixes it).
  dataType: string | null;
  increment: bigint | null;
  minValue: { value: bigint | null } | null;
  maxValue: { value: bigint | null } | null;
  start: bigint | null;
  cache: bigint | null;
  cycle: boolean | null;
};

// emptySeqOptions builds a fresh SeqOptions with every override unset (the all-default sequence).
export function emptySeqOptions(): SeqOptions {
  return {
    dataType: null,
    increment: null,
    minValue: null,
    maxValue: null,
    start: null,
    cache: null,
    cycle: null,
  };
}

// seqOptionsHasAny reports whether any sequence option was written (any field non-null) — used by
// ALTER SEQUENCE to require ≥ 1 action when there is no RESTART (spec/design/sequences.md §15).
export function seqOptionsHasAny(o: SeqOptions): boolean {
  return (
    o.dataType !== null ||
    o.increment !== null ||
    o.minValue !== null ||
    o.maxValue !== null ||
    o.start !== null ||
    o.cache !== null ||
    o.cycle !== null
  );
}

// CreateSequence is a `CREATE SEQUENCE [IF NOT EXISTS] <name> [options]` statement — a named,
// persisted i64 generator (spec/design/sequences.md). Execution validates the option set (22023),
// rejects a relation-namespace collision (42P07 unless `ifNotExists`), and registers the sequence.
export type CreateSequence = {
  kind: "createSequence";
  name: string;
  ifNotExists: boolean;
  options: SeqOptions;
};

// IdentitySpec is a column's `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [( seq_options )]`
// constraint (spec/design/sequences.md §13). `always` distinguishes ALWAYS (true) from BY DEFAULT
// (false); `options` tunes the auto-created owned sequence (defaults to the standard ascending i64).
export type IdentitySpec = { always: boolean; options: SeqOptions };

// DropSequence is a `DROP SEQUENCE [IF EXISTS] <name> [, …] [RESTRICT]` statement — remove one or
// more sequences (spec/design/sequences.md §1). A missing sequence without IF EXISTS is 42P01;
// CASCADE is 0A000 (RESTRICT is the default and only mode this slice).
export type DropSequence = {
  kind: "dropSequence";
  names: string[];
  ifExists: boolean;
};

// SeqRestart is a parsed RESTART pseudo-option on ALTER SEQUENCE (spec/design/sequences.md §15):
// `{ toStart: true }` is a bare RESTART (reset to the stored START); otherwise `value` is RESTART
// WITH n. `null` (no SeqRestart) means RESTART was not written. Mirrors Rust's Option<Option<i64>>.
export type SeqRestart = { toStart: true } | { toStart: false; value: bigint };

// AlterSequence is an `ALTER SEQUENCE [IF EXISTS] <name> <action>` statement (spec/design/sequences.md
// §4/§15). A missing sequence without ifExists is 42P01. The two action forms: `setOptions` re-edits
// the definition (the order-free CREATE options minus AS, plus an interleavable RESTART; the parser
// requires ≥ 1 — a bare ALTER SEQUENCE s is 42601), `rename` moves the catalog key.
export type AlterSequence = {
  kind: "alterSequence";
  name: string;
  ifExists: boolean;
  action:
    | { kind: "setOptions"; options: SeqOptions; restart: SeqRestart | null }
    | { kind: "rename"; newName: string };
};

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
  // The optional `OVERRIDING { SYSTEM | USER } VALUE` clause (spec/design/sequences.md §13),
  // governing IDENTITY columns. null is the default (no override).
  overriding: Overriding | null;
  source: { kind: "values"; rows: InsertValue[][] } | { kind: "select"; select: Select };
  // The optional ON CONFLICT clause (UPSERT — spec/design/upsert.md), between the source and
  // RETURNING. Null = no clause (a conflict traps 23505 as usual).
  onConflict: OnConflict | null;
  // The optional terminal RETURNING clause (spec/design/grammar.md §32): project each stored
  // row, turning the statement into a query result. Null = no clause.
  returning: SelectItems | null;
};

// OnConflict is the `ON CONFLICT [target] action` clause (spec/design/upsert.md §1). `target` is
// null only with DO NOTHING (any uniqueness conflict is then skipped); DO UPDATE with a null
// target is 42601. When `doUpdate` is true, `assignments` (SET …) and `filter` (optional WHERE …)
// apply; for DO NOTHING `assignments` is empty and `filter` null.
export type OnConflict = {
  target: ConflictTarget | null;
  doUpdate: boolean;
  assignments: Assignment[];
  filter: Expr | null;
};

// ConflictTarget is the arbiter constraint named by an ON CONFLICT target (spec/design/upsert.md
// §2): a `( col [, ...] )` inference list matched as a SET against a unique index / the primary key
// (no match → 42P10), or `ON CONSTRAINT name` (a unique-index name or the synthesized <table>_pkey;
// miss → 42704).
export type ConflictTarget =
  | { kind: "columns"; columns: string[] }
  | { kind: "constraint"; name: string };

// Overriding is the INSERT `OVERRIDING { SYSTEM | USER } VALUE` clause (spec/design/sequences.md
// §13): "system" lets an explicit value land in a GENERATED ALWAYS identity column; "user" discards
// a supplied value for any identity column and uses its sequence instead.
export type Overriding = "system" | "user";

// InsertValue is one value slot in an INSERT VALUES row: a literal, or the DEFAULT keyword —
// which substitutes the target column's declared default (or NULL if it has none). The DEFAULT
// keyword is not reserved (spec/design/grammar.md §3). See spec/design/constraints.md §2.
export type InsertValue =
  | { kind: "lit"; lit: Literal }
  // A bind parameter $N (1-based), bound at execute — spec/design/api.md §5.
  | { kind: "param"; index: number }
  // A ROW(…) constructor in a VALUES slot (spec/design/composite.md §1) — a composite value for a
  // composite target column. Fields are themselves InsertValues (a literal, a $N, or a nested
  // ROW(…)); DEFAULT is not a valid field (only a top-level slot takes a default).
  | { kind: "row"; fields: InsertValue[] }
  // An ARRAY[…] constructor in a VALUES slot (spec/design/array.md §1) — an array value for an array
  // target column. Elements are themselves InsertValues (a literal or a $N).
  | { kind: "array"; elements: InsertValue[] }
  | { kind: "default" };

// TableRef is a table reference in a FROM clause: a table name with an optional alias
// (`orders o` or `orders AS o`). The alias, or the table name when there is none, is the
// relation's LABEL — it qualifies columns (o.col) and must be distinct within one query (a
// self-join needs aliases; a duplicate label is 42712). See spec/design/grammar.md §15.
//
// When `args` is non-null the reference is instead a set-returning FUNCTION call used as a row
// source (generate_series(1, 5)): `name` is the function name and `args` its argument
// expressions (the label is then the alias, or the function name when there is none —
// grammar.md §35). `null` = an ordinary base table.
// A `subquery` instead marks a DERIVED TABLE — a parenthesized subquery used as a relation,
// `FROM (SELECT …) [AS] t` (grammar.md §42), mechanically an anonymous always-inlined
// single-reference CTE (the planner reuses the CTE synthetic-relation seam). The alias is OPTIONAL
// (PG 18): present, it is the label and `columnAliases` the optional column-rename list; absent,
// `name` is "" / `alias` is null and the relation has no qualifier.
// A `values` body instead marks a VALUES-body derived table — FROM (VALUES (e11,…),(e21,…)) AS
// v(c1,…) (grammar.md §42): a parenthesized VALUES list used as a relation, a computed relation of
// literal rows. It is the FROM-position alternative body to `subquery` (the two are mutually
// exclusive — at most one is set on a derived table). Each value is a general constant expression
// (resolved parent=null, non-LATERAL unless this TableRef is marked `lateral`); the rows share arity
// and the columns' types unify across rows like a set operation. The outer array is the rows, each
// inner array one row's values.
// `lateral` is set when the FROM item is preceded by the LATERAL keyword (spec/design/grammar.md
// §44): the derived-table body / SRF arguments may then reference columns of the FROM relations that
// appear BEFORE this one (a dependent / correlated join). It is meaningful only for a derived table
// or table function; a table function is implicitly lateral, so the planner correlates an SRF's args
// to the earlier siblings whether or not this flag is set.
export type TableRef = {
  name: string;
  alias: string | null;
  args: Expr[] | null;
  subquery?: QueryExpr;
  values?: Expr[][];
  columnAliases?: string[];
  lateral?: boolean;
};

// JoinKind is the kind of a join. "inner"/"cross" execute this slice; the "left"/"right"/"full"
// outer kinds parse and are carried in the AST but executing one is a documented 0A000
// narrowing (the OUTER family is a fast-follow — spec/design/grammar.md §15).
export type JoinKind = "inner" | "cross" | "left" | "right" | "full";

// JoinClause is one JOIN step in the left-deep FROM chain: the join kind, the right-hand
// table reference, and the optional ON predicate (null for CROSS JOIN; set for INNER/outer,
// which require an ON). See spec/design/grammar.md §15.
export type JoinClause = { kind: JoinKind; table: TableRef; on: Expr | null };

// GroupItem is one GROUP BY grouping term (spec/design/aggregates.md §12). Most queries use only
// "set" with one column each (plain `GROUP BY a, b` → two "set" items); the ROLLUP/CUBE/GROUPING SETS
// forms produce several grouping sets the resolver expands and cross-products. Each Expr is a
// bare/qualified column (the parser enforces it). A "set" with no cols is the empty set `()`.
export type GroupItem =
  | { kind: "set"; cols: Expr[] }
  | { kind: "rollup"; groups: Expr[][] }
  | { kind: "cube"; groups: Expr[][] }
  | { kind: "groupingSets"; elems: GroupItem[] };

// forEachGroupExpr visits every column Expr in a grouping term — used by the analysis walks that
// scan a SELECT's expressions (privilege collection, sublink / sequence-mutator detection).
export function forEachGroupExpr(item: GroupItem, f: (e: Expr) => void): void {
  switch (item.kind) {
    case "set":
      for (const e of item.cols) f(e);
      break;
    case "rollup":
    case "cube":
      for (const g of item.groups) for (const e of g) f(e);
      break;
    case "groupingSets":
      for (const el of item.elems) forEachGroupExpr(el, f);
      break;
  }
}

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
  // GROUP BY grouping terms — a GroupItem per comma-separated term: a plain column set, or the
  // ROLLUP/CUBE/GROUPING SETS forms that expand to multiple grouping sets (spec/design/aggregates.md
  // §12). Empty means no GROUP BY. Every grouping column is a "column"/"qualifiedColumn".
  groupBy: GroupItem[];
  // The HAVING predicate (a boolean filter over the grouped rows), or null. May reference
  // aggregates and grouping keys; evaluated after aggregation, before ORDER BY. HAVING makes a
  // query an aggregate query even with no GROUP BY (spec/design/grammar.md §19).
  having: Expr | null;
  // ORDER BY sort keys, applied left to right; empty means no ORDER BY (grammar.md §10).
  orderBy: OrderKey[];
  limit: bigint | null;
  offset: bigint | null;
  // Named windows from a `WINDOW name AS (definition)` clause (spec/design/window.md §5,
  // grammar.ebnf `window_clause`), referenced by `OVER name`. Empty when absent. Resolved by a
  // desugaring pass that rewrites each `OVER name` to its definition before resolution.
  windows: [string, WindowDef][];
};

// Update is `UPDATE <table> SET ... [WHERE ...]`. Assigning a PRIMARY KEY column re-keys
// the row — the storage key is recomputed and the row moves (see the executor). The WHERE
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
// transactions.md §4.2). Begin's `writable` is the *requested* access mode: true READ WRITE,
// false READ ONLY (a write inside → 25006), null unspecified — which defaults to READ WRITE on
// a normal handle and READ ONLY on a read-only handle (api.md §2.1). A nested BEGIN is 25001; a
// COMMIT/ROLLBACK with no open block is a no-op success.
export type Begin = { kind: "begin"; writable: boolean | null };
export type Commit = { kind: "commit" };
export type Rollback = { kind: "rollback" };

// SetOpKind is the set operator (spec/design/grammar.md §25).
export type SetOpKind = "union" | "intersect" | "except";

// QueryExpr is the operand of a set operation (spec/design/grammar.md §25): a single SELECT core, a
// nested set operation (so a chain like `a UNION b INTERSECT c` forms a tree), or a nested WITH
// clause (spec/design/cte.md §7).
export type QueryExpr = Select | SetOp | WithExpr;

// WithExpr is a nested `WITH … query_expr` (spec/design/cte.md §7): the CTE list `ctes` (forward-only
// visibility; self-referencing when `recursive`) prefixing the inner query expression `body`, in a
// subquery / derived-table / CTE-body position — as opposed to the top-level WithQuery (which may
// prefix a data-modifying primary). The CTEs are visible only within `body` (and to each other); the
// enclosing statement's CTE bindings are NOT inherited — a documented narrowing (cte.md §7). A
// data-modifying CTE here is rejected at planning (0A000 — PostgreSQL restricts a DML-WITH to the
// statement top level).
export type WithExpr = {
  kind: "withExpr";
  ctes: Cte[];
  recursive: boolean;
  body: QueryExpr;
};

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

// CteBody is the body of a CTE, or the WITH-prefixed primary statement (spec/design/writable-cte.md):
// an ordinary query expression (a SELECT or set operation), or a DATA-MODIFYING statement (a writable
// CTE — INSERT/UPDATE/DELETE). The variants are already disjoint by their `kind` tags (Select/SetOp
// vs insert/update/delete), so the union needs no extra wrapper. `cteBodyAsQuery` returns the query
// expression for a plain body (used by the recursive-CTE analysis and the pure-query WITH path);
// `cteBodyIsDataModifying` reports whether a body is an INSERT/UPDATE/DELETE.
export type CteBody = QueryExpr | Insert | Update | Delete;

// cteBodyAsQuery returns the query expression if `body` is a plain query, else null (a
// data-modifying body). Only a query body can be a recursive UNION shape (writable-cte.md §3).
export function cteBodyAsQuery(body: CteBody): QueryExpr | null {
  return body.kind === "select" || body.kind === "setOp" || body.kind === "withExpr" ? body : null;
}

// cteBodyIsDataModifying reports whether `body` is a data-modifying statement (INSERT/UPDATE/DELETE).
export function cteBodyIsDataModifying(body: CteBody): boolean {
  return body.kind === "insert" || body.kind === "update" || body.kind === "delete";
}

// Cte is one common table expression in a WITH list (spec/design/cte.md). A named, statement-local
// relation backed by a query or (spec/design/writable-cte.md) a data-modifying statement. `columns`
// is the optional column-rename list (renames the body's output columns left to right; a count
// mismatch with MORE aliases is 42P10). `materialized` is the explicit evaluation hint: true =
// MATERIALIZED, false = NOT MATERIALIZED, null = PostgreSQL's default (inline a single-reference CTE,
// materialize a multi-reference one — cost.md §3; a data-modifying CTE is always materialized, the
// hint inert). The body is a cte_body.
export type Cte = {
  name: string;
  columns: string[] | null;
  materialized: boolean | null;
  body: CteBody;
};

// WithQuery is a top-level statement prefixed by a WITH clause (spec/design/cte.md). `ctes` is the
// non-empty list of common table expressions (each visible to later CTEs and to `body`); `body` is
// the main statement — a query, or (spec/design/writable-cte.md) a data-modifying INSERT/UPDATE/DELETE
// primary. Built only when a WITH is present — a plain query stays `Select`/`SetOp`, so those paths
// are untouched (the SetOp precedent). `recursive` is the WITH RECURSIVE flag
// (spec/design/recursive-cte.md): a flag on the whole list that ENABLES a CTE to reference itself
// (lifting the forward-only 42P01); a CTE that does not reference itself is still an ordinary
// non-recursive CTE.
export type WithQuery = {
  kind: "with";
  ctes: Cte[];
  body: CteBody;
  recursive: boolean;
};

// Statement is a parsed top-level statement. A lone SELECT stays `Select`; `SetOp` appears only
// when at least one set operator is present, and `With` only when a WITH prefix is present, so the
// plain-query path and host API are untouched.
export type Statement =
  | CreateTable
  | DropTable
  | CreateIndex
  | DropIndex
  | CreateType
  | DropType
  | CreateSequence
  | AlterSequence
  | DropSequence
  | Insert
  | Select
  | SetOp
  | WithQuery
  | Update
  | Delete
  | Begin
  | Commit
  | Rollback;
