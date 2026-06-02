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

// Update is `UPDATE <table> SET <col> = <expr> [, ...] [WHERE <expr>]`. Each
// assignment's right-hand side is evaluated against the pre-update row (so
// `SET a = b, b = a` swaps). Assigning a PRIMARY KEY column is rejected this slice
// (the storage key must not change — see the executor). The WHERE expression must
// resolve to boolean.
type Update struct {
	Table       string
	Assignments []Assignment
	Filter      *Expr
}

// Assignment is one `SET <Column> = <Value>` clause; Value is a general expression.
type Assignment struct {
	Column string
	Value  Expr
}

// Delete is `DELETE FROM <table> [WHERE <expr>]`. No WHERE deletes every row; the
// WHERE expression must resolve to boolean.
type Delete struct {
	Table  string
	Filter *Expr
}

// Select is a single-table SELECT. Filter (the WHERE expression) must resolve to
// boolean.
type Select struct {
	Items   SelectItems
	From    string
	Filter  *Expr
	OrderBy *OrderBy
}

// SelectItems is either all columns (*) or a list of projected expressions.
type SelectItems struct {
	All   bool
	Items []Expr
}

// CompareOp is unused now that comparisons are BinaryOp; retained name removed.

// ExprKind tags an expression node (Go has no sum types; this is the discriminant —
// the one place this slice deviates from the "exactly one pointer set" idiom, because
// a Column is a bare string).
type ExprKind int

const (
	// ExprColumn is a column reference.
	ExprColumn ExprKind = iota
	// ExprLiteral is a literal value.
	ExprLiteral
	// ExprCast is CAST(inner AS type).
	ExprCast
	// ExprUnary is a unary operator applied to one operand.
	ExprUnary
	// ExprBinary is a binary operator over two operands.
	ExprBinary
	// ExprIsNull is a postfix IS [NOT] NULL test.
	ExprIsNull
	// ExprIsDistinct is `lhs IS [NOT] DISTINCT FROM rhs` (NULL-safe equality).
	ExprIsDistinct
)

// UnaryOp is a unary operator.
type UnaryOp int

const (
	// OpNeg is arithmetic negation `-x`.
	OpNeg UnaryOp = iota
	// OpNot is logical negation `NOT x`.
	OpNot
)

// BinaryOp is a binary operator (arithmetic, comparison, or logical).
type BinaryOp int

const (
	// OpAdd is +.
	OpAdd BinaryOp = iota
	// OpSub is -.
	OpSub
	// OpMul is *.
	OpMul
	// OpDiv is /.
	OpDiv
	// OpMod is %.
	OpMod
	// OpEq is =.
	OpEq
	// OpLt is <.
	OpLt
	// OpGt is >.
	OpGt
	// OpLe is <=.
	OpLe
	// OpGe is >=.
	OpGe
	// OpAnd is AND.
	OpAnd
	// OpOr is OR.
	OpOr
)

// Expr is a general expression, shared by the SELECT list, WHERE, and UPDATE ... SET.
// Kind selects which fields are meaningful. A comparison/logical/null-test node is
// boolean-valued; arithmetic and columns/integer-literals are integer-valued.
type Expr struct {
	Kind       ExprKind
	Column     string     // ExprColumn
	Literal    *Literal   // ExprLiteral
	Cast       *CastExpr  // ExprCast
	Unary      *UnaryExpr // ExprUnary
	Binary     *BinaryExpr
	IsNullOf   *IsNullExpr     // ExprIsNull
	IsDistinct *IsDistinctExpr // ExprIsDistinct
}

// CastExpr is CAST(Inner AS TypeName).
type CastExpr struct {
	Inner    Expr
	TypeName string
}

// UnaryExpr is Op applied to Operand.
type UnaryExpr struct {
	Op      UnaryOp
	Operand Expr
}

// BinaryExpr is Lhs Op Rhs.
type BinaryExpr struct {
	Op  BinaryOp
	Lhs Expr
	Rhs Expr
}

// IsNullExpr is `Operand IS [NOT] NULL`.
type IsNullExpr struct {
	Operand Expr
	Negated bool
}

// IsDistinctExpr is `Lhs IS [NOT] DISTINCT FROM Rhs` — NULL-safe equality. Negated
// carries the NOT keyword: Negated == true is `IS NOT DISTINCT FROM` (NULL-safe `=`),
// false is `IS DISTINCT FROM` (its negation). Always boolean-valued, never unknown
// (spec/design/functions.md §3).
type IsDistinctExpr struct {
	Lhs     Expr
	Rhs     Expr
	Negated bool
}

// OrderBy is an ORDER BY clause. Step-1 corpus uses ascending only; Descending is
// reserved for later.
type OrderBy struct {
	Column     string
	Descending bool
}

// LiteralKind distinguishes the literal forms.
type LiteralKind int

const (
	// LiteralNull is the NULL literal.
	LiteralNull LiteralKind = iota
	// LiteralInt is an integer literal.
	LiteralInt
	// LiteralBool is a boolean literal (TRUE / FALSE).
	LiteralBool
)

// Literal is a literal value as written in SQL. A bare integer literal is an *untyped
// constant* that adapts to its context — the target column on INSERT/UPDATE, a sibling
// operand, the compared column in a WHERE predicate — and traps 22003 if it does not
// fit; with no context it defaults to int64 (spec/design/types.md §6). A boolean
// literal is expression-only this slice (it cannot be stored).
type Literal struct {
	Kind LiteralKind
	Int  int64
	Bool bool
}
