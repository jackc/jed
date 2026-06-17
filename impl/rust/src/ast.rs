//! Abstract syntax for the step-1 SQL surface. Boring, explicit shapes
//! (CLAUDE.md §10); the hand-written parser produces these.

use crate::decimal::Decimal;

/// A parsed top-level statement.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Statement {
    CreateTable(CreateTable),
    DropTable(DropTable),
    CreateIndex(CreateIndex),
    DropIndex(DropIndex),
    CreateType(CreateType),
    DropType(DropType),
    Insert(Insert),
    Select(Select),
    /// A set operation (`UNION`/`INTERSECT`/`EXCEPT`) combining two query expressions
    /// (spec/design/grammar.md §25). A lone `SELECT` stays `Statement::Select` — this variant
    /// appears only when at least one set operator is present, so the plain-query path and the
    /// host API are untouched.
    SetOp(SetOp),
    /// A query prefixed by a `WITH` clause defining one or more common table expressions
    /// (spec/design/cte.md). Appears only when a `WITH` is present; a plain query stays
    /// `Select`/`SetOp`, so the host API and the no-CTE paths are untouched.
    With(WithQuery),
    Update(Update),
    Delete(Delete),
    /// `BEGIN [TRANSACTION|WORK] [READ ONLY|READ WRITE]` / `START TRANSACTION [...]` — open an
    /// explicit transaction block (spec/design/grammar.md §27). `writable` is the *requested*
    /// access mode: `Some(true)` READ WRITE, `Some(false)` READ ONLY, `None` unspecified —
    /// which defaults to READ WRITE on a normal handle and READ ONLY on a read-only handle
    /// (api.md §2.1; a write inside a READ ONLY block → 25006). A nested `BEGIN` is 25001
    /// (transactions.md §4.2).
    Begin {
        writable: Option<bool>,
    },
    /// `COMMIT [TRANSACTION|WORK]` / `END [...]` — publish the open block durably and return to
    /// autocommit; a `COMMIT` with no open block is a no-op success (transactions.md §4.2).
    Commit,
    /// `ROLLBACK [TRANSACTION|WORK]` — discard the open block's working set and return to
    /// autocommit; a `ROLLBACK` with no open block is a no-op success (transactions.md §4.2).
    Rollback,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CreateTable {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    /// The table-level `PRIMARY KEY (a, b, …)` constraints, each a list of member column
    /// names in key order (spec/design/grammar.md §28). The parser collects every one it
    /// sees; CREATE TABLE's execution resolves them (42703/42701) and rejects more than one
    /// primary key across both forms (42P16) — spec/design/constraints.md §3.
    pub table_pks: Vec<Vec<String>>,
    /// Every `[CONSTRAINT name] CHECK ( expr )` of the statement — column-level and
    /// table-level forms are semantically identical, so both collect here, in **textual
    /// definition order** (it drives validation and naming — spec/design/constraints.md §4).
    /// CREATE TABLE's execution validates each (0A000/42803/42P02/42703/42804) and names
    /// the unnamed ones (42710 on a collision).
    pub checks: Vec<CheckDef>,
    /// Every `[CONSTRAINT name] UNIQUE [(cols)]` of the statement — the column-level form
    /// collects as a one-member list — in **textual definition order** (it drives member
    /// resolution, the dedup/PK fold, and naming — spec/design/constraints.md §5). Each
    /// survivor becomes a unique secondary index (spec/design/indexes.md §8).
    pub uniques: Vec<UniqueDef>,
}

/// One parsed `UNIQUE` constraint (spec/design/grammar.md §31): the optional explicit
/// `CONSTRAINT` name (it names the backing index) and the member column names in list
/// order. Execution resolves the members (42703/42701/0A000) and names the index
/// (42P07/42710) — spec/design/constraints.md §5.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UniqueDef {
    pub name: Option<String>,
    pub columns: Vec<String>,
}

/// One parsed `CHECK` constraint (spec/design/grammar.md §29): the optional explicit
/// `CONSTRAINT` name, the expression, and the expression's persisted text — the source
/// token sequence between the parentheses re-rendered per the closed table in
/// spec/fileformat/format.md "Check-expression text".
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CheckDef {
    pub name: Option<String>,
    pub expr: Expr,
    pub text: String,
}

/// A parsed `DEFAULT <expr>` column constraint (spec/design/constraints.md §2): the default
/// expression and its persisted text (the source token sequence re-rendered per the closed
/// table in spec/fileformat/format.md "Check-expression text", as a `CHECK` is). Execution
/// classifies it: a bare `Expr::Literal` is a constant (pre-evaluated at CREATE TABLE), any
/// other expression is stored as text and evaluated per row at INSERT.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DefaultDef {
    pub expr: Expr,
    pub text: String,
}

/// `DROP TABLE <name>`. Removes a table — its definition and all its rows — from the
/// catalog. Dropping a table that does not exist is an error (42P01); there is no
/// `IF EXISTS` this slice. Single table only; no `CASCADE` / `RESTRICT` (no dependent
/// objects exist yet). See spec/design/grammar.md §13.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DropTable {
    pub name: String,
}

/// `CREATE [UNIQUE] INDEX [name] ON <table> ( col [, col]* )` — a secondary index
/// (spec/design/indexes.md, grammar.md §30). `name: None` is the unnamed form; the
/// executor derives PostgreSQL's auto-name. Key columns are bare names (no expression /
/// ordered / partial keys this slice); a column may repeat (PG allows it). Execution
/// validates in PG's order: table 42P01, columns 42703/0A000, name collision 42P07. A
/// `unique` index additionally verifies the existing rows at build (23505) and enforces
/// uniqueness thereafter (spec/design/indexes.md §8).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CreateIndex {
    pub name: Option<String>,
    pub table: String,
    pub columns: Vec<String>,
    pub unique: bool,
}

/// `DROP INDEX <name>` — remove one secondary index (spec/design/indexes.md §2).
/// Missing → 42704; a table's name → 42809.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DropIndex {
    pub name: String,
}

/// `CREATE TYPE <name> AS ( field type [NOT NULL] [, …] )` — a user-defined composite (row)
/// type (spec/design/composite.md, grammar.md). Execution resolves each field's type (built-in
/// scalar or a previously-defined composite — 42704 if unknown), rejects a duplicate type name
/// (42710), and registers it in the catalog. Named composites only this slice; anonymous
/// `record` is not supported.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CreateType {
    pub name: String,
    pub fields: Vec<TypeFieldDef>,
}

/// One field of a `CREATE TYPE` definition: its name, its type as written (a built-in scalar
/// alias or a composite type name), an optional `numeric(p,s)` modifier, and an explicit
/// `NOT NULL`. Resolved at execution (mirrors `ColumnDef`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TypeFieldDef {
    pub name: String,
    pub type_name: String,
    pub type_mod: Option<TypeMod>,
    pub not_null: bool,
}

/// `DROP TYPE [IF EXISTS] <name> [RESTRICT]` — remove a composite type (spec/design/composite.md
/// §7). RESTRICT (the default and only behavior this slice) fails with 2BP01 if a table column
/// or another composite type still references it; `CASCADE` is `0A000`. A missing type without
/// `IF EXISTS` is 42704.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DropType {
    pub name: String,
    pub if_exists: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ColumnDef {
    pub name: String,
    /// The type name as written (canonical or alias); resolved during analysis.
    pub type_name: String,
    /// An optional parenthesized type modifier, `numeric(p[,s])` — the first parameterized
    /// type. Meaningful only for decimal; validated at resolve (spec/design/grammar.md §14).
    pub type_mod: Option<TypeMod>,
    pub primary_key: bool,
    /// An explicit `NOT NULL` column constraint. A PRIMARY KEY column is implicitly NOT NULL
    /// regardless of this flag; the executor ORs the two (spec/design/constraints.md).
    pub not_null: bool,
    /// An optional `DEFAULT <expr>` — the value for this column when a row omits it (or uses
    /// the `DEFAULT` keyword). A constant literal is pre-evaluated at CREATE TABLE; any other
    /// expression is evaluated per row at INSERT (spec/design/constraints.md §2).
    pub default: Option<DefaultDef>,
}

/// A parsed type modifier: a precision and an optional scale, as written
/// (`numeric(p)` → `scale = None`, `numeric(p,s)` → `scale = Some(s)`). The values are the
/// raw lexed magnitudes; range validation (1..=1000, 0..=p; else 22023) is at resolve.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TypeMod {
    pub precision: u64,
    pub scale: Option<u64>,
}

/// `INSERT INTO <table> [(col, ..)] ( VALUES (..)[, (..)]* | <select> )`. The rows come from
/// either a `VALUES` list (each value a literal or the `DEFAULT` keyword) or a `SELECT`
/// (spec/design/grammar.md §24). An INSERT is two-phase / all-or-nothing — every row is
/// validated before any is stored (spec/design/grammar.md §12).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Insert {
    pub table: String,
    /// An optional explicit column list (`INSERT INTO t (a, c) VALUES ...` / `... SELECT ...`).
    /// `None` is the positional form — every column, in declaration order. Names resolve at
    /// execution time (unknown → 42703, duplicate → 42701); an unlisted column takes its default
    /// else NULL (spec/design/constraints.md §2).
    pub columns: Option<Vec<String>>,
    /// Where the rows come from: a `VALUES` list or a `SELECT`.
    pub source: InsertSource,
    /// The optional terminal `RETURNING` clause (spec/design/grammar.md §32): project each
    /// stored row, turning the statement into a query result. `None` = no clause.
    pub returning: Option<SelectItems>,
}

/// The source of an INSERT's rows (spec/design/grammar.md §24): a literal `VALUES` list, or a
/// query whose result rows are inserted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InsertSource {
    /// `VALUES (..)[, (..)]*` — one or more rows, in statement order; each inner vec is one
    /// row's values in the order of `columns` (or column order when `columns` is `None`).
    /// Always non-empty.
    Values(Vec<Vec<InsertValue>>),
    /// `SELECT ...` — the rows the query produces, in its output order. Boxed to keep `Insert`
    /// (and so `Statement`) small, since the SELECT source is the rarer form.
    Select(Box<Select>),
}

/// One value slot in an INSERT `VALUES` row: a literal, a bind parameter (`$N`, bound at
/// execute time — spec/design/api.md §5), or the `DEFAULT` keyword — which substitutes the
/// target column's declared default (or NULL if it has none). The `DEFAULT` keyword is not
/// reserved (spec/design/grammar.md §3). See spec/design/constraints.md §2.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InsertValue {
    Lit(Literal),
    /// A bind parameter `$N` (1-based); typed against the target column at resolve.
    Param(u32),
    Default,
    /// A `ROW(…)` constructor in a VALUES slot (spec/design/composite.md §1) — a composite value
    /// for a composite target column. Fields are themselves `InsertValue`s (a literal, a `$N`, or a
    /// nested `ROW(…)`); `DEFAULT` is not a valid field (only a top-level slot takes a default).
    Row(Vec<InsertValue>),
    /// An `ARRAY[…]` constructor in a VALUES slot (spec/design/array.md §1) — an array value for an
    /// array target column. Elements are themselves `InsertValue`s (a literal or a `$N`).
    Array(Vec<InsertValue>),
}

/// `UPDATE <table> SET <col> = <expr> [, ...] [WHERE <expr>]`. Each assignment's
/// right-hand side is evaluated against the *pre-update* row (so `SET a = b, b = a`
/// swaps). Assigning a PRIMARY KEY column is rejected this slice (the storage key must
/// not change — see the executor). The WHERE expression must resolve to boolean.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Update {
    pub table: String,
    pub assignments: Vec<Assignment>,
    pub filter: Option<Expr>,
    /// The optional terminal `RETURNING` clause (spec/design/grammar.md §32): project each
    /// matched row's NEW (post-assignment) values.
    pub returning: Option<SelectItems>,
}

/// One `SET <column> = <expr>` clause.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Assignment {
    pub column: String,
    pub value: Expr,
}

/// `DELETE FROM <table> [WHERE <expr>]`. No WHERE deletes every row; the WHERE
/// expression must resolve to boolean.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Delete {
    pub table: String,
    pub filter: Option<Expr>,
    /// The optional terminal `RETURNING` clause (spec/design/grammar.md §32): project each
    /// deleted row's OLD values.
    pub returning: Option<SelectItems>,
}

/// A table reference in a FROM clause: a table name with an optional alias (`orders o`
/// or `orders AS o`). The alias, or the table name when there is none, is the relation's
/// **label** — it qualifies columns (`o.col`) and must be distinct within one query
/// (a self-join needs aliases; a duplicate label is `42712`). See spec/design/grammar.md §15.
///
/// When `args` is `Some`, the reference is instead a **set-returning function** call used as a
/// row source (`generate_series(1, 5)`): `name` is the function name and `args` its argument
/// expressions. The label is then the alias, or the function name when there is none
/// (spec/design/grammar.md §35). `None` = an ordinary base table.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
    pub args: Option<Vec<Expr>>,
}

/// The kind of a join. `Inner` and `Cross` execute this slice; the `Left`/`Right`/`Full`
/// outer kinds parse and are carried in the AST but executing one is a documented `0A000`
/// narrowing (the OUTER family is a fast-follow — spec/design/grammar.md §15).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinKind {
    Inner,
    Cross,
    Left,
    Right,
    Full,
}

/// One `JOIN` step in the left-deep FROM chain: the join kind, the right-hand table
/// reference, and the optional `ON` predicate (`None` for `CROSS JOIN`; `Some(expr)` for
/// the `INNER`/outer kinds, which require an `ON`). See spec/design/grammar.md §15.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JoinClause {
    pub kind: JoinKind,
    pub table: TableRef,
    pub on: Option<Expr>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Select {
    /// `SELECT DISTINCT` — deduplicate the projected output rows (NULL-safe), applied
    /// after ORDER BY and before LIMIT/OFFSET (spec/design/grammar.md §11).
    pub distinct: bool,
    /// Projected expressions, or `*` for all (`SelectItems::All`).
    pub items: SelectItems,
    /// The first table reference of the FROM clause, or `None` for a FROM-less SELECT —
    /// the select list evaluates over one virtual zero-column row (spec/design/grammar.md §34).
    pub from: Option<TableRef>,
    /// Zero or more left-deep JOINs after `from` (empty = a single-table SELECT; always
    /// empty when `from` is `None` — joins exist only inside a FROM clause).
    /// spec/design/grammar.md §15.
    pub joins: Vec<JoinClause>,
    /// The WHERE expression (must resolve to boolean), if any.
    pub filter: Option<Expr>,
    /// GROUP BY keys — bare or qualified table columns (never expressions/aliases/ordinals);
    /// empty means no GROUP BY. Each is a `Column` or `QualifiedColumn` (the parser restricts
    /// it to `column_ref`). With keys present the query groups (spec/design/grammar.md §18).
    pub group_by: Vec<Expr>,
    /// The HAVING predicate (a boolean filter over the grouped rows), if any. May reference
    /// aggregates and grouping keys; evaluated after aggregation, before ORDER BY. HAVING makes
    /// a query an aggregate query even with no GROUP BY (spec/design/grammar.md §19).
    pub having: Option<Expr>,
    /// ORDER BY sort keys, applied left to right; empty means no ORDER BY
    /// (spec/design/grammar.md §10).
    pub order_by: Vec<OrderKey>,
    /// `LIMIT n` — cap the result at `n` rows (a non-negative count). Applied after
    /// ORDER BY, before projection (spec/design/grammar.md §9).
    pub limit: Option<i64>,
    /// `OFFSET m` — skip the first `m` rows of the result (a non-negative count).
    pub offset: Option<i64>,
}

/// A query expression — the operand of a set operation (spec/design/grammar.md §25). Either a
/// single `SELECT` core or a nested set operation, so chains like `a UNION b INTERSECT c` form a
/// tree. Boxed at each arm to keep the recursive type sized.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum QueryExpr {
    Select(Box<Select>),
    SetOp(Box<SetOp>),
}

/// The three set operators (spec/design/grammar.md §25).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
}

/// A set operation combining two query expressions (spec/design/grammar.md §25). `all` is the
/// `ALL` (multiset) flag — `false` is the deduplicating default. The optional trailing
/// `ORDER BY` / `LIMIT` / `OFFSET` apply to the WHOLE combined result and live on the OUTERMOST
/// node only (an operand carries none — a deferred narrowing); `order_by` keys resolve against
/// the output column names (the left operand's). Precedence is handled by the parser:
/// `INTERSECT` binds tighter than `UNION`/`EXCEPT`, which are left-associative.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SetOp {
    pub op: SetOpKind,
    pub all: bool,
    pub lhs: QueryExpr,
    pub rhs: QueryExpr,
    /// Trailing ORDER BY over the combined result (empty = none); keys resolve by output name.
    pub order_by: Vec<OrderKey>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// One common table expression in a `WITH` list (spec/design/cte.md). A named, statement-local
/// relation backed by a query. `columns` is the optional column-rename list (renames the body's
/// output columns left to right; a count mismatch is 42P10). `materialized` is the explicit
/// evaluation hint: `Some(true)` = `MATERIALIZED`, `Some(false)` = `NOT MATERIALIZED`, `None` =
/// PostgreSQL's default (inline a single-reference CTE, materialize a multi-reference one —
/// cost.md §3). The body is any `query_expr` (a SELECT or a set operation).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cte {
    pub name: String,
    pub columns: Option<Vec<String>>,
    pub materialized: Option<bool>,
    pub query: QueryExpr,
}

/// A top-level query prefixed by a `WITH` clause (spec/design/cte.md). `ctes` is the non-empty
/// list of common table expressions (each visible to later CTEs and to `body`); `body` is the
/// main query expression. Built only when a `WITH` is present — a plain query stays
/// `Statement::Select`/`Statement::SetOp`, so those paths are untouched (the `SetOp` precedent).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WithQuery {
    pub ctes: Vec<Cte>,
    pub body: QueryExpr,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SelectItems {
    All,
    /// Projected expressions, one per output column.
    Items(Vec<SelectItem>),
}

/// One select-list expression with its optional output-name alias (`expr AS name`).
/// The alias is an output label only — it never enters resolution (spec/design/grammar.md
/// §8). The output name when `alias` is `None` is derived by the resolver: a bare column's
/// canonical name, or the fixed `?column?` for any other expression.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SelectItem {
    pub expr: Expr,
    pub alias: Option<String>,
}

/// One subscript spec inside an [`Expr::Subscript`] (spec/design/array.md §6): an index `[i]` or a
/// slice `[m:n]`. A slice's lower/upper bound may be omitted (`[:n]`, `[m:]`, `[:]`), defaulting to
/// the array's own lower / upper bound at evaluation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SubscriptSpec {
    Index(Expr),
    Slice(Option<Expr>, Option<Expr>),
}

/// A general expression, shared by the SELECT list, WHERE, and UPDATE ... SET. The
/// productions are layered by precedence in the parser (spec/grammar/grammar.ebnf
/// `expr`); this is the flat resulting tree. A comparison/logical/null-test node is
/// boolean-valued; arithmetic and columns/integer-literals are integer-valued.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Expr {
    Column(String),
    /// A qualified column reference `rel.col`, where `rel` is a relation label in the FROM
    /// clause (its alias, else its table name). Resolved against exactly that one relation —
    /// never ambiguous (spec/design/grammar.md §15). Bare `Column` stays the unqualified form.
    QualifiedColumn {
        qualifier: String,
        name: String,
    },
    Literal(Literal),
    /// A `ROW(e1, e2, …)` composite constructor (spec/design/composite.md §1). Builds a row value
    /// from the field expressions; `ROW(x)` is a one-field row, `ROW()` the zero-field row. The
    /// bare parenthesized `(a, b)` form is deferred (`0A000`) — only the keyword form is parsed.
    Row(Vec<Expr>),
    /// An `ARRAY[e1, e2, …]` array constructor (spec/design/array.md §1). Builds a 1-D array value
    /// from the element expressions, unified to a common element type at resolve; `ARRAY[]` is the
    /// empty array (its element type comes from an enclosing cast/column context).
    Array(Vec<Expr>),
    /// Field selection `(expr).field` (spec/design/composite.md §S4) — the value of one named field
    /// of a composite `base`. The parser produces this for a `.name` postfix on a parenthesized /
    /// `ROW(…)` / cast / qualified-column base; a bare `a.b` stays `QualifiedColumn` and only falls
    /// back to field access at resolve when `a` is no relation but a composite column (the ambiguity
    /// rule — table.column first, then column.field). Field lookup is case-insensitive; an unknown
    /// field is 42703, a non-composite base 42809.
    FieldAccess {
        base: Box<Expr>,
        field: String,
    },
    /// Whole-row expansion `(expr).*` (spec/design/composite.md §S4) — expands a composite `base`
    /// into one output column per field, in declaration order. Valid only in a SELECT/RETURNING
    /// projection list (where `*` expands); in any scalar expression position it is 42601.
    FieldStar {
        base: Box<Expr>,
    },
    /// Array subscript `base[..][..]` (spec/design/array.md §6) — one or more bracketed specs
    /// applied to an array `base`. Each spec is an index `[i]` or a slice `[m:n]` (with optionally-
    /// omitted bounds: `[:n]`, `[m:]`, `[:]`). All-index access reads a single **1-based** element
    /// (the element type); if any spec is a slice the access returns a sub-array (the array type),
    /// and a scalar index `i` then means `1:i` (PG). An out-of-bounds / NULL subscript yields NULL
    /// (PG, not an error); subscripting a non-array base is 42804 at resolve. The parser collects
    /// consecutive `[…]` postfixes on any base into one node (so `a[1][2]` is one access, two specs).
    Subscript {
        base: Box<Expr>,
        subscripts: Vec<SubscriptSpec>,
    },
    /// A typed string literal `type '...'` (spec/design/grammar.md §36) — PostgreSQL's
    /// `type 'string'` form, equal to `CAST('string' AS type)` over a string-literal operand.
    /// `type_name` names the target scalar (resolved by `ScalarType::from_name`; unknown → 42704)
    /// and `text` is the literal's string. The keyword names the type, so the literal carries it in
    /// any expression position (`SELECT INTERVAL '1 day'`, `SELECT INTEGER '42'`); the string is
    /// coerced to the type at resolve — 22P02 malformed / 22003 out of range / the type's parse code.
    TypedLiteral {
        type_name: String,
        text: String,
    },
    /// A bind parameter `$N` (1-based index). Like an integer/string literal it is an
    /// *adaptable* operand: its type is inferred from context at resolve (sibling operand,
    /// target column, or CAST target), and the host binds a value at execute time
    /// (spec/design/api.md §5). An indeterminate type is 42P18.
    Param(u32),
    Cast {
        inner: Box<Expr>,
        type_name: String,
        /// An optional `numeric(p[,s])` type modifier on the CAST target.
        type_mod: Option<TypeMod>,
    },
    Unary {
        op: UnaryOp,
        operand: Box<Expr>,
    },
    Binary {
        op: BinaryOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    IsNull {
        operand: Box<Expr>,
        negated: bool,
    },
    /// `lhs IS [NOT] DISTINCT FROM rhs` — NULL-safe equality. `negated` carries the NOT
    /// keyword: `negated = true` is `IS NOT DISTINCT FROM` (NULL-safe `=`); `false` is
    /// `IS DISTINCT FROM` (its negation). Always boolean-valued, never unknown
    /// (spec/design/functions.md §3).
    IsDistinctFrom {
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        negated: bool,
    },
    /// `lhs IN (list)` / `lhs NOT IN (list)` — membership over a non-empty value list
    /// (grammar.md §20). Desugared at resolve into the OR-chain PostgreSQL defines it as
    /// (`x IN (a,b)` ≡ `x = a OR x = b`; `NOT IN` is its negation), so the three-valued NULL
    /// semantics and per-element operand typing are inherited from `=`/OR/NOT. The parser
    /// guarantees `list` is non-empty (`IN ()` is 42601).
    In {
        lhs: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `lhs BETWEEN lo AND hi` / `lhs NOT BETWEEN lo AND hi` — range test (grammar.md §21).
    /// Desugared at resolve into `lhs >= lo AND lhs <= hi` (NOT BETWEEN negates), inheriting the
    /// three-valued NULL semantics from the comparisons and Kleene AND. The bounds are parsed at
    /// the additive level so the structural `AND` is not the logical connective.
    Between {
        lhs: Box<Expr>,
        lo: Box<Expr>,
        hi: Box<Expr>,
        negated: bool,
    },
    /// `lhs LIKE rhs` / `lhs NOT LIKE rhs` — text pattern match (grammar.md §22). `%` matches
    /// any run of characters, `_` one code point, with the default `\` escape. Both operands
    /// must be text; NULL propagates. A genuine operator (not desugared) with a hand-written
    /// matcher. `negated` carries the NOT keyword.
    Like {
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        negated: bool,
    },
    /// A `CASE` expression (grammar.md §23). Searched form: `operand` is `None`, each `whens`
    /// condition must be boolean. Simple form: `operand` is `Some(x)`, each branch matches when
    /// `x = val`. `whens` is `(condition_or_value, result)` pairs (≥1). `els` is the `ELSE`
    /// result, or `None` for an implicit `ELSE NULL`. Lazily evaluated: the first TRUE branch
    /// wins; result-arm types unify to a common type.
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        els: Option<Box<Expr>>,
    },
    /// A function call — the shared aggregate/scalar call syntax (grammar.md §17). `name` is
    /// the spelling as written, resolved case-insensitively: an aggregate (COUNT/SUM/MIN/MAX/
    /// AVG, kind = "aggregate"), a scalar function (abs/round, kind = "function",
    /// spec/design/functions.md §9), or 42883 (undefined_function). `star` is the `COUNT(*)`
    /// row-count form (then `args` is empty); otherwise `args` is the comma-separated argument
    /// list — aggregates and `abs` take one, `round` one or two. DISTINCT inside the parens is
    /// rejected at parse (42601). An aggregate in WHERE/ON or nested in another aggregate is
    /// 42803 (spec/design/aggregates.md); a scalar function is legal anywhere an expression is.
    /// `arg_names` carries PostgreSQL named notation (`name => value`, grammar.md §17): `None`
    /// ⇒ every argument positional (the common case — no allocation, and the hot `Expr` enum
    /// stays small); `Some(boxed)` is a per-argument name vector parallel to `args` (`Some(name)`
    /// for a named slot, `None` for a positional one). Boxed so a plain call does not grow `Expr`.
    /// The parser enforces that no positional arg follows a named one.
    FuncCall {
        name: String,
        args: Vec<Expr>,
        arg_names: Option<Box<Vec<Option<String>>>>,
        star: bool,
        /// `true` when the final argument was prefixed with the `VARIADIC` keyword
        /// (`num_nulls(VARIADIC arr)`, array-functions.md §12 / grammar.md §17): the array is
        /// passed directly to a variadic parameter rather than spreading individual arguments.
        /// `false` for every ordinary call (the all-positional/spread fast path).
        variadic: bool,
    },
    /// A scalar subquery `( query_expr )` in expression position (grammar.md §26). `resolve`
    /// plans it once against the scope chain; an uncorrelated one is then folded to a constant,
    /// a correlated one is re-executed per outer row. A `$N` inside is a `0A000`.
    ScalarSubquery(Box<QueryExpr>),
    /// `EXISTS ( query_expr )` (grammar.md §26) — the bare existence predicate (a leading `NOT`
    /// is the ordinary unary connective wrapping this).
    Exists(Box<QueryExpr>),
    /// `lhs [NOT] IN ( query_expr )` (grammar.md §26) — membership of `lhs` in the subquery's
    /// single output column (three-valued, like a literal `IN`).
    InSubquery {
        lhs: Box<Expr>,
        query: Box<QueryExpr>,
        negated: bool,
    },
    /// `lhs op ANY/SOME/ALL ( array )` — a quantified array comparison (grammar.md §41,
    /// array-functions.md §11). `op` is a comparison (`= < > <= >=`); `all` is true for `ALL`,
    /// false for `ANY`/`SOME` (SOME folds to ANY at parse). The array operand resolves to an
    /// array type; the three-valued fold over its flattened elements reuses the `IN`-list
    /// membership semantics (`x = ANY(arr)` ≡ `x IN (the elements)`), generalized to all five
    /// operators and both quantifiers. The subquery form `op ANY(SELECT …)` is a deferred 0A000.
    Quantified {
        op: BinaryOp,
        all: bool,
        lhs: Box<Expr>,
        array: Box<Expr>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum UnaryOp {
    /// Arithmetic negation `-x`.
    Neg,
    /// Logical negation `NOT x`.
    Not,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BinaryOp {
    // arithmetic (integer operands → promoted integer result)
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    // comparison (integer operands → boolean result)
    Eq,
    Lt,
    Gt,
    Le,
    Ge,
    // logical (boolean operands → boolean result, Kleene)
    And,
    Or,
    // array concatenation `||` (spec/design/array-functions.md §8): array∥array (array_cat),
    // array∥element (array_append), element∥array (array_prepend). Resolved polymorphically.
    Concat,
    // array containment / overlap (spec/design/array-functions.md §10): `@>` contains, `<@`
    // contained-by, `&&` overlaps. Each `anyarray <op> anyarray → boolean`, resolved polymorphically.
    Contains,
    ContainedBy,
    Overlaps,
}

/// One ORDER BY sort key: a bare table column, a sort direction, and a resolved NULL
/// placement. `nulls_first` is resolved at parse time — an explicit `NULLS FIRST|LAST`,
/// else the direction default (`descending`: ASC → last, DESC → first, the PostgreSQL
/// model where NULL is the largest value) — so the executor never re-derives it. Placement
/// is applied independently of the `descending` value flip (spec/design/grammar.md §10).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OrderKey {
    /// An optional relation qualifier (`ORDER BY t.a`); `None` is a bare column.
    pub qualifier: Option<String>,
    pub column: String,
    pub descending: bool,
    pub nulls_first: bool,
}

/// A literal value as written in SQL.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Literal {
    Null,
    /// An integer literal. Stored as i64 (the parser folds a leading unary minus into
    /// the value). A bare integer literal is an *untyped constant* that adapts to its
    /// context — the target column on INSERT/UPDATE, a sibling operand, the compared
    /// column in a WHERE predicate — and traps 22003 if it does not fit; with no
    /// context it defaults to int64. See spec/design/types.md §6.
    Int(i64),
    /// A boolean literal, `TRUE` or `FALSE`. boolean is expression-only this slice
    /// (spec/design/types.md §1): a boolean literal is well-formed but cannot be
    /// stored in a column.
    Bool(bool),
    /// A single-quoted text literal (decoded content). Its type is always `text`
    /// (the one collation, `C`); it does not adapt to context like an integer literal
    /// does (spec/design/types.md §11).
    Text(String),
    /// A decimal literal (a numeric literal with a `.`). Carries the constructed value (the
    /// parser folds a leading unary minus into its sign); it is an untyped decimal constant
    /// that adapts to context and whose precision/scale caps are checked at resolve
    /// (spec/design/grammar.md §14, decimal.md §6).
    Decimal(Decimal),
}
