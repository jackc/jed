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

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Insert {
    pub table: String,
    /// One row's worth of literal values, in column order.
    pub values: Vec<Literal>,
}

/// `UPDATE <table> SET <col> = <operand> [, ...] [WHERE <predicate>]`. Each
/// assignment's right-hand side is evaluated against the *pre-update* row (so
/// `SET a = b, b = a` swaps). Assigning a PRIMARY KEY column is rejected this slice
/// (the storage key must not change — see the executor).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Update {
    pub table: String,
    pub assignments: Vec<Assignment>,
    pub filter: Option<Predicate>,
}

/// One `SET <column> = <value>` clause.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Assignment {
    pub column: String,
    pub value: Operand,
}

/// `DELETE FROM <table> [WHERE <predicate>]`. No WHERE deletes every row.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Delete {
    pub table: String,
    pub filter: Option<Predicate>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Select {
    /// Projected columns, or `*` for all (`SelectItems::All`).
    pub items: SelectItems,
    pub from: String,
    pub filter: Option<Predicate>,
    pub order_by: Option<OrderBy>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SelectItems {
    All,
    /// Named projections; each is a column name or a CAST of one.
    Items(Vec<SelectExpr>),
}

/// A projected expression: a column reference or a cast of an expression.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SelectExpr {
    Column(String),
    Cast {
        inner: Box<SelectExpr>,
        type_name: String,
    },
    Literal(Literal),
}

/// A WHERE predicate. Step-1 has single predicates only (no AND/OR — boolean type
/// deferred). Either a comparison, or a NULL test.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Predicate {
    Compare {
        column: String,
        op: CompareOp,
        /// The right-hand side: another column or a literal.
        rhs: Operand,
    },
    IsNull {
        column: String,
        negated: bool,
    },
}

/// A comparison operand: a column reference or a literal value.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Operand {
    Column(String),
    Literal(Literal),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CompareOp {
    Eq,
    Lt,
    Gt,
    Le,
    Ge,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OrderBy {
    pub column: String,
    // Step-1 corpus uses ascending only; direction reserved for later.
    pub descending: bool,
}

/// A literal value as written in SQL.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Literal {
    Null,
    /// An integer literal. Stored as i64. A bare integer literal is an *untyped
    /// constant* that adapts to its context — the target column on INSERT/UPDATE,
    /// the CAST target, the compared column in a WHERE predicate — and traps 22003
    /// if it does not fit; with no context it defaults to int64. See
    /// spec/design/types.md (Integer-literal typing).
    Int(i64),
}
