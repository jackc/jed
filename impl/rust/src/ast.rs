//! Abstract syntax for the step-1 SQL surface. Boring, explicit shapes
//! (CLAUDE.md §10); the hand-written parser produces these.

/// A parsed top-level statement.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Statement {
    CreateTable(CreateTable),
    Insert(Insert),
    Select(Select),
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
        value: Literal,
    },
    IsNull {
        column: String,
        negated: bool,
    },
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
    /// An integer literal. Stored as i64; its type is resolved by context
    /// (the target column on INSERT, the other operand in a comparison). The
    /// type of a bare integer literal is intentionally not committed here — the
    /// open spec question recorded in spec/design/conformance.md §7.
    Int(i64),
}
