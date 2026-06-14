package jed

// Abstract syntax for the step-1 SQL surface. Boring, explicit shapes (CLAUDE.md
// §10); the hand-written parser produces these.

// Statement is a parsed top-level statement (exactly one of the fields is set).
type Statement struct {
	CreateTable *CreateTable
	DropTable   *DropTable
	CreateIndex *CreateIndex
	DropIndex   *DropIndex
	Insert      *Insert
	Select      *Select
	// SetOp is a set operation (UNION/INTERSECT/EXCEPT) combining two query expressions
	// (spec/design/grammar.md §25). Non-nil only when at least one set operator is present; a
	// lone SELECT stays in Select, so the plain-query path and host API are untouched.
	SetOp  *SetOp
	Update *Update
	Delete *Delete
	// Begin/Commit/Rollback are the explicit transaction-control statements (grammar.md §27,
	// transactions.md §4.2). Non-nil only for that statement.
	Begin    *Begin
	Commit   *Commit
	Rollback *Rollback
}

// Begin is a BEGIN [TRANSACTION|WORK] [READ ONLY|READ WRITE] / START TRANSACTION [...] statement
// — open an explicit transaction block (spec/design/grammar.md §27). Writable is the *requested*
// access mode when ModeSet (true READ WRITE, false READ ONLY — a write inside → 25006). With
// ModeSet false the mode was unspecified and defaults to the handle's — READ WRITE normally,
// READ ONLY on a read-only handle (api.md §2.1). A nested BEGIN is 25001 (transactions.md §4.2).
type Begin struct {
	Writable bool
	ModeSet  bool
}

// Commit is a COMMIT [TRANSACTION|WORK] / END [...] statement — publish the open block durably and
// return to autocommit; a COMMIT with no open block is a no-op success (transactions.md §4.2).
type Commit struct{}

// Rollback is a ROLLBACK [TRANSACTION|WORK] statement — discard the open block's working set and
// return to autocommit; a ROLLBACK with no open block is a no-op success (transactions.md §4.2).
type Rollback struct{}

// CreateTable is a CREATE TABLE statement.
type CreateTable struct {
	Name    string
	Columns []ColumnDef
	// TablePKs is the table-level `PRIMARY KEY (a, b, …)` constraints, each a list of
	// member column names in key order (spec/design/grammar.md §28). The parser collects
	// every one it sees; CREATE TABLE's execution resolves them (42703/42701) and rejects
	// more than one primary key across both forms (42P16) — spec/design/constraints.md §3.
	TablePKs [][]string
	// Checks is every `[CONSTRAINT name] CHECK ( expr )` of the statement — column-level
	// and table-level forms are semantically identical, so both collect here, in TEXTUAL
	// DEFINITION ORDER (it drives validation and naming — spec/design/constraints.md §4).
	// CREATE TABLE's execution validates each (0A000/42803/42P02/42703/42804) and names the
	// unnamed ones (42710 on a collision).
	Checks []CheckDef
	// Uniques is every `[CONSTRAINT name] UNIQUE [(cols)]` of the statement — the
	// column-level form collects as a one-member list — in TEXTUAL DEFINITION ORDER (it
	// drives member resolution, the dedup/PK fold, and naming —
	// spec/design/constraints.md §5). Each survivor becomes a unique secondary index
	// (spec/design/indexes.md §8).
	Uniques []UniqueDef
}

// UniqueDef is one parsed UNIQUE constraint (spec/design/grammar.md §31): the optional
// explicit CONSTRAINT name (empty = unnamed; it names the backing index) and the member
// column names in list order. Execution resolves the members (42703/42701/0A000) and
// names the index (42P07/42710) — spec/design/constraints.md §5.
type UniqueDef struct {
	Name    string
	Columns []string
}

// CheckDef is one parsed CHECK constraint (spec/design/grammar.md §29): the optional
// explicit CONSTRAINT name (empty = unnamed), the expression, and the expression's
// persisted text — the source token sequence between the parentheses re-rendered per the
// closed table in spec/fileformat/format.md "Check-expression text".
type CheckDef struct {
	Name string
	Expr Expr
	Text string
}

// DropTable is a DROP TABLE statement. Removes a table — its definition and all its
// rows — from the catalog. Dropping a table that does not exist is an error (42P01);
// there is no IF EXISTS this slice. Single table only; no CASCADE/RESTRICT (no
// dependent objects exist yet). See spec/design/grammar.md §13.
type DropTable struct {
	Name string
}

// CreateIndex is a CREATE [UNIQUE] INDEX [name] ON <table> ( col [, col]* ) statement —
// a secondary index (spec/design/indexes.md, grammar.md §30). Name == "" is the unnamed
// form; the executor derives PostgreSQL's auto-name. Key columns are bare names (no
// expression/ordered/partial keys this slice); a column may repeat (PG allows it).
// Execution validates in PG's order: table 42P01, columns 42703/0A000, name collision
// 42P07. A Unique index additionally verifies the existing rows at build (23505) and
// enforces uniqueness thereafter (spec/design/indexes.md §8).
type CreateIndex struct {
	Name    string
	Table   string
	Columns []string
	Unique  bool
}

// DropIndex is a DROP INDEX <name> statement — remove one secondary index
// (spec/design/indexes.md §2). Missing → 42704; a table's name → 42809.
type DropIndex struct {
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
	// NotNull is an explicit NOT NULL column constraint. A PRIMARY KEY column is implicitly
	// NOT NULL regardless of this flag; the executor ORs the two (spec/design/constraints.md).
	NotNull bool
	// Default is an optional DEFAULT <literal> — the value for this column when a row omits it
	// (or uses the DEFAULT keyword). Literal-only this slice; evaluated + type-coerced once at
	// CREATE TABLE (spec/design/constraints.md §2). nil = no default.
	Default *Literal
}

// TypeMod is a parsed type modifier: a precision and an optional scale, as written
// (numeric(p) → Scale nil, numeric(p,s) → Scale set). The values are the raw lexed magnitudes;
// range validation (1..=1000, 0..=p; else 22023) is at resolve.
type TypeMod struct {
	Precision uint64
	Scale     *uint64
}

// Insert is an INSERT ... [(col, ..)] whose rows come from EITHER a VALUES list (each value a
// literal or the DEFAULT keyword) OR a SELECT (INSERT ... SELECT — spec/design/grammar.md §24).
// An INSERT is two-phase / all-or-nothing — every row is validated before any is stored
// (spec/design/grammar.md §12).
type Insert struct {
	Table string
	// Columns is the optional explicit column list (`INSERT INTO t (a, c) VALUES ...` /
	// `... SELECT ...`); nil is the positional form (every column, in declaration order). Names
	// resolve at execution time (unknown → 42703, duplicate → 42701); an unlisted column takes
	// its default else NULL.
	Columns []string
	// EXACTLY ONE of Rows / Select is set (the parser guarantees it). Rows is the VALUES source:
	// each inner slice is one row's values in the order of Columns (or column order when Columns
	// is nil); non-empty when set, nil when Select is set. Select is the SELECT source: nil when
	// Rows is set.
	Rows   [][]InsertValue
	Select *Select
	// Returning is the optional terminal RETURNING clause (spec/design/grammar.md §32):
	// project each stored row, turning the statement into a query result. Nil = no clause.
	Returning *SelectItems
}

// InsertValue is one value slot in an INSERT VALUES row: a literal, a bind parameter ($N,
// bound at execute — spec/design/api.md §5), or the DEFAULT keyword (IsDefault) — which
// substitutes the target column's declared default (or NULL if it has none). The DEFAULT
// keyword is not reserved (spec/design/grammar.md §3). constraints.md §2.
type InsertValue struct {
	IsDefault bool
	IsParam   bool    // a $N bind parameter; Param holds the 1-based index
	Param     uint64  // valid when IsParam
	Lit       Literal // valid when !IsDefault && !IsParam
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
	// Returning is the optional terminal RETURNING clause (spec/design/grammar.md §32):
	// project each matched row's NEW (post-assignment) values. Nil = no clause.
	Returning *SelectItems
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
	// Returning is the optional terminal RETURNING clause (spec/design/grammar.md §32):
	// project each deleted row's OLD values. Nil = no clause.
	Returning *SelectItems
}

// TableRef is a table reference in a FROM clause: a table name with an optional alias
// (`orders o` or `orders AS o`). The alias, or the table name when there is none, is the
// relation's LABEL — it qualifies columns (o.col) and must be distinct within one query (a
// self-join needs aliases; a duplicate label is 42712). See spec/design/grammar.md §15.
//
// When IsFunc is true the reference is instead a set-returning FUNCTION call used as a row
// source (generate_series(1, 5)): Name is the function name and Args its argument expressions
// (the label is then the alias, or the function name when there is none — grammar.md §35).
type TableRef struct {
	Name   string
	Alias  *string
	IsFunc bool
	Args   []*Expr
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
	// From is the first table reference of the FROM clause, or nil for a FROM-less SELECT —
	// the select list evaluates over one virtual zero-column row (spec/design/grammar.md §34).
	From *TableRef
	// Joins holds the left-deep JOINs after From (nil/empty = a single-table SELECT; always
	// empty when From is nil — joins exist only inside a FROM clause).
	Joins  []JoinClause
	Filter *Expr
	// GroupBy holds the GROUP BY keys — bare or qualified table columns (never expressions /
	// aliases / ordinals); nil/empty means no GROUP BY. Each is an ExprColumn or
	// ExprQualifiedColumn (the parser restricts it to column_ref). spec/design/grammar.md §18.
	GroupBy []Expr
	// Having is the HAVING predicate (a boolean filter over the grouped rows), if any. May
	// reference aggregates and grouping keys; evaluated after aggregation, before ORDER BY.
	// HAVING makes a query an aggregate query even with no GROUP BY (spec/design/grammar.md §19).
	Having *Expr
	// OrderBy holds the ORDER BY sort keys, applied left to right; nil/empty means no
	// ORDER BY (spec/design/grammar.md §10).
	OrderBy []OrderKey
	// Limit caps the result at Limit rows; Offset skips the first Offset rows. Both are
	// non-negative counts, applied after ORDER BY, before projection (grammar.md §9). A
	// nil pointer means the clause is absent.
	Limit  *int64
	Offset *int64
}

// QueryExpr is the operand of a set operation (spec/design/grammar.md §25): either a single
// SELECT core or a nested set operation, so a chain like `a UNION b INTERSECT c` forms a tree.
// Exactly one field is non-nil.
type QueryExpr struct {
	Select *Select
	SetOp  *SetOp
}

// SetOpKind is the set operator (spec/design/grammar.md §25).
type SetOpKind int

const (
	SetOpUnion SetOpKind = iota
	SetOpIntersect
	SetOpExcept
)

// SetOp combines two query expressions (spec/design/grammar.md §25). All is the ALL (multiset)
// flag — false is the deduplicating default. The optional trailing ORDER BY / LIMIT / OFFSET apply
// to the WHOLE combined result and live on the outermost node only (an operand carries none — a
// deferred narrowing); OrderBy keys resolve against the output column names (the left operand's).
// Precedence is handled by the parser: INTERSECT binds tighter than UNION/EXCEPT (left-associative).
type SetOp struct {
	Op      SetOpKind
	All     bool
	Lhs     QueryExpr
	Rhs     QueryExpr
	OrderBy []OrderKey
	Limit   *int64
	Offset  *int64
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
	// ExprTypedLiteral is a typed string literal `type '...'` — PostgreSQL's `type 'string'`,
	// equal to CAST('string' AS type) over a string-literal operand (spec/design/grammar.md §36).
	// TypeLitName names the target scalar (resolved by from_name; unknown → 42704), TypeLitText is
	// the literal's string; the string is coerced to the type at resolve. The keyword names the
	// type, so the literal carries it in any expression position (`INTERVAL '1 day'`, `INTEGER '42'`).
	ExprTypedLiteral
	// ExprParam is a bind parameter $N (Param holds the 1-based index). Like an adaptable
	// literal it takes its type from context at resolve; the host binds a value at execute
	// (spec/design/api.md §5).
	ExprParam
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
	// ExprFuncCall is a function call — the shared aggregate/scalar call syntax
	// (spec/design/grammar.md §17): an aggregate or a scalar function (abs/round) resolve.
	ExprFuncCall
	// ExprIn is `lhs IN (list)` / `lhs NOT IN (list)` — membership over a non-empty
	// value list, desugared at resolve to the OR-chain (spec/design/grammar.md §20).
	ExprIn
	// ExprBetween is `lhs BETWEEN lo AND hi` / `lhs NOT BETWEEN lo AND hi` — a range test
	// desugared at resolve to `lhs >= lo AND lhs <= hi` (spec/design/grammar.md §21).
	ExprBetween
	// ExprLike is `lhs LIKE rhs` / `lhs NOT LIKE rhs` — a text pattern match with a dedicated
	// matcher (spec/design/grammar.md §22).
	ExprLike
	// ExprCase is a CASE expression (searched or simple form), lazily evaluated
	// (spec/design/grammar.md §23).
	ExprCase
	// ExprScalarSubquery is a scalar subquery `( query_expr )` in expression position
	// (spec/design/grammar.md §26). resolve plans it once against the scope chain; an uncorrelated
	// one is then folded to a constant, a correlated one is re-executed per outer row.
	ExprScalarSubquery
	// ExprExists is `EXISTS ( query_expr )` (a leading NOT is the ordinary unary connective).
	ExprExists
	// ExprInSubquery is `lhs [NOT] IN ( query_expr )` (spec/design/grammar.md §26) — membership of
	// lhs in the subquery's single output column (three-valued, like a literal IN).
	ExprInSubquery
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
	Kind        ExprKind
	Column      string     // ExprColumn, ExprQualifiedColumn (the column name)
	Qualifier   string     // ExprQualifiedColumn (the relation label)
	Param       uint64     // ExprParam (the 1-based bind-parameter index)
	Literal     *Literal   // ExprLiteral
	TypeLitName string     // ExprTypedLiteral (the named type, e.g. "integer", "interval")
	TypeLitText string     // ExprTypedLiteral (the literal's string, coerced to the type at resolve)
	Cast        *CastExpr  // ExprCast
	Unary       *UnaryExpr // ExprUnary
	Binary      *BinaryExpr
	IsNullOf    *IsNullExpr     // ExprIsNull
	IsDistinct  *IsDistinctExpr // ExprIsDistinct
	FuncCall    *FuncCallExpr   // ExprFuncCall
	In          *InExpr         // ExprIn
	Between     *BetweenExpr    // ExprBetween
	Like        *LikeExpr       // ExprLike
	Case        *CaseExpr       // ExprCase
	Subquery    *QueryExpr      // ExprScalarSubquery, ExprExists (the inner query)
	InSubquery  *InSubqueryExpr // ExprInSubquery
}

// InSubqueryExpr is `Lhs [NOT] IN ( Query )` (spec/design/grammar.md §26) — membership of Lhs in
// Query's single output column (three-valued, like a literal IN).
type InSubqueryExpr struct {
	Lhs     Expr
	Query   QueryExpr
	Negated bool
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

// FuncCallExpr is a function call — the shared aggregate/scalar call syntax
// (spec/design/grammar.md §17). Name is the spelling as written, resolved case-insensitively:
// an aggregate (COUNT/SUM/MIN/MAX/AVG), a scalar function (abs/round, kind = "function",
// spec/design/functions.md §9), or 42883 (undefined_function). Star is the COUNT(*) row-count
// form (then Args is empty); otherwise Args is the comma-separated argument list — aggregates
// and abs take one, round one or two. DISTINCT inside the parens is rejected at parse (42601).
// An aggregate in WHERE/ON or nested in another aggregate is 42803 (spec/design/aggregates.md);
// a scalar function is legal anywhere an expression is.
//
// ArgNames carries PostgreSQL named notation (name => value, grammar.md §17): nil ⇒ every
// argument positional (the common case); otherwise it is parallel to Args, with a non-nil
// *string for a named slot and nil for a positional one. The parser rejects a positional arg
// after a named one.
type FuncCallExpr struct {
	Name     string
	Args     []*Expr
	ArgNames []*string
	Star     bool
}

// InExpr is `Lhs IN (List)` / `Lhs NOT IN (List)` — membership over a non-empty value list
// (spec/design/grammar.md §20). Desugared at resolve into the OR-chain PostgreSQL defines it
// as (`x IN (a,b)` is `x = a OR x = b`; NOT IN is its negation), inheriting the three-valued
// NULL semantics and per-element operand typing from `=`/OR/NOT. The parser guarantees List is
// non-empty (`IN ()` is 42601).
type InExpr struct {
	Lhs     Expr
	List    []Expr
	Negated bool
}

// BetweenExpr is `Lhs BETWEEN Lo AND Hi` / `Lhs NOT BETWEEN Lo AND Hi` — a range test
// (spec/design/grammar.md §21). Desugared at resolve into `Lhs >= Lo AND Lhs <= Hi` (NOT
// BETWEEN negates), inheriting the three-valued NULL semantics from the comparisons and the
// Kleene AND. The bounds parse at the additive level so the structural `AND` is not the
// logical connective.
type BetweenExpr struct {
	Lhs     Expr
	Lo      Expr
	Hi      Expr
	Negated bool
}

// LikeExpr is `Lhs LIKE Rhs` / `Lhs NOT LIKE Rhs` — a text pattern match (spec/design/grammar.md
// §22). `%` matches any run of characters, `_` one code point, with the default `\` escape. Both
// operands must be text; NULL propagates. A genuine operator (not desugared) with a hand-written
// matcher. Negated carries the NOT keyword.
type LikeExpr struct {
	Lhs     Expr
	Rhs     Expr
	Negated bool
}

// CaseExpr is a CASE expression (spec/design/grammar.md §23). Searched form: Operand is nil and
// each When.Cond must be boolean. Simple form: Operand is non-nil and each branch matches when
// `Operand = When.Cond`. Whens has ≥1 entry. Els is the ELSE result, or nil for an implicit
// `ELSE NULL`. Lazily evaluated: the first TRUE branch wins; result-arm types unify.
type CaseExpr struct {
	Operand *Expr
	Whens   []CaseWhen
	Els     *Expr
}

// CaseWhen is one `WHEN cond THEN result` branch of a CaseExpr (Cond is the searched predicate,
// or the simple form's value compared for equality to the operand).
type CaseWhen struct {
	Cond   Expr
	Result Expr
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
