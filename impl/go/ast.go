package abide

// Abstract syntax for the step-1 SQL surface. Boring, explicit shapes (CLAUDE.md
// §10); the hand-written parser produces these.

// Statement is a parsed top-level statement (exactly one of the fields is set).
type Statement struct {
	CreateTable *CreateTable
	Insert      *Insert
	Select      *Select
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

// Literal is a literal value as written in SQL. The type of a bare integer literal
// is intentionally not committed here — the open spec question recorded in
// spec/design/conformance.md §7; it is resolved by context.
type Literal struct {
	Kind LiteralKind
	Int  int64
}
