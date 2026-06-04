//! Abstract syntax for the step-1 SQL surface. Boring, explicit shapes
//! (CLAUDE.md §10); the hand-written parser produces these.

use crate::decimal::Decimal;

/// A parsed top-level statement.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Statement {
    CreateTable(CreateTable),
    DropTable(DropTable),
    Insert(Insert),
    Select(Select),
    Update(Update),
    Delete(Delete),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CreateTable {
    pub name: String,
    pub columns: Vec<ColumnDef>,
}

/// `DROP TABLE <name>`. Removes a table — its definition and all its rows — from the
/// catalog. Dropping a table that does not exist is an error (42P01); there is no
/// `IF EXISTS` this slice. Single table only; no `CASCADE` / `RESTRICT` (no dependent
/// objects exist yet). See spec/design/grammar.md §13.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DropTable {
    pub name: String,
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
    /// An optional `DEFAULT <literal>` — the value for this column when a row omits it (or
    /// uses the `DEFAULT` keyword). Literal-only this slice; evaluated + type-coerced once at
    /// CREATE TABLE (spec/design/constraints.md §2).
    pub default: Option<Literal>,
}

/// A parsed type modifier: a precision and an optional scale, as written
/// (`numeric(p)` → `scale = None`, `numeric(p,s)` → `scale = Some(s)`). The values are the
/// raw lexed magnitudes; range validation (1..=1000, 0..=p; else 22023) is at resolve.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TypeMod {
    pub precision: u64,
    pub scale: Option<u64>,
}

/// `INSERT INTO <table> [(col, ..)] VALUES (..)[, (..)]*`. One or more rows of values,
/// each value either a literal or the `DEFAULT` keyword. A multi-row INSERT is two-phase /
/// all-or-nothing — every row is validated before any is stored (spec/design/grammar.md §12).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Insert {
    pub table: String,
    /// An optional explicit column list (`INSERT INTO t (a, c) VALUES ...`). `None` is the
    /// positional form — every column, in declaration order. Names resolve at execution time
    /// (unknown → 42703, duplicate → 42701); an unlisted column takes its default else NULL
    /// (spec/design/constraints.md §2).
    pub columns: Option<Vec<String>>,
    /// The rows to insert, in statement order; each inner vec is one row's values in the
    /// order of `columns` (or column order when `columns` is `None`). Always non-empty.
    pub rows: Vec<Vec<InsertValue>>,
}

/// One value slot in an INSERT `VALUES` row: a literal, or the `DEFAULT` keyword — which
/// substitutes the target column's declared default (or NULL if it has none). The `DEFAULT`
/// keyword is not reserved (spec/design/grammar.md §3). See spec/design/constraints.md §2.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InsertValue {
    Lit(Literal),
    Default,
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
}

/// A table reference in a FROM clause: a table name with an optional alias (`orders o`
/// or `orders AS o`). The alias, or the table name when there is none, is the relation's
/// **label** — it qualifies columns (`o.col`) and must be distinct within one query
/// (a self-join needs aliases; a duplicate label is `42712`). See spec/design/grammar.md §15.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
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
    /// The first table reference of the FROM clause.
    pub from: TableRef,
    /// Zero or more left-deep JOINs after `from` (empty = a single-table SELECT).
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
    /// An aggregate function call — the engine's first function-call syntax (grammar.md §17).
    /// `name` is the spelling as written (resolved case-insensitively against the aggregate
    /// catalog; an unknown name is 42883). `star` is the `COUNT(*)` row-count form (then `arg`
    /// is `None`); otherwise `arg` is the single argument expression. DISTINCT inside the
    /// parens is rejected at parse (42601). Only aggregates resolve this slice; an aggregate
    /// in WHERE/ON or nested in another aggregate is 42803 (spec/design/aggregates.md).
    FuncCall {
        name: String,
        arg: Option<Box<Expr>>,
        star: bool,
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
