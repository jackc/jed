package jed

// Abstract syntax for the step-1 SQL surface. Boring, explicit shapes (CLAUDE.md
// §10); the hand-written parser produces these.

// Statement is a parsed top-level statement (exactly one of the fields is set).
type Statement struct {
	CreateTable *CreateTable
	DropTable   *DropTable
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

// DropTable is a DROP TABLE statement. Removes a table — its definition and all its
// rows — from the catalog. Dropping a table that does not exist is an error (42P01);
// there is no IF EXISTS this slice. Single table only; no CASCADE/RESTRICT (no
// dependent objects exist yet). See spec/design/grammar.md §13.
type DropTable struct {
	Name string
}

// ColumnDef is a column definition in a CREATE TABLE.
type ColumnDef struct {
	Name string
	// TypeName as written (canonical or alias); resolved during analysis.
	TypeName string
	// TypeMod is an optional parenthesized type modifier, numeric(p[,s]) — the first
	// parameterized type. Meaningful only for decimal; validated at resolve (grammar.md §14).
	TypeMod    *TypeMod
	PrimaryKey bool
}

// TypeMod is a parsed type modifier: a precision and an optional scale, as written
// (numeric(p) → Scale nil, numeric(p,s) → Scale set). The values are the raw lexed magnitudes;
// range validation (1..=1000, 0..=p; else 22023) is at resolve.
type TypeMod struct {
	Precision uint64
	Scale     *uint64
}

// Insert is an INSERT ... VALUES with one or more rows of literals, each in column
// order. A multi-row INSERT is two-phase / all-or-nothing — every row is validated
// before any is stored (spec/design/grammar.md §12). Rows is always non-empty (the
// parser requires ≥1 row).
type Insert struct {
	Table string
	Rows  [][]Literal
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

// TableRef is a table reference in a FROM clause: a table name with an optional alias
// (`orders o` or `orders AS o`). The alias, or the table name when there is none, is the
// relation's LABEL — it qualifies columns (o.col) and must be distinct within one query (a
// self-join needs aliases; a duplicate label is 42712). See spec/design/grammar.md §15.
type TableRef struct {
	Name  string
	Alias *string
}

// JoinKind is the kind of a join. Inner and Cross execute this slice; the Left/Right/Full
// outer kinds parse and are carried in the AST but executing one is a documented 0A000
// narrowing (the OUTER family is a fast-follow — spec/design/grammar.md §15).
type JoinKind int

const (
	// JoinInner is INNER JOIN (a bare JOIN is INNER).
	JoinInner JoinKind = iota
	// JoinCross is CROSS JOIN (a Cartesian product, no ON).
	JoinCross
	// JoinLeft is LEFT [OUTER] JOIN (deferred — 0A000 at execution).
	JoinLeft
	// JoinRight is RIGHT [OUTER] JOIN (deferred — 0A000).
	JoinRight
	// JoinFull is FULL [OUTER] JOIN (deferred — 0A000).
	JoinFull
)

// JoinClause is one JOIN step in the left-deep FROM chain: the join kind, the right-hand
// table reference, and the optional ON predicate (nil for CROSS JOIN; set for INNER/outer,
// which require an ON). See spec/design/grammar.md §15.
type JoinClause struct {
	Kind  JoinKind
	Table TableRef
	On    *Expr
}

// Select is a SELECT. The FROM clause is a left-deep chain: From followed by zero or more
// Joins (empty = single-table). Filter (the WHERE expression) must resolve to boolean.
type Select struct {
	// Distinct is SELECT DISTINCT — deduplicate the projected output rows (NULL-safe),
	// applied after ORDER BY and before LIMIT/OFFSET (spec/design/grammar.md §11).
	Distinct bool
	Items    SelectItems
	From     TableRef
	// Joins holds the left-deep JOINs after From (nil/empty = a single-table SELECT).
	Joins  []JoinClause
	Filter *Expr
	// OrderBy holds the ORDER BY sort keys, applied left to right; nil/empty means no
	// ORDER BY (spec/design/grammar.md §10).
	OrderBy []OrderKey
	// Limit caps the result at Limit rows; Offset skips the first Offset rows. Both are
	// non-negative counts, applied after ORDER BY, before projection (grammar.md §9). A
	// nil pointer means the clause is absent.
	Limit  *int64
	Offset *int64
}

// SelectItems is either all columns (*) or a list of projected expressions.
type SelectItems struct {
	All   bool
	Items []SelectItem
}

// SelectItem is one select-list expression with its optional output-name alias
// (expr AS name). The alias is an output label only — it never enters resolution
// (spec/design/grammar.md §8). When Alias is nil the output name is derived by the
// resolver: a bare column's canonical name, or the fixed "?column?" otherwise.
type SelectItem struct {
	Expr  Expr
	Alias *string
}

// CompareOp is unused now that comparisons are BinaryOp; retained name removed.

// ExprKind tags an expression node (Go has no sum types; this is the discriminant —
// the one place this slice deviates from the "exactly one pointer set" idiom, because
// a Column is a bare string).
type ExprKind int

const (
	// ExprColumn is a bare (unqualified) column reference (Column holds the name).
	ExprColumn ExprKind = iota
	// ExprQualifiedColumn is a qualified reference `rel.col` (Qualifier holds the relation
	// label, Column the column name); resolved against exactly that one relation, never
	// ambiguous (spec/design/grammar.md §15).
	ExprQualifiedColumn
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
	Column     string     // ExprColumn, ExprQualifiedColumn (the column name)
	Qualifier  string     // ExprQualifiedColumn (the relation label)
	Literal    *Literal   // ExprLiteral
	Cast       *CastExpr  // ExprCast
	Unary      *UnaryExpr // ExprUnary
	Binary     *BinaryExpr
	IsNullOf   *IsNullExpr     // ExprIsNull
	IsDistinct *IsDistinctExpr // ExprIsDistinct
}

// CastExpr is CAST(Inner AS TypeName). TypeMod is the optional numeric(p[,s]) modifier.
type CastExpr struct {
	Inner    Expr
	TypeName string
	TypeMod  *TypeMod
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

// OrderKey is one ORDER BY sort key: a bare table column, a sort direction, and a resolved
// NULL placement. NullsFirst is resolved at parse time — an explicit NULLS FIRST|LAST, else
// the direction default (Descending: ASC -> last, DESC -> first, the PostgreSQL model where
// NULL is the largest value) — and is applied independently of the Descending value flip
// (spec/design/grammar.md §10).
type OrderKey struct {
	// Qualifier is an optional relation qualifier (`ORDER BY t.a`); "" is a bare column.
	Qualifier  string
	Column     string
	Descending bool
	NullsFirst bool
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
	// LiteralText is a single-quoted text literal (Str holds the decoded content). Its
	// type is always text (collation C); it does not adapt to context like an integer
	// literal does (spec/design/types.md §11).
	LiteralText
	// LiteralDecimal is a decimal literal (Dec holds the constructed value, sign folded). An
	// untyped decimal constant that adapts to context; caps are checked at resolve
	// (spec/design/grammar.md §14, decimal.md §6).
	LiteralDecimal
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
	Str  string  // LiteralText
	Dec  Decimal // LiteralDecimal
}
