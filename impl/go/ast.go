package abide

// Abstract syntax for the step-1 SQL surface. Boring, explicit shapes (CLAUDE.md
// §10); the hand-written parser produces these.

// Statement is a parsed top-level statement (exactly one of the fields is set).
type Statement struct {
	CreateTable *CreateTable
	Insert      *Insert
	Select      *Select
	Update      *Update
	Delete      *Delete
}

// CreateTable is a CREATE TABLE statement.
type CreateTable struct {
	Name    string
	Columns []ColumnDef
}

// ColumnDef is a column definition in a CREATE TABLE.
type ColumnDef struct {
	Name string
	// TypeName as written (canonical or alias); resolved during analysis.
	TypeName   string
	PrimaryKey bool
}

// Insert is an INSERT ... VALUES with one row of literals, in column order.
type Insert struct {
	Table  string
	Values []Literal
}

// Update is `UPDATE <table> SET <col> = <operand> [, ...] [WHERE <predicate>]`. Each
// assignment's right-hand side is evaluated against the pre-update row (so
// `SET a = b, b = a` swaps). Assigning a PRIMARY KEY column is rejected this slice
// (the storage key must not change — see the executor).
type Update struct {
	Table       string
	Assignments []Assignment
	Filter      *Predicate
}

// Assignment is one `SET <Column> = <Value>` clause.
type Assignment struct {
	Column string
	Value  Operand
}

// Delete is `DELETE FROM <table> [WHERE <predicate>]`. No WHERE deletes every row.
type Delete struct {
	Table  string
	Filter *Predicate
}

// Select is a single-table SELECT.
type Select struct {
	Items   SelectItems
	From    string
	Filter  *Predicate
	OrderBy *OrderBy
}

// SelectItems is either all columns (*) or a list of projected expressions.
type SelectItems struct {
	All   bool
	Items []SelectExpr
}

// SelectExpr is a projected expression: a column reference, a cast, or a literal.
// Exactly one field is set (Cast nests via Inner).
type SelectExpr struct {
	Column  string
	Literal *Literal
	Cast    *CastExpr
}

// CastExpr is CAST(Inner AS TypeName).
type CastExpr struct {
	Inner    SelectExpr
	TypeName string
}

// CompareOp is a comparison operator.
type CompareOp int

const (
	// OpEq is =.
	OpEq CompareOp = iota
	// OpLt is <.
	OpLt
	// OpGt is >.
	OpGt
	// OpLe is <=.
	OpLe
	// OpGe is >=.
	OpGe
)

// Predicate is a WHERE predicate. Step-1 has single predicates only (no AND/OR —
// boolean type deferred): either a comparison or a NULL test. Exactly one is set.
type Predicate struct {
	Compare *ComparePredicate
	IsNull  *IsNullPredicate
}

// ComparePredicate is `column <op> rhs`, where rhs is another column or a literal.
type ComparePredicate struct {
	Column string
	Op     CompareOp
	RHS    Operand
}

// Operand is a comparison's right-hand side: a column reference or a literal.
// Exactly one of Column / Literal is set.
type Operand struct {
	Column  string
	Literal *Literal
}

// IsNullPredicate is `column IS [NOT] NULL`.
type IsNullPredicate struct {
	Column  string
	Negated bool
}

// OrderBy is an ORDER BY clause. Step-1 corpus uses ascending only; Descending is
// reserved for later.
type OrderBy struct {
	Column     string
	Descending bool
}

// LiteralKind distinguishes a NULL literal from an integer literal.
type LiteralKind int

const (
	// LiteralNull is the NULL literal.
	LiteralNull LiteralKind = iota
	// LiteralInt is an integer literal.
	LiteralInt
)

// Literal is a literal value as written in SQL. A bare integer literal is an *untyped
// constant* that adapts to its context — the target column on INSERT/UPDATE, the CAST
// target, the compared column in a WHERE predicate — and traps 22003 if it does not fit;
// with no context it defaults to int64. See spec/design/types.md (Integer-literal typing).
type Literal struct {
	Kind LiteralKind
	Int  int64
}
