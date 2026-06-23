package jed

// Abstract syntax for the step-1 SQL surface. Boring, explicit shapes (CLAUDE.md
// §10); the hand-written parser produces these.

// Statement is a parsed top-level statement (exactly one of the fields is set).
type Statement struct {
	CreateTable *CreateTable
	DropTable   *DropTable
	CreateIndex *CreateIndex
	DropIndex   *DropIndex
	CreateType  *CreateType
	DropType    *DropType
	// CreateSequence/AlterSequence/DropSequence are the sequence DDL statements
	// (spec/design/sequences.md): a named, persisted i64 generator. Non-nil only for that statement.
	CreateSequence *CreateSequence
	AlterSequence  *AlterSequence
	DropSequence   *DropSequence
	Insert         *Insert
	Select         *Select
	// SetOp is a set operation (UNION/INTERSECT/EXCEPT) combining two query expressions
	// (spec/design/grammar.md §25). Non-nil only when at least one set operator is present; a
	// lone SELECT stays in Select, so the plain-query path and host API are untouched.
	SetOp *SetOp
	// With is a query prefixed by a WITH clause defining one or more common table expressions
	// (spec/design/cte.md). Non-nil only when a WITH is present; a plain query stays Select/SetOp,
	// so the host API and the no-CTE paths are untouched (the SetOp precedent).
	With   *WithQuery
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
	Name string
	// Temp is whether `TEMP` / `TEMPORARY` preceded `TABLE` — a temporary table
	// (spec/design/temp-tables.md). A temp table makes ZERO writes to the database file (it lives
	// outside the serialized snapshot) and is dropped at session / database close. Its DDL is gated by
	// allowTempDDL (session-local) or allowSharedTempDDL (shared) rather than allowDDL (temp-tables.md
	// §5). Shared implies Temp (a SHARED table is always temporary).
	Temp bool
	// Shared is whether `SHARED` preceded `TEMP`/`TEMPORARY` — a DATABASE-WIDE shared temporary table
	// (temp-tables.md §4): one set of rows visible to and writable by every session of the open
	// Database, still never written to the file. Shared==true always has Temp==true (the parser
	// rejects SHARED not followed by TEMP/TEMPORARY as 42601); when false (and Temp), the table is
	// session-local.
	Shared  bool
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
	// ForeignKeys is every `FOREIGN KEY (cols) REFERENCES …` of the statement — the
	// column-level `REFERENCES` form collects as a one-member list — in TEXTUAL DEFINITION
	// ORDER (it drives resolution and naming — spec/design/constraints.md §6). CREATE TABLE's
	// execution resolves each (42703/42701/42P01/42830/42804), rejects unsupported actions
	// (0A000), and names the unnamed ones (42710).
	ForeignKeys []ForeignKeyDef
}

// RefAction is a referential action for `ON DELETE` / `ON UPDATE`
// (spec/design/constraints.md §6.6). Only NoAction (the default) and Restrict are
// supported — identical in jed (no deferrable constraints); the write-actions parse but
// are rejected 0A000 at CREATE TABLE.
type RefAction int

const (
	// RefNoAction is the default NO ACTION.
	RefNoAction RefAction = iota
	// RefRestrict is RESTRICT.
	RefRestrict
	// RefCascade is CASCADE (parses; rejected 0A000).
	RefCascade
	// RefSetNull is SET NULL (parses; rejected 0A000).
	RefSetNull
	// RefSetDefault is SET DEFAULT (parses; rejected 0A000).
	RefSetDefault
)

// ForeignKeyDef is one parsed `FOREIGN KEY` / `REFERENCES` constraint (spec/design/grammar.md
// §43): the optional explicit CONSTRAINT name (empty = unnamed), the local (referencing)
// column names in list order, the referenced (parent) table name, the optional referenced
// column names (RefColumns nil = the parent's primary key), and the ON DELETE / ON UPDATE
// actions. Execution resolves it (42703/42701/42P01/42830/42804) and names the unnamed ones
// (42710) — spec/design/constraints.md §6.
type ForeignKeyDef struct {
	Name       string
	Columns    []string
	RefTable   string
	RefColumns []string // nil = default to the parent's primary key
	OnDelete   RefAction
	OnUpdate   RefAction
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

// DefaultDef is a parsed DEFAULT <expr> column constraint (spec/design/constraints.md §2):
// the default expression and its persisted text (the source token sequence re-rendered per
// the closed table in spec/fileformat/format.md "Check-expression text", as a CHECK is).
// Execution classifies it: a bare *Literal is a constant (pre-evaluated at CREATE TABLE), any
// other expression is stored as text and evaluated per row at INSERT.
type DefaultDef struct {
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
	// Using is the `USING <method>` access method as written, or "" for the default ordered
	// B-tree. Resolved at execution: ""/"btree" → B-tree, "gin" → GIN, else 42704 (gin.md §3).
	Using string
}

// DropIndex is a DROP INDEX <name> statement — remove one secondary index
// (spec/design/indexes.md §2). Missing → 42704; a table's name → 42809.
type DropIndex struct {
	Name string
}

// CreateType is a CREATE TYPE <name> AS ( field type [NOT NULL] [, …] ) statement — a
// user-defined composite (row) type (spec/design/composite.md, grammar.md). Execution resolves
// each field's type (a built-in scalar or a previously-defined composite — 42704 if unknown),
// rejects a duplicate type name (42710), and registers it in the catalog. Named composites only
// this slice; anonymous record is not supported.
type CreateType struct {
	Name   string
	Fields []TypeFieldDef
}

// TypeFieldDef is one field of a CREATE TYPE definition: its name, its type as written (a built-in
// scalar alias or a composite type name), an optional numeric(p,s) modifier, and an explicit
// NOT NULL. Resolved at execution (mirrors ColumnDef).
type TypeFieldDef struct {
	Name     string
	TypeName string
	TypeMod  *TypeMod
	NotNull  bool
}

// DropType is a DROP TYPE [IF EXISTS] <name> [RESTRICT] statement — remove a composite type
// (spec/design/composite.md §7). RESTRICT (the default and only behavior this slice) fails with
// 2BP01 if a table column or another composite type still references it; CASCADE is 0A000. A
// missing type without IF EXISTS is 42704.
type DropType struct {
	Name     string
	IfExists bool
}

// SeqOptions is the parsed, order-free sequence-option set shared by CREATE SEQUENCE and an
// IDENTITY column's optional `( seq_options )` (spec/design/sequences.md §13). Each is captured as
// a parsed override, with a nil pointer meaning "use the default" (resolved at execution against
// the INCREMENT sign); execution validates the set (22023).
type SeqOptions struct {
	// DataType is the `AS <type>` value type as written (the raw type name, e.g. "smallint" /
	// "int4"), resolved to a SeqDataType at execution (spec/design/sequences.md §14); "" = the
	// bigint default. A non-integer type is 22023. Inside an IDENTITY column's options a set
	// DataType is 42601 (the column type fixes it).
	DataType  string
	Increment *int64
	// MinValue is the MINVALUE override: a SeqBound whose NoValue distinguishes
	// MINVALUE v (Value=v) from NO MINVALUE (NoValue) from unset (nil) — the
	// Rust Option<Option<i64>>. nil = unset (use the default).
	MinValue *SeqBound
	MaxValue *SeqBound
	Start    *int64
	Cache    *int64
	Cycle    *bool
}

// CreateSequence is a CREATE SEQUENCE [IF NOT EXISTS] <name> [options] statement — a named,
// persisted i64 generator (spec/design/sequences.md). Execution validates the option set (22023),
// rejects a relation-namespace collision (42P07 unless IfNotExists), and registers the sequence.
type CreateSequence struct {
	Name        string
	IfNotExists bool
	Options     SeqOptions
}

// IdentitySpec is a column's `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [( seq_options )]`
// constraint (spec/design/sequences.md §13). Always distinguishes ALWAYS (true) from BY DEFAULT
// (false); Options tunes the auto-created owned sequence (defaults to the standard ascending i64).
type IdentitySpec struct {
	Always  bool
	Options SeqOptions
}

// SeqBound is a MINVALUE/MAXVALUE override (spec/design/sequences.md): NoValue true selects
// the type default (NO MINVALUE / NO MAXVALUE); otherwise Value is the explicit bound. A nil
// *SeqBound on CreateSequence means the option was unset (also the default).
type SeqBound struct {
	NoValue bool
	Value   int64
}

// DropSequence is a DROP SEQUENCE [IF EXISTS] <name> [, …] [RESTRICT] statement — remove one or
// more sequences (spec/design/sequences.md §1). A missing sequence without IF EXISTS is 42P01;
// CASCADE is 0A000 (RESTRICT is the default and only mode this slice).
type DropSequence struct {
	Names    []string
	IfExists bool
}

// AlterSequence is an ALTER SEQUENCE [IF EXISTS] <name> <action> statement (spec/design/sequences.md
// §4/§15). A missing sequence without IfExists is 42P01. RenameTo != "" selects the RENAME TO form;
// otherwise the option form (Options + Restart) applies. The two are mutually exclusive (the parser
// requires ≥ 1 option/RESTART for the option form — a bare ALTER SEQUENCE s is 42601).
type AlterSequence struct {
	Name     string
	IfExists bool
	// RenameTo is the new name for the RENAME TO form, or "" for the option form.
	RenameTo string
	// Options + Restart describe the option form. Restart mirrors Rust's Option<Option<i64>>:
	// nil = no RESTART; non-nil with ToStart = bare RESTART (reset to the stored START); non-nil
	// with a Value = RESTART WITH Value.
	Options SeqOptions
	Restart *SeqRestart
}

// SeqRestart is a parsed RESTART pseudo-option on ALTER SEQUENCE (spec/design/sequences.md §15):
// ToStart true is a bare RESTART (reset to the stored START); otherwise Value is the RESTART WITH n.
type SeqRestart struct {
	ToStart bool
	Value   int64
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
	// Default is an optional DEFAULT <expr> — the value for this column when a row omits it (or
	// uses the DEFAULT keyword). A constant literal is pre-evaluated at CREATE TABLE; any other
	// expression is evaluated per row at INSERT (spec/design/constraints.md §2). nil = no default.
	Default *DefaultDef
	// Identity is an optional `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [( opts )]` constraint
	// (spec/design/sequences.md §13). Desugars like serial (an owned sequence + a nextval default +
	// NOT NULL) plus the persisted ALWAYS/BY DEFAULT distinction. nil = a non-identity column.
	Identity *IdentitySpec
	// Collation is an optional `COLLATE "name"` column modifier (spec/design/collation.md §1) — a
	// quoted, case-sensitive collation name. Text-only (else 42804); the name must be loaded or "C"
	// (else 42704). "" = no clause ⇒ inherit the per-database default. Frozen into the column at
	// CREATE TABLE.
	Collation string
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
	// Overriding is the optional `OVERRIDING { SYSTEM | USER } VALUE` clause
	// (spec/design/sequences.md §13), governing IDENTITY columns. nil is the default (no override).
	Overriding *Overriding
	// EXACTLY ONE of Rows / Select is set (the parser guarantees it). Rows is the VALUES source:
	// each inner slice is one row's values in the order of Columns (or column order when Columns
	// is nil); non-empty when set, nil when Select is set. Select is the SELECT source: nil when
	// Rows is set.
	Rows   [][]InsertValue
	Select *Select
	// OnConflict is the optional ON CONFLICT clause (UPSERT — spec/design/upsert.md), between the
	// source and RETURNING. Nil = no clause (a conflict traps 23505 as usual).
	OnConflict *OnConflict
	// Returning is the optional terminal RETURNING clause (spec/design/grammar.md §32):
	// project each stored row, turning the statement into a query result. Nil = no clause.
	Returning *SelectItems
}

// OnConflict is the `ON CONFLICT [target] action` clause (spec/design/upsert.md §1).
type OnConflict struct {
	// Target is the optional conflict target (the arbiter). Nil is legal only with DoNothing
	// (any uniqueness conflict is then skipped); DoUpdate with a nil target is 42601.
	Target *ConflictTarget
	// DoUpdate true = DO UPDATE (Assignments + Filter apply); false = DO NOTHING.
	DoUpdate    bool
	Assignments []Assignment // DO UPDATE SET … (empty for DO NOTHING)
	Filter      *Expr        // optional DO UPDATE WHERE … (nil otherwise)
}

// ConflictTarget is the arbiter constraint named by an ON CONFLICT target (spec/design/upsert.md
// §2). EXACTLY ONE of Columns / Constraint is meaningful (IsConstraint selects which).
type ConflictTarget struct {
	// Columns is the `( col [, ...] )` inference list — matched as a SET against a unique index /
	// the primary key (order-independent; no match → 42P10). Valid when !IsConstraint.
	Columns []string
	// IsConstraint marks `ON CONSTRAINT name`; Constraint is a unique-index name or the
	// synthesized <table>_pkey (miss → 42704).
	IsConstraint bool
	Constraint   string
}

// Overriding is the INSERT `OVERRIDING { SYSTEM | USER } VALUE` clause (spec/design/sequences.md
// §13): OverridingSystem lets an explicit value land in a GENERATED ALWAYS identity column;
// OverridingUser discards a supplied value for any identity column and uses its sequence instead.
type Overriding int

const (
	OverridingSystem Overriding = iota
	OverridingUser
)

// InsertValue is one value slot in an INSERT VALUES row: a literal, a bind parameter ($N,
// bound at execute — spec/design/api.md §5), or the DEFAULT keyword (IsDefault) — which
// substitutes the target column's declared default (or NULL if it has none). The DEFAULT
// keyword is not reserved (spec/design/grammar.md §3). constraints.md §2.
type InsertValue struct {
	IsDefault bool
	IsParam   bool    // a $N bind parameter; Param holds the 1-based index
	Param     uint64  // valid when IsParam
	Lit       Literal // valid when !IsDefault && !IsParam && !IsRow
	// IsRow marks a ROW(...) composite constructor (spec/design/composite.md §1); Row holds the
	// field slots, in order (each itself a literal, a $N, or a nested ROW). A composite-typed column
	// takes a ROW(...) value in INSERT VALUES.
	IsRow bool
	Row   []InsertValue // valid when IsRow
	// IsArray marks an ARRAY[...] constructor (spec/design/array.md §1); Array holds the element
	// slots, in order (each a literal or a $N). An array-typed column takes an ARRAY[...] value.
	IsArray bool
	Array   []InsertValue // valid when IsArray
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
// TableRef is one FROM relation: a base table NAME, a set-returning function CALL (IsFunc, Args —
// generate_series, grammar.md §35), or a DERIVED TABLE (Subquery non-nil — a parenthesized subquery
// `FROM (SELECT …) [AS] t`, grammar.md §42). A derived table is mechanically an anonymous,
// always-inlined single-reference CTE: the planner reuses the CTE synthetic-relation seam. Its alias
// is OPTIONAL (PG 18); when present it is the label and ColumnAliases is the optional column-rename
// list, when absent Name/Alias are empty and the relation has no qualifier.
// Values carries a VALUES-body derived table — FROM (VALUES (e11,…),(e21,…)) AS v(c1,…)
// (spec/design/grammar.md §42): a parenthesized VALUES list used as a relation, a computed
// relation of literal rows. It is the FROM-position alternative body to Subquery (the two are
// mutually exclusive — at most one is non-nil on a derived table). Each value is a general
// constant expression (resolved parent=nil, non-LATERAL unless this TableRef is marked Lateral);
// the rows share arity and the columns' types unify across rows like a set operation. The outer
// slice is the rows, each inner slice one row's values, left to right.
// Lateral is set when the FROM item is preceded by the LATERAL keyword (spec/design/grammar.md §44):
// the derived-table body / SRF arguments may then reference columns of the FROM relations that appear
// BEFORE this one (a dependent / correlated join). It is meaningful only for a derived table or table
// function; a table function is implicitly lateral, so the planner correlates an SRF's args to the
// earlier siblings whether or not this flag is set.
type TableRef struct {
	Name          string
	Alias         *string
	IsFunc        bool
	Args          []*Expr
	Subquery      *QueryExpr
	Values        [][]*Expr
	ColumnAliases []string
	Lateral       bool
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
	// Windows holds the named windows from a `WINDOW name AS (definition)` clause
	// (spec/design/window.md §5, grammar.ebnf window_clause), referenced by `OVER name`. Empty when
	// absent. Resolved by a desugaring pass that rewrites each `OVER name` to its definition (into
	// the call's Over) before resolution.
	Windows []NamedWindow
}

// NamedWindow is one `name AS (definition)` entry of a WINDOW clause (spec/design/window.md §5).
type NamedWindow struct {
	Name string
	Def  WindowDef
}

// QueryExpr is the operand of a set operation (spec/design/grammar.md §25): a single SELECT core, a
// nested set operation (so a chain like `a UNION b INTERSECT c` forms a tree), or a nested WITH
// clause (spec/design/cte.md §7). Exactly one field is non-nil.
type QueryExpr struct {
	Select *Select
	SetOp  *SetOp
	With   *WithExpr
}

// WithExpr is a nested `WITH … query_expr` (spec/design/cte.md §7): the CTE list Ctes (forward-only
// visibility; self-referencing when Recursive) prefixing the inner query expression Body, in a
// subquery / derived-table / CTE-body position — as opposed to the top-level WithQuery (which may
// prefix a data-modifying primary). The CTEs are visible only within Body (and to each other); the
// enclosing statement's CTE bindings are NOT inherited — a documented narrowing (cte.md §7). A
// data-modifying CTE here is rejected at planning (0A000 — PostgreSQL restricts a DML-WITH to the
// statement top level).
type WithExpr struct {
	Ctes      []Cte
	Recursive bool
	Body      *QueryExpr
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

// CteBody is the body of a CTE, or the WITH-prefixed primary statement
// (spec/design/writable-cte.md): an ordinary query expression, or a data-modifying statement (a
// writable CTE). Exactly one field is non-nil (the QueryExpr/InsertSource sum-type precedent); the
// data-modifying variants carry the parsed INSERT/UPDATE/DELETE.
type CteBody struct {
	Query  *QueryExpr
	Insert *Insert
	Update *Update
	Delete *Delete
}

// AsQuery returns the query expression when this body is a plain query, else nil — used by the
// recursive-CTE analysis (only a query body can be a recursive UNION) and the pure-query WITH path.
func (b *CteBody) AsQuery() *QueryExpr { return b.Query }

// IsDataModifying reports whether this body is a data-modifying statement (INSERT/UPDATE/DELETE).
func (b *CteBody) IsDataModifying() bool { return b.Query == nil }

// Cte is one common table expression in a WITH list (spec/design/cte.md). A named, statement-local
// relation backed by a query or (spec/design/writable-cte.md) a data-modifying statement. Columns is
// the optional column-rename list (renames the body's output columns left to right; a count mismatch
// is 42P10). Materialized is the explicit evaluation hint: a non-nil pointer to true is MATERIALIZED,
// to false is NOT MATERIALIZED, nil is PostgreSQL's default (inline a single-reference CTE,
// materialize a multi-reference one — cost.md §3; a data-modifying CTE is always materialized, the
// hint inert). Body is a cte_body.
type Cte struct {
	Name         string
	Columns      []string
	Materialized *bool
	Body         CteBody
}

// WithQuery is a top-level statement prefixed by a WITH clause (spec/design/cte.md). Ctes is the
// non-empty list of common table expressions (each visible to later CTEs and to Body); Body is the
// main statement the CTEs prefix — a query, or (spec/design/writable-cte.md) a data-modifying
// INSERT/UPDATE/DELETE primary. Built only when a WITH is present — a plain query stays
// Statement{Select} / Statement{SetOp}, so those paths are untouched (the SetOp precedent).
// Recursive is the WITH RECURSIVE flag (spec/design/recursive-cte.md): a flag on the whole list that
// ENABLES a CTE to reference itself (lifting the forward-only 42P01); a CTE that does not reference
// itself is still an ordinary non-recursive CTE.
type WithQuery struct {
	Ctes      []Cte
	Body      CteBody
	Recursive bool
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
	// ExprExtract is EXTRACT(field FROM source) — the datetime field special form (timezones.md §9.2,
	// grammar.md §50). The field is syntactic; resolves to a numeric.
	ExprExtract
	// ExprCollate is `expr COLLATE "name"` — the postfix collation operator
	// (spec/design/collation.md §1).
	ExprCollate
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
	// ExprRegex is `lhs ~ rhs` / `~*` / `!~` / `!~*` — a regular-expression match with a hand-written
	// Pike VM (spec/design/grammar.md §22b, regex.md).
	ExprRegex
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
	// ExprQuantified is `lhs op ANY/SOME/ALL ( array )` (spec/design/array-functions.md §11) — a
	// quantified array comparison, the array spelling of IN. The three-valued fold over the array's
	// flattened elements reuses the IN-list membership semantics, generalized to all five comparison
	// operators and both quantifiers.
	ExprQuantified
	// ExprQuantifiedSubquery is `lhs op ANY/SOME/ALL ( query_expr )` (array-functions.md §11.6) — the
	// SUBQUERY form of the quantified comparison, the subquery spelling of IN. Parallel to
	// ExprInSubquery: the body's single column (42601 if >1) folds through the SAME three-valued fold
	// as ExprQuantified. Uncorrelated folds to a constant-array Quantified; correlated re-executes
	// per outer row.
	ExprQuantifiedSubquery
	// ExprRow is a `ROW(e1, e2, …)` composite constructor (spec/design/composite.md §1): RowItems
	// holds the field expressions, in order. A one-field ROW(x) is a one-field row; ROW() is the
	// zero-field row. The bare `(a, b)` form is deferred (0A000); only the keyword form parses.
	ExprRow
	// ExprArray is an `ARRAY[e1, e2, …]` array constructor (spec/design/array.md §1): RowItems holds
	// the element expressions, in order (reusing the RowItems slot). `ARRAY[]` is the empty array.
	ExprArray
	// ExprFieldAccess is field selection `(expr).field` (spec/design/composite.md §S4) — the value
	// of one named field of a composite Base. The parser produces this for a `.name` postfix on a
	// parenthesized / `ROW(…)` / cast / qualified-column base; a bare `a.b` stays
	// ExprQualifiedColumn and only falls back to field access at resolve when `a` is no relation
	// but a composite column (the ambiguity rule — table.column first, then column.field). Field
	// lookup is case-insensitive; an unknown field is 42703, a non-composite base 42809.
	ExprFieldAccess
	// ExprFieldStar is whole-row expansion `(expr).*` (spec/design/composite.md §S4) — expands a
	// composite Base into one output column per field, in declaration order. Valid only in a
	// SELECT/RETURNING projection list (where `*` expands); in any scalar expression position it is
	// 0A000.
	ExprFieldStar
	// ExprSubscript is array subscript `Base[..][..]` (spec/design/array.md §6) — one or more
	// bracketed specs (Subscripts) applied to an array Base. Each spec is an index `[i]` or a slice
	// `[m:n]` (with optionally-omitted bounds). All-index access reads a single 1-based element (the
	// element type); if any spec is a slice the access returns a sub-array (the array type), and a
	// scalar index i then means 1:i (PG). An out-of-bounds / NULL subscript yields NULL (PG, not an
	// error); a non-array base is 42804 at resolve.
	ExprSubscript
)

// SubscriptSpec is one subscript spec inside an ExprSubscript (spec/design/array.md §6): an index
// `[i]` (IsSlice false, Index set) or a slice `[m:n]` (IsSlice true; Lower/Upper may be nil for an
// omitted bound `[:n]`/`[m:]`/`[:]`).
type SubscriptSpec struct {
	IsSlice bool
	Index   *Expr
	Lower   *Expr
	Upper   *Expr
}

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
	// OpNe is <> (alias !=): the 3VL negation of OpEq, propagating NULL like OpEq.
	OpNe
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
	// OpConcat is the `||` array concatenation operator (spec/design/array-functions.md §8):
	// array∥array (array_cat), array∥element (array_append), element∥array (array_prepend),
	// resolved polymorphically.
	OpConcat
	// OpContains/OpContainedBy/OpOverlaps are the array containment/overlap operators `@>`/`<@`/`&&`
	// (spec/design/array-functions.md §10): each `anyarray <op> anyarray → boolean`, resolved
	// polymorphically. The range surface (spec/design/range-functions.md §3) reuses these three
	// (range operands route to the range axis) and adds the five positional/adjacency operators below.
	OpContains
	OpContainedBy
	OpOverlaps
	// OpStrictlyLeft/OpStrictlyRight/OpNotExtendRight/OpNotExtendLeft/OpAdjacent are the range boolean
	// operators `<<`/`>>`/`&<`/`&>`/`-|-` (spec/design/range-functions.md §3, RF3): range-only,
	// `anyrange <op> anyrange → boolean`.
	OpStrictlyLeft
	OpStrictlyRight
	OpNotExtendRight
	OpNotExtendLeft
	OpAdjacent
)

// Expr is a general expression, shared by the SELECT list, WHERE, and UPDATE ... SET.
// Kind selects which fields are meaningful. A comparison/logical/null-test node is
// boolean-valued; arithmetic and columns/integer-literals are integer-valued.
type Expr struct {
	Kind        ExprKind
	Column      string       // ExprColumn, ExprQualifiedColumn (the column name)
	Qualifier   string       // ExprQualifiedColumn (the relation label)
	Param       uint64       // ExprParam (the 1-based bind-parameter index)
	Literal     *Literal     // ExprLiteral
	TypeLitName string       // ExprTypedLiteral (the named type, e.g. "integer", "interval")
	TypeLitText string       // ExprTypedLiteral (the literal's string, coerced to the type at resolve)
	Cast        *CastExpr    // ExprCast
	Extract     *ExtractExpr // ExprExtract
	Collate     *CollateExpr // ExprCollate
	Unary       *UnaryExpr   // ExprUnary
	Binary      *BinaryExpr
	IsNullOf    *IsNullExpr     // ExprIsNull
	IsDistinct  *IsDistinctExpr // ExprIsDistinct
	FuncCall    *FuncCallExpr   // ExprFuncCall
	In          *InExpr         // ExprIn
	Between     *BetweenExpr    // ExprBetween
	Like        *LikeExpr       // ExprLike
	Regex       *RegexExpr      // ExprRegex
	Case        *CaseExpr       // ExprCase
	Subquery    *QueryExpr      // ExprScalarSubquery, ExprExists (the inner query)
	InSubquery  *InSubqueryExpr // ExprInSubquery
	Quantified  *QuantifiedExpr // ExprQuantified

	QuantifiedSubquery *QuantifiedSubqueryExpr // ExprQuantifiedSubquery
	RowItems           []Expr                  // ExprRow (the ROW(...) field expressions, in order)
	// Base is the operand of a field-selection postfix (ExprFieldAccess / ExprFieldStar): the
	// composite expression `(base).field` / `(base).*` selects from (spec/design/composite.md §S4).
	Base *Expr
	// Field is the selected field name of an ExprFieldAccess (the `.field` part); lookup is
	// case-insensitive at resolve.
	Field string
	// Subscripts are the bracketed specs of an ExprSubscript (`Base[..][..]`) — one or more index /
	// slice specs (spec/design/array.md §6). Base holds the array operand.
	Subscripts []SubscriptSpec
}

// InSubqueryExpr is `Lhs [NOT] IN ( Query )` (spec/design/grammar.md §26) — membership of Lhs in
// Query's single output column (three-valued, like a literal IN).
type InSubqueryExpr struct {
	Lhs     Expr
	Query   QueryExpr
	Negated bool
}

// QuantifiedExpr is `Lhs Op ANY/SOME/ALL ( Array )` — a quantified array comparison
// (spec/design/array-functions.md §11), the array spelling of IN. Op is a comparison
// (OpEq/OpLt/OpGt/OpLe/OpGe); All is true for ALL, false for ANY/SOME (SOME folds to ANY at
// parse). Array resolves to an array type; the three-valued fold over its flattened elements
// reuses the IN-list membership semantics, generalized to all five operators and both quantifiers.
type QuantifiedExpr struct {
	Op    BinaryOp
	All   bool
	Lhs   Expr
	Array Expr
}

// QuantifiedSubqueryExpr is `Lhs Op ANY/SOME/ALL ( Query )` — the SUBQUERY form of the quantified
// comparison (spec/design/array-functions.md §11.6), the subquery spelling of IN. Op/All as in
// QuantifiedExpr; Query's single column (42601 if >1) folds through the same three-valued fold.
type QuantifiedSubqueryExpr struct {
	Op    BinaryOp
	All   bool
	Lhs   Expr
	Query QueryExpr
}

// CastExpr is CAST(Inner AS TypeName). TypeMod is the optional numeric(p[,s]) modifier.
type CastExpr struct {
	Inner    Expr
	TypeName string
	TypeMod  *TypeMod
}

// ExtractExpr is EXTRACT(Field FROM Source) (spec/design/timezones.md §9.2, grammar.md §50) — the
// datetime field special form. Field is the syntactic field name (identifier or string literal,
// lowercased at parse); Source is the datetime expression. Resolves to a numeric.
type ExtractExpr struct {
	Field  string
	Source Expr
}

// CollateExpr is `Inner COLLATE "Collation"` — the postfix collation operator
// (spec/design/collation.md §1). It sets an EXPLICIT collation on a text expression for the
// surrounding comparison / ORDER BY; it binds at the postfix/typecast level (tighter than || and
// the comparisons — PG precedence). Collation is a quoted identifier (case-sensitive, e.g. "C",
// "en-US"). A non-text Inner is 42804, an unloaded name 42704, two different explicit collations
// in one comparison 42P21.
type CollateExpr struct {
	Inner     Expr
	Collation string
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
// Variadic is true when the final argument was prefixed with the VARIADIC keyword
// (num_nulls(VARIADIC arr), array-functions.md §12 / grammar.md §17): the array is passed
// directly to a variadic parameter rather than spreading individual arguments. false for every
// ordinary call (the all-positional/spread fast path).
type FuncCallExpr struct {
	Name     string
	Args     []*Expr
	ArgNames []*string
	Star     bool
	Variadic bool
	// Over is set when the call carries a trailing OVER (...) window clause (a WINDOW-function
	// call — spec/design/window.md). nil for an ordinary scalar/aggregate/SRF call. A window-only
	// function (row_number/…) with Over == nil is 42809; an aggregate with Over set is a window
	// aggregate (S3, deferred).
	Over *WindowDef
	// OverName is the referenced named window when the call is `f(...) OVER name` (the WINDOW
	// clause — spec/design/window.md §5); "" for an inline `OVER (...)` or a non-window call. A
	// desugaring pass replaces it with the named definition (into Over) before resolution; exactly
	// one of Over/OverName is set on a window call.
	OverName string
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
// matcher. Negated carries the NOT keyword; Insensitive carries ILIKE (case-insensitive matching,
// both sides simple-lowercased under the casing regime — collation.md §16).
type LikeExpr struct {
	Lhs         Expr
	Rhs         Expr
	Negated     bool
	Insensitive bool
}

// RegexExpr is `Lhs ~ Rhs` / `~*` / `!~` / `!~*` — a regular-expression match (grammar.md §22b,
// regex.md). jed's own RE2-able flavor (not PostgreSQL-compatible), matched by a hand-written
// linear-time Pike VM. UNANCHORED (matches a substring). Both operands must be text; NULL
// propagates. Negated carries `!~`/`!~*`; Insensitive carries `~*`/`!~*` (case-insensitive, both
// sides simple-lowercased like ILIKE — collation.md §16).
type RegexExpr struct {
	Lhs         Expr
	Rhs         Expr
	Negated     bool
	Insensitive bool
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
	Qualifier string
	Column    string
	// Collation is an optional explicit `COLLATE "name"` on this sort key (spec/design/collation.md
	// §1); "" means the column's collation (the database default, C, until slice 1d). A non-C name
	// orders this key by that collation's UCA sort key; an unknown name is 42704, a non-text column
	// with a COLLATE is 42804.
	Collation  string
	Descending bool
	NullsFirst bool
}

// WindowDef is a window definition — the body of an OVER (...) clause (spec/design/window.md §3).
// S0 carries PARTITION BY columns and an ORDER BY; a base-window name is deferred (S5). Partition
// is narrowed to columns in S0 (the GROUP BY/ORDER BY narrowing — general expressions are a
// follow-on); Order reuses the query ORDER BY sort keys. Frame carries an explicit frame clause
// (`ROWS BETWEEN … AND …`), or nil for the default frame (spec/design/window.md §6). S4 supports
// ROWS mode; explicit RANGE/GROUPS and EXCLUDE are parsed but rejected 0A000 at resolve.
type WindowDef struct {
	Partition []Expr
	Order     []OrderKey
	Frame     *WindowFrame
}

// WindowFrame is a window frame clause (spec/design/window.md §6).
type WindowFrame struct {
	Mode  FrameMode
	Start FrameBound
	End   FrameBound
}

// FrameMode is the frame unit: ROWS, RANGE, or GROUPS. S4 supports ROWS only.
type FrameMode int

const (
	// FrameRows is a physical-row frame (ROWS).
	FrameRows FrameMode = iota
	// FrameRange is a value-range frame (RANGE) — parsed, deferred 0A000.
	FrameRange
	// FrameGroups is a peer-group frame (GROUPS) — parsed, deferred 0A000.
	FrameGroups
)

// FrameBoundKind distinguishes the five frame-boundary forms.
type FrameBoundKind int

const (
	// FrameUnboundedPreceding is UNBOUNDED PRECEDING.
	FrameUnboundedPreceding FrameBoundKind = iota
	// FramePreceding is `expr PRECEDING`; Offset carries the offset expression.
	FramePreceding
	// FrameCurrentRow is CURRENT ROW.
	FrameCurrentRow
	// FrameFollowing is `expr FOLLOWING`; Offset carries the offset expression.
	FrameFollowing
	// FrameUnboundedFollowing is UNBOUNDED FOLLOWING.
	FrameUnboundedFollowing
)

// FrameBound is one frame boundary. Offset carries the offset expression for FramePreceding /
// FrameFollowing (a non-negative integer in ROWS/GROUPS; a value offset in RANGE), nil otherwise.
type FrameBound struct {
	Kind   FrameBoundKind
	Offset Expr
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
// fit; with no context it defaults to i64 (spec/design/types.md §6). A boolean
// literal is expression-only this slice (it cannot be stored).
type Literal struct {
	Kind LiteralKind
	Int  int64
	Bool bool
	Str  string  // LiteralText
	Dec  Decimal // LiteralDecimal
}
