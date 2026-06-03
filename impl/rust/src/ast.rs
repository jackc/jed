//! Abstract syntax for the step-1 SQL surface. Boring, explicit shapes
//! (CLAUDE.md §10); the hand-written parser produces these.

/// A parsed top-level statement.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Statement {
    CreateTable(CreateTable),
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

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ColumnDef {
    pub name: String,
    /// The type name as written (canonical or alias); resolved during analysis.
    pub type_name: String,
    pub primary_key: bool,
}

/// `INSERT INTO <table> VALUES (..)[, (..)]*`. One or more rows of literal values,
/// each in column order. A multi-row INSERT is two-phase / all-or-nothing — every
/// row is validated before any is stored (spec/design/grammar.md §12).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Insert {
    pub table: String,
    /// The rows to insert, in statement order; each inner vec is one row's literal
    /// values in column order. Always non-empty (the parser requires ≥1 row).
    pub rows: Vec<Vec<Literal>>,
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

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Select {
    /// `SELECT DISTINCT` — deduplicate the projected output rows (NULL-safe), applied
    /// after ORDER BY and before LIMIT/OFFSET (spec/design/grammar.md §11).
    pub distinct: bool,
    /// Projected expressions, or `*` for all (`SelectItems::All`).
    pub items: SelectItems,
    pub from: String,
    /// The WHERE expression (must resolve to boolean), if any.
    pub filter: Option<Expr>,
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
    Literal(Literal),
    Cast {
        inner: Box<Expr>,
        type_name: String,
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
}
