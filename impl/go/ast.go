package jed

// Abstract syntax for the step-1 SQL surface. Boring, explicit shapes (CLAUDE.md
// §10); the hand-written parser produces these.

// Statement is a parsed top-level statement (exactly one of the fields is set).
type statement struct {
	CreateTable *createTable
	DropTable   *dropTable
	CreateIndex *createIndex
	DropIndex   *dropIndex
	CreateType  *createType
	DropType    *dropType
	// CreateSequence/AlterSequence/DropSequence are the sequence DDL statements
	// (spec/design/sequences.md): a named, persisted i64 generator. Non-nil only for that statement.
	CreateSequence *createSequence
	AlterSequence  *alterSequence
	DropSequence   *dropSequence
	Insert         *insert
	Select         *selectStmt
	// SetOp is a set operation (UNION/INTERSECT/EXCEPT) combining two query expressions
	// (spec/design/grammar.md §25). Non-nil only when at least one set operator is present; a
	// lone SELECT stays in Select, so the plain-query path and host API are untouched.
	SetOp *setOp
	// With is a query prefixed by a WITH clause defining one or more common table expressions
	// (spec/design/cte.md). Non-nil only when a WITH is present; a plain query stays Select/SetOp,
	// so the host API and the no-CTE paths are untouched (the SetOp precedent).
	With   *withQuery
	Update *update
	Delete *deleteStmt
	// Explain is `EXPLAIN [ANALYZE] <statement>` — render the planner's chosen plan for the inner
	// statement instead of running it (spec/design/explain.md). Non-nil only for an EXPLAIN. Plain
	// EXPLAIN plans but never executes; EXPLAIN ANALYZE runs the inner and reports its actual cost.
	Explain *explain
	// Begin/Commit/Rollback are the explicit transaction-control statements (grammar.md §27,
	// transactions.md §4.2). Non-nil only for that statement.
	Begin    *begin
	Commit   *commit
	Rollback *rollback
}

// explain is a parsed `EXPLAIN [ANALYZE] <statement>` (spec/design/explain.md). Inner is the wrapped
// statement (restricted to a query or DML by the parser — never DDL, transaction control, or a nested
// EXPLAIN). Analyze true ⇒ EXPLAIN ANALYZE: the inner statement is executed and its actual accrued
// cost + row count are reported; false ⇒ the plan is rendered without executing the inner statement.
type explain struct {
	Analyze bool
	Inner   *statement
}

// Begin is a BEGIN [TRANSACTION|WORK] [READ ONLY|READ WRITE] / START TRANSACTION [...] statement
// — open an explicit transaction block (spec/design/grammar.md §27). Writable is the *requested*
// access mode when ModeSet (true READ WRITE, false READ ONLY — a write inside → 25006). With
// ModeSet false the mode was unspecified and defaults to the handle's — READ WRITE normally,
// READ ONLY on a read-only handle (api.md §2.1). A nested BEGIN is 25001 (transactions.md §4.2).
type begin struct {
	Writable bool
	ModeSet  bool
}

// Commit is a COMMIT [TRANSACTION|WORK] / END [...] statement — publish the open block durably and
// return to autocommit; a COMMIT with no open block is a no-op success (transactions.md §4.2).
type commit struct{}

// Rollback is a ROLLBACK [TRANSACTION|WORK] statement — discard the open block's working set and
// return to autocommit; a ROLLBACK with no open block is a no-op success (transactions.md §4.2).
type rollback struct{}

// CreateTable is a CREATE TABLE statement.
type createTable struct {
	Name string
	// Temp is whether `TEMP` / `TEMPORARY` preceded `TABLE` — a session-local temporary table
	// (spec/design/temp-tables.md). A temp table makes ZERO writes to the database file (it lives
	// outside the serialized snapshot) and is dropped at session close. Its DDL is gated by
	// allowTempDDL rather than allowDDL (temp-tables.md §5).
	Temp    bool
	Columns []columnDef
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
	Checks []checkDef
	// Uniques is every `[CONSTRAINT name] UNIQUE [(cols)]` of the statement — the
	// column-level form collects as a one-member list — in TEXTUAL DEFINITION ORDER (it
	// drives member resolution, the dedup/PK fold, and naming —
	// spec/design/constraints.md §5). Each survivor becomes a unique secondary index
	// (spec/design/indexes.md §8).
	Uniques []uniqueDef
	// ForeignKeys is every `FOREIGN KEY (cols) REFERENCES …` of the statement — the
	// column-level `REFERENCES` form collects as a one-member list — in TEXTUAL DEFINITION
	// ORDER (it drives resolution and naming — spec/design/constraints.md §6). CREATE TABLE's
	// execution resolves each (42703/42701/42P01/42830/42804), rejects unsupported actions
	// (0A000), and names the unnamed ones (42710).
	ForeignKeys []foreignKeyDef
	// Excludes is every table-level `[CONSTRAINT name] EXCLUDE [USING gist] (col WITH op [, …])`
	// of the statement, in TEXTUAL DEFINITION ORDER (spec/design/gist.md §7). Execution resolves
	// each element (42703/42701/42704/0A000), builds the backing multi-column GiST index, and
	// names the unnamed ones (42P07/42710).
	Excludes []excludeDef
}

// ExcludeDef is one parsed EXCLUDE constraint (spec/design/gist.md §7, grammar.md): the optional
// explicit CONSTRAINT name (empty = unnamed; it names the backing GiST index), the optional USING
// method (empty = the default gist; anything else is 42704 at execution), and the (column, operator)
// element list in declaration order. Each operand is a bare column name; the operator is the WITH
// operator's source text (= or &&). Execution resolves the columns + operators
// (42703/42701/42704/0A000), creates the multi-column GiST index, and names the unnamed ones.
type excludeDef struct {
	Name  string
	Using string
	// Elements is (column name, operator source text) per element, in declaration order.
	Elements []excludeElementDef
}

// ExcludeElementDef is one parsed (column WITH operator) element of an EXCLUDE constraint.
type excludeElementDef struct {
	Column string
	Op     string
}

// RefAction is a referential action for `ON DELETE` / `ON UPDATE`
// (spec/design/constraints.md §6.6). Only NoAction (the default) and Restrict are
// supported — identical in jed (no deferrable constraints); the write-actions parse but
// are rejected 0A000 at CREATE TABLE.
type refAction int

const (
	// RefNoAction is the default NO ACTION.
	refNoAction refAction = iota
	// RefRestrict is RESTRICT.
	refRestrict
	// RefCascade is CASCADE (parses; rejected 0A000).
	refCascade
	// RefSetNull is SET NULL (parses; rejected 0A000).
	refSetNull
	// RefSetDefault is SET DEFAULT (parses; rejected 0A000).
	refSetDefault
)

// ForeignKeyDef is one parsed `FOREIGN KEY` / `REFERENCES` constraint (spec/design/grammar.md
// §43): the optional explicit CONSTRAINT name (empty = unnamed), the local (referencing)
// column names in list order, the referenced (parent) table name, the optional referenced
// column names (RefColumns nil = the parent's primary key), and the ON DELETE / ON UPDATE
// actions. Execution resolves it (42703/42701/42P01/42830/42804) and names the unnamed ones
// (42710) — spec/design/constraints.md §6.
type foreignKeyDef struct {
	Name       string
	Columns    []string
	RefTable   string
	RefColumns []string // nil = default to the parent's primary key
	OnDelete   refAction
	OnUpdate   refAction
}

// UniqueDef is one parsed UNIQUE constraint (spec/design/grammar.md §31): the optional
// explicit CONSTRAINT name (empty = unnamed; it names the backing index) and the member
// column names in list order. Execution resolves the members (42703/42701/0A000) and
// names the index (42P07/42710) — spec/design/constraints.md §5.
type uniqueDef struct {
	Name    string
	Columns []string
}

// CheckDef is one parsed CHECK constraint (spec/design/grammar.md §29): the optional
// explicit CONSTRAINT name (empty = unnamed), the expression, and the expression's
// persisted text — the source token sequence between the parentheses re-rendered per the
// closed table in spec/fileformat/format.md "Check-expression text".
type checkDef struct {
	Name string
	Expr exprNode
	Text string
}

// DefaultDef is a parsed DEFAULT <expr> column constraint (spec/design/constraints.md §2):
// the default expression and its persisted text (the source token sequence re-rendered per
// the closed table in spec/fileformat/format.md "Check-expression text", as a CHECK is).
// Execution classifies it: a bare *Literal is a constant (pre-evaluated at CREATE TABLE), any
// other expression is stored as text and evaluated per row at INSERT.
type defaultDef struct {
	Expr exprNode
	Text string
}

// DropTable is a DROP TABLE [IF EXISTS] <name> [, …] [CASCADE | RESTRICT] statement. Removes
// one or more tables — their definitions and all their rows — from the catalog. A comma list
// is dropped two-phase / all-or-nothing (validate every name, then remove); a repeated name is
// deduplicated (PG-faithful). A missing table without IF EXISTS is an error (42P01); with
// IF EXISTS it is a no-op success (PostgreSQL turns the missing-table error into a notice).
// IF EXISTS suppresses only the missing-table error — a name that resolves to a non-table
// relation (an index) is still the 42809 wrong-object-type error. The trailing keyword picks
// the FK-dependency mode: RESTRICT (default) refuses to drop a table another table's FK
// references (2BP01); CASCADE drops those dependent FK constraints with it. A FK between two
// tables both in the same statement never blocks. See spec/design/grammar.md §13.
type dropTable struct {
	Names    []string
	IfExists bool
	Cascade  bool
}

// CreateIndex is a CREATE [UNIQUE] INDEX [name] ON <table> ( col [, col]* ) statement —
// a secondary index (spec/design/indexes.md, grammar.md §30). Name == "" is the unnamed
// form; the executor derives PostgreSQL's auto-name. Key columns are bare names (no
// expression/ordered/partial keys this slice); a column may repeat (PG allows it).
// Execution validates in PG's order: table 42P01, columns 42703/0A000, name collision
// 42P07. A Unique index additionally verifies the existing rows at build (23505) and
// enforces uniqueness thereafter (spec/design/indexes.md §8).
type createIndex struct {
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
type dropIndex struct {
	Name string
}

// CreateType is a CREATE TYPE <name> AS ( field type [NOT NULL] [, …] ) statement — a
// user-defined composite (row) type (spec/design/composite.md, grammar.md). Execution resolves
// each field's type (a built-in scalar or a previously-defined composite — 42704 if unknown),
// rejects a duplicate type name (42710), and registers it in the catalog. Named composites only
// this slice; anonymous record is not supported.
type createType struct {
	Name   string
	Fields []typeFieldDef
}

// TypeFieldDef is one field of a CREATE TYPE definition: its name, its type as written (a built-in
// scalar alias or a composite type name), an optional numeric(p,s) modifier, and an explicit
// NOT NULL. Resolved at execution (mirrors ColumnDef).
type typeFieldDef struct {
	Name     string
	TypeName string
	TypeMod  *typeMod
	NotNull  bool
}

// DropType is a DROP TYPE [IF EXISTS] <name> [RESTRICT] statement — remove a composite type
// (spec/design/composite.md §7). RESTRICT (the default and only behavior this slice) fails with
// 2BP01 if a table column or another composite type still references it; CASCADE is 0A000. A
// missing type without IF EXISTS is 42704.
type dropType struct {
	Name     string
	IfExists bool
}

// SeqOptions is the parsed, order-free sequence-option set shared by CREATE SEQUENCE and an
// IDENTITY column's optional `( seq_options )` (spec/design/sequences.md §13). Each is captured as
// a parsed override, with a nil pointer meaning "use the default" (resolved at execution against
// the INCREMENT sign); execution validates the set (22023).
type seqOptions struct {
	// DataType is the `AS <type>` value type as written (the raw type name, e.g. "smallint" /
	// "int4"), resolved to a SeqDataType at execution (spec/design/sequences.md §14); "" = the
	// bigint default. A non-integer type is 22023. Inside an IDENTITY column's options a set
	// DataType is 42601 (the column type fixes it).
	DataType  string
	Increment *int64
	// MinValue is the MINVALUE override: a SeqBound whose NoValue distinguishes
	// MINVALUE v (Value=v) from NO MINVALUE (NoValue) from unset (nil) — the
	// Rust Option<Option<i64>>. nil = unset (use the default).
	MinValue *seqBound
	MaxValue *seqBound
	Start    *int64
	Cache    *int64
	Cycle    *bool
}

// CreateSequence is a CREATE SEQUENCE [IF NOT EXISTS] <name> [options] statement — a named,
// persisted i64 generator (spec/design/sequences.md). Execution validates the option set (22023),
// rejects a relation-namespace collision (42P07 unless IfNotExists), and registers the sequence.
type createSequence struct {
	Name        string
	IfNotExists bool
	Options     seqOptions
}

// IdentitySpec is a column's `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [( seq_options )]`
// constraint (spec/design/sequences.md §13). Always distinguishes ALWAYS (true) from BY DEFAULT
// (false); Options tunes the auto-created owned sequence (defaults to the standard ascending i64).
type identitySpec struct {
	Always  bool
	Options seqOptions
}

// SeqBound is a MINVALUE/MAXVALUE override (spec/design/sequences.md): NoValue true selects
// the type default (NO MINVALUE / NO MAXVALUE); otherwise Value is the explicit bound. A nil
// *SeqBound on CreateSequence means the option was unset (also the default).
type seqBound struct {
	NoValue bool
	Value   int64
}

// DropSequence is a DROP SEQUENCE [IF EXISTS] <name> [, …] [RESTRICT] statement — remove one or
// more sequences (spec/design/sequences.md §1). A missing sequence without IF EXISTS is 42P01;
// CASCADE is 0A000 (RESTRICT is the default and only mode this slice).
type dropSequence struct {
	Names    []string
	IfExists bool
}

// AlterSequence is an ALTER SEQUENCE [IF EXISTS] <name> <action> statement (spec/design/sequences.md
// §4/§15). A missing sequence without IfExists is 42P01. RenameTo != "" selects the RENAME TO form;
// otherwise the option form (Options + Restart) applies. The two are mutually exclusive (the parser
// requires ≥ 1 option/RESTART for the option form — a bare ALTER SEQUENCE s is 42601).
type alterSequence struct {
	Name     string
	IfExists bool
	// RenameTo is the new name for the RENAME TO form, or "" for the option form.
	RenameTo string
	// Options + Restart describe the option form. Restart mirrors Rust's Option<Option<i64>>:
	// nil = no RESTART; non-nil with ToStart = bare RESTART (reset to the stored START); non-nil
	// with a Value = RESTART WITH Value.
	Options seqOptions
	Restart *seqRestart
}

// SeqRestart is a parsed RESTART pseudo-option on ALTER SEQUENCE (spec/design/sequences.md §15):
// ToStart true is a bare RESTART (reset to the stored START); otherwise Value is the RESTART WITH n.
type seqRestart struct {
	ToStart bool
	Value   int64
}

// ColumnDef is a column definition in a CREATE TABLE.
type columnDef struct {
	Name string
	// TypeName as written (canonical or alias); resolved during analysis.
	TypeName string
	// TypeMod is an optional parenthesized type modifier, numeric(p[,s]) — the first
	// parameterized type. Meaningful only for decimal; validated at resolve (grammar.md §14).
	TypeMod    *typeMod
	PrimaryKey bool
	// NotNull is an explicit NOT NULL column constraint. A PRIMARY KEY column is implicitly
	// NOT NULL regardless of this flag; the executor ORs the two (spec/design/constraints.md).
	NotNull bool
	// Default is an optional DEFAULT <expr> — the value for this column when a row omits it (or
	// uses the DEFAULT keyword). A constant literal is pre-evaluated at CREATE TABLE; any other
	// expression is evaluated per row at INSERT (spec/design/constraints.md §2). nil = no default.
	Default *defaultDef
	// Identity is an optional `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [( opts )]` constraint
	// (spec/design/sequences.md §13). Desugars like serial (an owned sequence + a nextval default +
	// NOT NULL) plus the persisted ALWAYS/BY DEFAULT distinction. nil = a non-identity column.
	Identity *identitySpec
	// Collation is an optional `COLLATE "name"` column modifier (spec/design/collation.md §1) — a
	// quoted, case-sensitive collation name. Text-only (else 42804); the name must be loaded or "C"
	// (else 42704). "" = no clause ⇒ inherit the per-database default. Frozen into the column at
	// CREATE TABLE.
	Collation string
}

// TypeMod is a parsed type modifier: a precision and an optional scale, as written
// (numeric(p) → Scale nil, numeric(p,s) → Scale set). The values are the raw lexed magnitudes;
// range validation (1..=1000, 0..=p; else 22023) is at resolve.
type typeMod struct {
	Precision uint64
	Scale     *uint64
}

// Insert is an INSERT ... [(col, ..)] whose rows come from EITHER a VALUES list (each value a
// literal or the DEFAULT keyword) OR a SELECT (INSERT ... SELECT — spec/design/grammar.md §24).
// An INSERT is two-phase / all-or-nothing — every row is validated before any is stored
// (spec/design/grammar.md §12).
type insert struct {
	Table string
	// DB is the optional database qualifier on the target (`INSERT INTO reports.t …`), like
	// tableRef.DB (spec/design/attached-databases.md §3). Nil = implicit scope.
	DB *string
	// Columns is the optional explicit column list (`INSERT INTO t (a, c) VALUES ...` /
	// `... SELECT ...`); nil is the positional form (every column, in declaration order). Names
	// resolve at execution time (unknown → 42703, duplicate → 42701); an unlisted column takes
	// its default else NULL.
	Columns []string
	// Overriding is the optional `OVERRIDING { SYSTEM | USER } VALUE` clause
	// (spec/design/sequences.md §13), governing IDENTITY columns. nil is the default (no override).
	Overriding *overridingKind
	// EXACTLY ONE of Rows / Select is set (the parser guarantees it). Rows is the VALUES source:
	// each inner slice is one row's values in the order of Columns (or column order when Columns
	// is nil); non-empty when set, nil when Select is set. Select is the SELECT source: nil when
	// Rows is set.
	Rows   [][]insertValue
	Select *selectStmt
	// OnConflict is the optional ON CONFLICT clause (UPSERT — spec/design/upsert.md), between the
	// source and RETURNING. Nil = no clause (a conflict traps 23505 as usual).
	OnConflict *onConflict
	// Returning is the optional terminal RETURNING clause (spec/design/grammar.md §32):
	// project each stored row, turning the statement into a query result. Nil = no clause.
	Returning *selectItems
}

// OnConflict is the `ON CONFLICT [target] action` clause (spec/design/upsert.md §1).
type onConflict struct {
	// Target is the optional conflict target (the arbiter). Nil is legal only with DoNothing
	// (any uniqueness conflict is then skipped); DoUpdate with a nil target is 42601.
	Target *conflictTarget
	// DoUpdate true = DO UPDATE (Assignments + Filter apply); false = DO NOTHING.
	DoUpdate    bool
	Assignments []assignment // DO UPDATE SET … (empty for DO NOTHING)
	Filter      *exprNode    // optional DO UPDATE WHERE … (nil otherwise)
}

// ConflictTarget is the arbiter constraint named by an ON CONFLICT target (spec/design/upsert.md
// §2). EXACTLY ONE of Columns / Constraint is meaningful (IsConstraint selects which).
type conflictTarget struct {
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
type overridingKind int

const (
	overridingSystem overridingKind = iota
	overridingUser
)

// InsertValue is one value slot in an INSERT VALUES row: a literal, a bind parameter ($N,
// bound at execute — spec/design/api.md §5), or the DEFAULT keyword (IsDefault) — which
// substitutes the target column's declared default (or NULL if it has none). The DEFAULT
// keyword is not reserved (spec/design/grammar.md §3). constraints.md §2.
type insertValue struct {
	IsDefault bool
	IsParam   bool    // a $N bind parameter; Param holds the 1-based index
	Param     uint64  // valid when IsParam
	Lit       literal // valid when !IsDefault && !IsParam && !IsRow
	// IsRow marks a ROW(...) composite constructor (spec/design/composite.md §1); Row holds the
	// field slots, in order (each itself a literal, a $N, or a nested ROW). A composite-typed column
	// takes a ROW(...) value in INSERT VALUES.
	IsRow bool
	Row   []insertValue // valid when IsRow
	// IsArray marks an ARRAY[...] constructor (spec/design/array.md §1); Array holds the element
	// slots, in order (each a literal or a $N). An array-typed column takes an ARRAY[...] value.
	IsArray bool
	Array   []insertValue // valid when IsArray
}

// Update is `UPDATE <table> SET <col> = <expr> [, ...] [WHERE <expr>]`. Each
// assignment's right-hand side is evaluated against the pre-update row (so
// `SET a = b, b = a` swaps). Assigning a PRIMARY KEY column re-keys the row — the storage
// key is recomputed and the row moves (see the executor). The WHERE expression must
// resolve to boolean.
type update struct {
	Table string
	// DB is the optional database qualifier on the target (`UPDATE reports.t SET …`), like
	// tableRef.DB (spec/design/attached-databases.md §3). Nil = implicit scope.
	DB          *string
	Assignments []assignment
	Filter      *exprNode
	// Returning is the optional terminal RETURNING clause (spec/design/grammar.md §32):
	// project each matched row's NEW (post-assignment) values. Nil = no clause.
	Returning *selectItems
}

// Assignment is one `SET <Column> = <Value>` clause; Value is a general expression.
type assignment struct {
	Column string
	Value  exprNode
}

// Delete is `DELETE FROM <table> [WHERE <expr>]`. No WHERE deletes every row; the
// WHERE expression must resolve to boolean.
type deleteStmt struct {
	Table string
	// DB is the optional database qualifier on the target (`DELETE FROM reports.t …`), like
	// tableRef.DB (spec/design/attached-databases.md §3). Nil = implicit scope.
	DB     *string
	Filter *exprNode
	// Returning is the optional terminal RETURNING clause (spec/design/grammar.md §32):
	// project each deleted row's OLD values. Nil = no clause.
	Returning *selectItems
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
type tableRef struct {
	Name string
	// DB is the optional database qualifier (`reports.sales` → DB="reports", Name="sales"), jed's
	// first multi-part name in table position (spec/design/attached-databases.md §3). Nil = a bare,
	// implicit-scope name. Only the reserved implicit qualifiers `main` (the file database) and `temp`
	// (the session-local domain) resolve this slice; any other is 42P01 (Slice 1b adds host-attached
	// databases). Never set on the function / derived-table alternatives.
	DB            *string
	Alias         *string
	IsFunc        bool
	Args          []*exprNode
	Subquery      *queryExpr
	Values        [][]*exprNode
	ColumnAliases []string
	// ColumnDefs is a FROM-clause **column-definition list** `AS t(col type, …)` (C0, json-table.md
	// §1): the typed columns a record-returning function (`json[b]_to_record(set)`) declares. Mutually
	// exclusive with ColumnAliases (a rename-only list). Nil for an ordinary table / SRF.
	ColumnDefs []typeFieldDef
	// JsonTable is a `JSON_TABLE(...)` table source (json-table.md §3, T1) — projects a JSON document
	// into a relation via the `COLUMNS` clause. When non-nil, the other source fields (`Name`/`Args`/…)
	// are unused. Implicitly lateral (its `Ctx` may reference earlier FROM siblings).
	JsonTable *jsonTable
	Lateral   bool
}

// JsonTable is a `JSON_TABLE(ctx, path [AS name] COLUMNS (col, …))` table source (json-table.md §3,
// T1). The root `path` is evaluated over `ctx` to a sequence of row items; the `COLUMNS` tree
// projects each item (and, via `NESTED PATH`, child items) into relational columns under the default
// plan (parent→child LEFT OUTER, sibling NESTED paths UNIONed).
type jsonTable struct {
	Ctx     *exprNode
	Path    *exprNode
	Columns []jtColumn
}

// JtColumn is one `JSON_TABLE` `COLUMNS` entry (json-table.md §3.3): an ordinality, a regular, an
// EXISTS, or a NESTED column. Modeled as a tagged union (one struct per kind, marker method).
type jtColumn interface{ isJtColumn() }

// JtColumnOrdinality is `name FOR ORDINALITY` — a per-level 1-based row counter (`integer`).
type jtColumnOrdinality struct {
	Name string
}

// JtColumnRegular is `name type [PATH p] [wrapper] [quotes] [ON EMPTY] [ON ERROR]` — a regular
// column: evaluate `p` (default `$.name`) over the current row item and coerce to `type` like
// JSON_VALUE (scalar) or JSON_QUERY (json/jsonb).
type jtColumnRegular struct {
	Name       string
	TypeName   string
	Array      bool
	Path       *string
	Wrapper    jsonWrapper
	KeepQuotes bool
	OnEmpty    *jsonOnBehavior
	OnError    *jsonOnBehavior
}

// JtColumnExists is `name type EXISTS [PATH p] [behavior ON ERROR]` — JSON_EXISTS of `p`, coerced
// to `type`.
type jtColumnExists struct {
	Name     string
	TypeName string
	Path     *string
	OnError  *jsonOnBehavior
}

// JtColumnNested is `NESTED [PATH] p [AS n] COLUMNS (…)` — recursively expand a child path over the
// row item.
type jtColumnNested struct {
	Path    string
	Columns []jtColumn
}

func (*jtColumnOrdinality) isJtColumn() {}
func (*jtColumnRegular) isJtColumn()    {}
func (*jtColumnExists) isJtColumn()     {}
func (*jtColumnNested) isJtColumn()     {}

// JoinKind is the kind of a join. Inner and Cross execute this slice; the Left/Right/Full
// outer kinds parse and are carried in the AST but executing one is a documented 0A000
// narrowing (the OUTER family is a fast-follow — spec/design/grammar.md §15).
type joinKind int

const (
	// JoinInner is INNER JOIN (a bare JOIN is INNER).
	joinInner joinKind = iota
	// JoinCross is CROSS JOIN (a Cartesian product, no ON).
	joinCross
	// JoinLeft is LEFT [OUTER] JOIN (deferred — 0A000 at execution).
	joinLeft
	// JoinRight is RIGHT [OUTER] JOIN (deferred — 0A000).
	joinRight
	// JoinFull is FULL [OUTER] JOIN (deferred — 0A000).
	joinFull
)

// JoinClause is one JOIN step in the left-deep FROM chain: the join kind, the right-hand
// table reference, and the optional ON predicate (nil for CROSS JOIN; set for INNER/outer,
// which require an ON). See spec/design/grammar.md §15.
type joinClause struct {
	Kind  joinKind
	Table tableRef
	On    *exprNode
	// Using is the `USING (col, …)` column list (spec/design/grammar.md §15), mutually exclusive
	// with On (a join has exactly one of ON/USING, or neither for CROSS/comma/NATURAL). Each named
	// column must exist in BOTH sides; the join matches on their equality and the output MERGES them
	// into a single column (FULL JOIN ... USING is a deferred 0A000). Non-nil only for an explicit
	// USING join.
	Using []string
	// Natural is true for a NATURAL join (spec/design/grammar.md §15): the USING column list is
	// DERIVED at resolution as the column names common to both sides (left order), then the merge
	// proceeds exactly like USING. With no common column it degenerates to a CROSS join. Mutually
	// exclusive with On/Using.
	Natural bool
	// Comma is true when this is the implicit CROSS JOIN synthesized from a comma in the FROM
	// list (`FROM a, b`). The comma binds LOOSER than JOIN, so each comma-separated FROM item is
	// its own ON-resolution segment: a later join's ON may not reference an earlier comma item
	// (matching PostgreSQL). This flag marks the segment boundary; it is otherwise an ordinary
	// CROSS join (Kind == JoinCross, On == nil). See spec/design/grammar.md §15.
	Comma bool
}

// Select is a SELECT. The FROM clause is a left-deep chain: From followed by zero or more
// Joins (empty = single-table). Filter (the WHERE expression) must resolve to boolean.
type selectStmt struct {
	// Distinct is SELECT DISTINCT — deduplicate the projected output rows (NULL-safe),
	// applied after ORDER BY and before LIMIT/OFFSET (spec/design/grammar.md §11).
	Distinct bool
	Items    selectItems
	// From is the first table reference of the FROM clause, or nil for a FROM-less SELECT —
	// the select list evaluates over one virtual zero-column row (spec/design/grammar.md §34).
	From *tableRef
	// Joins holds the left-deep JOINs after From (nil/empty = a single-table SELECT; always
	// empty when From is nil — joins exist only inside a FROM clause).
	Joins  []joinClause
	Filter *exprNode
	// GroupBy holds the GROUP BY grouping terms — GroupSet for plain keys (`GROUP BY a, b` →
	// two GroupSet items) plus the ROLLUP/CUBE/GROUPING SETS forms that expand to multiple
	// grouping sets (spec/design/aggregates.md §12). nil/empty means no GROUP BY. Every grouping
	// column is an ExprColumn or ExprQualifiedColumn (the parser restricts each to column_ref).
	GroupBy []groupItem
	// Having is the HAVING predicate (a boolean filter over the grouped rows), if any. May
	// reference aggregates and grouping keys; evaluated after aggregation, before ORDER BY.
	// HAVING makes a query an aggregate query even with no GROUP BY (spec/design/grammar.md §19).
	Having *exprNode
	// OrderBy holds the ORDER BY sort keys, applied left to right; nil/empty means no
	// ORDER BY (spec/design/grammar.md §10).
	OrderBy []orderKey
	// Limit caps the result at Limit rows; Offset skips the first Offset rows. Both are
	// non-negative counts, applied after ORDER BY, before projection (grammar.md §9). A
	// nil pointer means the clause is absent.
	Limit  *int64
	Offset *int64
	// Windows holds the named windows from a `WINDOW name AS (definition)` clause
	// (spec/design/window.md §5, grammar.ebnf window_clause), referenced by `OVER name`. Empty when
	// absent. Resolved by a desugaring pass that rewrites each `OVER name` to its definition (into
	// the call's Over) before resolution.
	Windows []namedWindow
}

// NamedWindow is one `name AS (definition)` entry of a WINDOW clause (spec/design/window.md §5).
type namedWindow struct {
	Name string
	Def  windowDef
}

// GroupItemKind tags a GroupItem (spec/design/aggregates.md §12).
type groupItemKind int

const (
	// GroupSet is a single grouping set's column list: a bare column `a` (Cols=[a]), a
	// parenthesized group `(a, b)` (Cols=[a,b]), or the empty set `()` (Cols=[]).
	groupSet groupItemKind = iota
	// GroupRollup is `ROLLUP (g1, …, gn)` — n+1 grouping sets (the prefixes of the column groups,
	// longest first down to empty). Groups holds the column groups.
	groupRollup
	// GroupCube is `CUBE (g1, …, gn)` — 2^n grouping sets (every subset of the column groups).
	groupCube
	// GroupGroupingSets is `GROUPING SETS (e1, …, en)` — the concatenation of each element's
	// expansion; an element may itself be any GroupItem (Elems holds them).
	groupGroupingSets
)

// GroupItem is one GROUP BY grouping term (spec/design/aggregates.md §12). Most queries use only
// GroupSet with one column each (plain `GROUP BY a, b`); the ROLLUP/CUBE/GROUPING SETS forms produce
// several grouping sets the resolver expands and cross-products. Each Expr is a bare/qualified column.
type groupItem struct {
	Kind   groupItemKind
	Cols   []exprNode   // GroupSet
	Groups [][]exprNode // GroupRollup / GroupCube
	Elems  []groupItem  // GroupGroupingSets
}

// forEachExpr visits every column Expr in this grouping term — used by the analysis walks that scan
// a SELECT's expressions (privilege collection, sublink / sequence-mutator detection).
func (g *groupItem) forEachExpr(f func(*exprNode)) {
	switch g.Kind {
	case groupSet:
		for i := range g.Cols {
			f(&g.Cols[i])
		}
	case groupRollup, groupCube:
		for i := range g.Groups {
			for j := range g.Groups[i] {
				f(&g.Groups[i][j])
			}
		}
	case groupGroupingSets:
		for i := range g.Elems {
			g.Elems[i].forEachExpr(f)
		}
	}
}

// QueryExpr is the operand of a set operation (spec/design/grammar.md §25): a single SELECT core, a
// nested set operation (so a chain like `a UNION b INTERSECT c` forms a tree), or a nested WITH
// clause (spec/design/cte.md §7). Exactly one field is non-nil.
type queryExpr struct {
	Select *selectStmt
	SetOp  *setOp
	With   *withExpr
}

// WithExpr is a nested `WITH … query_expr` (spec/design/cte.md §7): the CTE list Ctes (forward-only
// visibility; self-referencing when Recursive) prefixing the inner query expression Body, in a
// subquery / derived-table / CTE-body position — as opposed to the top-level WithQuery (which may
// prefix a data-modifying primary). The CTEs are visible only within Body (and to each other); the
// enclosing statement's CTE bindings are NOT inherited — a documented narrowing (cte.md §7). A
// data-modifying CTE here is rejected at planning (0A000 — PostgreSQL restricts a DML-WITH to the
// statement top level).
type withExpr struct {
	Ctes      []cte
	Recursive bool
	Body      *queryExpr
}

// SetOpKind is the set operator (spec/design/grammar.md §25).
type setOpKind int

const (
	setOpUnion setOpKind = iota
	setOpIntersect
	setOpExcept
)

// SetOp combines two query expressions (spec/design/grammar.md §25). All is the ALL (multiset)
// flag — false is the deduplicating default. The optional trailing ORDER BY / LIMIT / OFFSET apply
// to the WHOLE combined result and live on the outermost node only (an operand carries none — a
// deferred narrowing); OrderBy keys resolve against the output column names (the left operand's).
// Precedence is handled by the parser: INTERSECT binds tighter than UNION/EXCEPT (left-associative).
type setOp struct {
	Op      setOpKind
	All     bool
	Lhs     queryExpr
	Rhs     queryExpr
	OrderBy []orderKey
	Limit   *int64
	Offset  *int64
}

// CteBody is the body of a CTE, or the WITH-prefixed primary statement
// (spec/design/writable-cte.md): an ordinary query expression, or a data-modifying statement (a
// writable CTE). Exactly one field is non-nil (the QueryExpr/InsertSource sum-type precedent); the
// data-modifying variants carry the parsed INSERT/UPDATE/DELETE.
type cteBody struct {
	Query  *queryExpr
	Insert *insert
	Update *update
	Delete *deleteStmt
}

// AsQuery returns the query expression when this body is a plain query, else nil — used by the
// recursive-CTE analysis (only a query body can be a recursive UNION) and the pure-query WITH path.
func (b *cteBody) AsQuery() *queryExpr { return b.Query }

// IsDataModifying reports whether this body is a data-modifying statement (INSERT/UPDATE/DELETE).
func (b *cteBody) IsDataModifying() bool { return b.Query == nil }

// Cte is one common table expression in a WITH list (spec/design/cte.md). A named, statement-local
// relation backed by a query or (spec/design/writable-cte.md) a data-modifying statement. Columns is
// the optional column-rename list (renames the body's output columns left to right; a count mismatch
// is 42P10). Materialized is the explicit evaluation hint: a non-nil pointer to true is MATERIALIZED,
// to false is NOT MATERIALIZED, nil is PostgreSQL's default (inline a single-reference CTE,
// materialize a multi-reference one — cost.md §3; a data-modifying CTE is always materialized, the
// hint inert). Body is a cte_body.
type cte struct {
	Name         string
	Columns      []string
	Materialized *bool
	Body         cteBody
}

// WithQuery is a top-level statement prefixed by a WITH clause (spec/design/cte.md). Ctes is the
// non-empty list of common table expressions (each visible to later CTEs and to Body); Body is the
// main statement the CTEs prefix — a query, or (spec/design/writable-cte.md) a data-modifying
// INSERT/UPDATE/DELETE primary. Built only when a WITH is present — a plain query stays
// Statement{Select} / Statement{SetOp}, so those paths are untouched (the SetOp precedent).
// Recursive is the WITH RECURSIVE flag (spec/design/recursive-cte.md): a flag on the whole list that
// ENABLES a CTE to reference itself (lifting the forward-only 42P01); a CTE that does not reference
// itself is still an ordinary non-recursive CTE.
type withQuery struct {
	Ctes      []cte
	Body      cteBody
	Recursive bool
}

// SelectItems is either all columns (*) or a list of projected expressions.
type selectItems struct {
	All   bool
	Items []selectItem
}

// SelectItem is one select-list expression with its optional output-name alias
// (expr AS name). The alias is an output label only — it never enters resolution
// (spec/design/grammar.md §8). When Alias is nil the output name is derived by the
// resolver: a bare column's canonical name, or the fixed "?column?" otherwise.
type selectItem struct {
	Expr  exprNode
	Alias *string
}

// CompareOp is unused now that comparisons are BinaryOp; retained name removed.

// ExprKind tags an expression node (Go has no sum types; this is the discriminant —
// the one place this slice deviates from the "exactly one pointer set" idiom, because
// a Column is a bare string).
type exprKind int

const (
	// ExprColumn is a bare (unqualified) column reference (Column holds the name).
	exprColumn exprKind = iota
	// ExprQualifiedColumn is a qualified reference `rel.col` (Qualifier holds the relation
	// label, Column the column name); resolved against exactly that one relation, never
	// ambiguous (spec/design/grammar.md §15).
	exprQualifiedColumn
	// ExprLiteral is a literal value.
	exprLiteral
	// ExprTypedLiteral is a typed string literal `type '...'` — PostgreSQL's `type 'string'`,
	// equal to CAST('string' AS type) over a string-literal operand (spec/design/grammar.md §36).
	// TypeLitName names the target scalar (resolved by from_name; unknown → 42704), TypeLitText is
	// the literal's string; the string is coerced to the type at resolve. The keyword names the
	// type, so the literal carries it in any expression position (`INTERVAL '1 day'`, `INTEGER '42'`).
	exprTypedLiteral
	// ExprParam is a bind parameter $N (Param holds the 1-based index). Like an adaptable
	// literal it takes its type from context at resolve; the host binds a value at execute
	// (spec/design/api.md §5).
	exprParam
	// ExprCast is CAST(inner AS type).
	exprCast
	// ExprExtract is EXTRACT(field FROM source) — the datetime field special form (timezones.md §9.2,
	// grammar.md §50). The field is syntactic; resolves to a numeric.
	exprExtract
	// ExprCollate is `expr COLLATE "name"` — the postfix collation operator
	// (spec/design/collation.md §1).
	exprCollate
	// ExprUnary is a unary operator applied to one operand.
	exprUnary
	// ExprBinary is a binary operator over two operands.
	exprBinary
	// ExprIsNull is a postfix IS [NOT] NULL test.
	exprIsNull
	// ExprIsJson is `operand IS [NOT] JSON [VALUE|SCALAR|ARRAY|OBJECT] [(WITH|WITHOUT) UNIQUE [KEYS]]`
	// — the SQL/JSON well-formedness predicate (spec/design/json-sql-functions.md §5): is operand (a
	// character string / json / jsonb) well-formed JSON of the optional kind, with optionally unique
	// object keys. A non-string/json operand → 42804; a NULL operand → NULL; never raises.
	exprIsJson
	// ExprJsonCtor is `JSON(expr [(WITH|WITHOUT) UNIQUE [KEYS]])` — the SQL/JSON JSON() constructor
	// (spec/design/json-sql-functions.md §5): parse a character string to a `json` value (verbatim).
	// Malformed → 22P02; `WITH UNIQUE KEYS` on a duplicate object key → 22030. STRICT (NULL → NULL).
	exprJsonCtor
	// ExprIsDistinct is `lhs IS [NOT] DISTINCT FROM rhs` (NULL-safe equality).
	exprIsDistinct
	// ExprFuncCall is a function call — the shared aggregate/scalar call syntax
	// (spec/design/grammar.md §17): an aggregate or a scalar function (abs/round) resolve.
	exprFuncCall
	// ExprIn is `lhs IN (list)` / `lhs NOT IN (list)` — membership over a non-empty
	// value list, desugared at resolve to the OR-chain (spec/design/grammar.md §20).
	exprIn
	// ExprBetween is `lhs BETWEEN lo AND hi` / `lhs NOT BETWEEN lo AND hi` — a range test
	// desugared at resolve to `lhs >= lo AND lhs <= hi` (spec/design/grammar.md §21).
	exprBetween
	// ExprLike is `lhs LIKE rhs` / `lhs NOT LIKE rhs` — a text pattern match with a dedicated
	// matcher (spec/design/grammar.md §22).
	exprLike
	// ExprRegex is `lhs ~ rhs` / `~*` / `!~` / `!~*` — a regular-expression match with a hand-written
	// Pike VM (spec/design/grammar.md §22b, regex.md).
	exprRegex
	// ExprCase is a CASE expression (searched or simple form), lazily evaluated
	// (spec/design/grammar.md §23).
	exprCase
	// ExprScalarSubquery is a scalar subquery `( query_expr )` in expression position
	// (spec/design/grammar.md §26). resolve plans it once against the scope chain; an uncorrelated
	// one is then folded to a constant, a correlated one is re-executed per outer row.
	exprScalarSubquery
	// ExprExists is `EXISTS ( query_expr )` (a leading NOT is the ordinary unary connective).
	exprExists
	// ExprInSubquery is `lhs [NOT] IN ( query_expr )` (spec/design/grammar.md §26) — membership of
	// lhs in the subquery's single output column (three-valued, like a literal IN).
	exprInSubquery
	// ExprQuantified is `lhs op ANY/SOME/ALL ( array )` (spec/design/array-functions.md §11) — a
	// quantified array comparison, the array spelling of IN. The three-valued fold over the array's
	// flattened elements reuses the IN-list membership semantics, generalized to all five comparison
	// operators and both quantifiers.
	exprQuantified
	// ExprQuantifiedSubquery is `lhs op ANY/SOME/ALL ( query_expr )` (array-functions.md §11.6) — the
	// SUBQUERY form of the quantified comparison, the subquery spelling of IN. Parallel to
	// ExprInSubquery: the body's single column (42601 if >1) folds through the SAME three-valued fold
	// as ExprQuantified. Uncorrelated folds to a constant-array Quantified; correlated re-executes
	// per outer row.
	exprQuantifiedSubquery
	// ExprRow is a `ROW(e1, e2, …)` composite constructor (spec/design/composite.md §1): RowItems
	// holds the field expressions, in order. A one-field ROW(x) is a one-field row; ROW() is the
	// zero-field row. The bare `(a, b)` form is deferred (0A000); only the keyword form parses.
	exprRow
	// ExprArray is an `ARRAY[e1, e2, …]` array constructor (spec/design/array.md §1): RowItems holds
	// the element expressions, in order (reusing the RowItems slot). `ARRAY[]` is the empty array.
	exprArray
	// ExprFieldAccess is field selection `(expr).field` (spec/design/composite.md §S4) — the value
	// of one named field of a composite Base. The parser produces this for a `.name` postfix on a
	// parenthesized / `ROW(…)` / cast / qualified-column base; a bare `a.b` stays
	// ExprQualifiedColumn and only falls back to field access at resolve when `a` is no relation
	// but a composite column (the ambiguity rule — table.column first, then column.field). Field
	// lookup is case-insensitive; an unknown field is 42703, a non-composite base 42809.
	exprFieldAccess
	// ExprFieldStar is whole-row expansion `(expr).*` (spec/design/composite.md §S4) — expands a
	// composite Base into one output column per field, in declaration order. Valid only in a
	// SELECT/RETURNING projection list (where `*` expands); in any scalar expression position it is
	// 0A000.
	exprFieldStar
	// ExprQualifiedStar is whole-relation expansion `t.*` (spec/design/grammar.md §15) — expands the
	// FROM relation labeled Qualifier into one output column per column, in catalog order. Like bare
	// `*` but for a single named relation, and (unlike bare `*`) MIXABLE with other select items
	// (`SELECT t.*, u.x`). Valid only in a SELECT/RETURNING projection list; in a scalar position it
	// is 42601. An unknown qualifier is 42P01. Qualifier holds the relation label; Base is unused
	// (distinct from the composite `(expr).*` ExprFieldStar — `t.*` names a relation, `(c).*` a value).
	exprQualifiedStar
	// ExprSubscript is array subscript `Base[..][..]` (spec/design/array.md §6) — one or more
	// bracketed specs (Subscripts) applied to an array Base. Each spec is an index `[i]` or a slice
	// `[m:n]` (with optionally-omitted bounds). All-index access reads a single 1-based element (the
	// element type); if any spec is a slice the access returns a sub-array (the array type), and a
	// scalar index i then means 1:i (PG). An out-of-bounds / NULL subscript yields NULL (PG, not an
	// error); a non-array base is 42804 at resolve.
	exprSubscript
	// ExprJsonExists is `JSON_EXISTS(ctx, path [behavior ON ERROR])` — the SQL/JSON existence
	// predicate (json-sql-functions.md §5, S2). The path is evaluated over the context item; a
	// non-empty sequence → true. The default `ON ERROR` behavior is `FALSE`. `PASSING` (vars) is
	// deferred.
	exprJsonExists
	// ExprJsonValue is `JSON_VALUE(ctx, path [RETURNING type] [ON EMPTY] [ON ERROR])` — extract a
	// single SCALAR item, coerced to the RETURNING type (default `text`). Empty → ON EMPTY (default
	// NULL); a non-scalar / >1 item / coercion failure → ON ERROR (default NULL). (json-sql-functions.md §5.)
	exprJsonValue
	// ExprJsonQuery is `JSON_QUERY(ctx, path [RETURNING type] [wrapper] [quotes] [ON EMPTY] [ON ERROR])`
	// — extract a JSON value (default `jsonb`). The wrapper controls array-wrapping; the quotes clause
	// controls scalar-string de-quoting. (json-sql-functions.md §5.)
	exprJsonQuery
)

// SubscriptSpec is one subscript spec inside an ExprSubscript (spec/design/array.md §6): an index
// `[i]` (IsSlice false, Index set) or a slice `[m:n]` (IsSlice true; Lower/Upper may be nil for an
// omitted bound `[:n]`/`[m:]`/`[:]`).
type subscriptSpec struct {
	IsSlice bool
	Index   *exprNode
	Lower   *exprNode
	Upper   *exprNode
}

// UnaryOp is a unary operator.
type unaryOp int

const (
	// OpNeg is arithmetic negation `-x`.
	opNeg unaryOp = iota
	// OpNot is logical negation `NOT x`.
	opNot
)

// BinaryOp is a binary operator (arithmetic, comparison, or logical).
type binaryOp int

const (
	// OpAdd is +.
	opAdd binaryOp = iota
	// OpSub is -.
	opSub
	// OpMul is *.
	opMul
	// OpDiv is /.
	opDiv
	// OpMod is %.
	opMod
	// OpEq is =.
	opEq
	// OpNe is <> (alias !=): the 3VL negation of OpEq, propagating NULL like OpEq.
	opNe
	// OpLt is <.
	opLt
	// OpGt is >.
	opGt
	// OpLe is <=.
	opLe
	// OpGe is >=.
	opGe
	// OpAnd is AND.
	opAnd
	// OpOr is OR.
	opOr
	// OpConcat is the `||` array concatenation operator (spec/design/array-functions.md §8):
	// array∥array (array_cat), array∥element (array_append), element∥array (array_prepend),
	// resolved polymorphically.
	opConcat
	// OpContains/OpContainedBy/OpOverlaps are the array containment/overlap operators `@>`/`<@`/`&&`
	// (spec/design/array-functions.md §10): each `anyarray <op> anyarray → boolean`, resolved
	// polymorphically. The range surface (spec/design/range-functions.md §3) reuses these three
	// (range operands route to the range axis) and adds the five positional/adjacency operators below.
	opContains
	opContainedBy
	opOverlaps
	// OpStrictlyLeft/OpStrictlyRight/OpNotExtendRight/OpNotExtendLeft/OpAdjacent are the range boolean
	// operators `<<`/`>>`/`&<`/`&>`/`-|-` (spec/design/range-functions.md §3, RF3): range-only,
	// `anyrange <op> anyrange → boolean`.
	opStrictlyLeft
	opStrictlyRight
	opNotExtendRight
	opNotExtendLeft
	opAdjacent
	// OpJsonGet/OpJsonGetText/OpJsonGetPath/OpJsonGetPathText are the jsonb accessor operators
	// `->`/`->>`/`#>`/`#>>` (spec/design/json-sql-functions.md §1, J4): `->` get field/element,
	// `->>` get as text, `#>` get at path, `#>>` get at path as text. The result type and the
	// field-vs-index split are decided at resolve from the operand types.
	opJsonGet
	opJsonGetText
	opJsonGetPath
	opJsonGetPathText
	// OpJsonHasKey/OpJsonHasAnyKey/OpJsonHasAllKeys are the jsonb key-existence operators
	// `?`/`?|`/`?&` (spec/design/json-sql-functions.md §1, J5): `?` a key exists, `?|` any key of a
	// text[] exists, `?&` all keys exist. `boolean` result.
	opJsonHasKey
	opJsonHasAnyKey
	opJsonHasAllKeys
	// OpJsonDeletePath is the jsonb delete-at-path operator `#-` (spec/design/json-sql-functions.md
	// §1, J6). (The `||` concat reuses OpConcat, and `-` delete reuses OpSub — both dispatched by
	// operand type.)
	opJsonDeletePath
	// OpJsonPathExists is the `@?` jsonpath-exists operator (`jsonb @? jsonpath` = `jsonb_path_exists`)
	// — jsonpath.md §6.
	opJsonPathExists
	// OpJsonPathMatch is the `@@` jsonpath-match operator (`jsonb @@ jsonpath` = `jsonb_path_match`)
	// — jsonpath.md §6.
	opJsonPathMatch
)

// Expr is a general expression, shared by the SELECT list, WHERE, and UPDATE ... SET.
// Kind selects which fields are meaningful. A comparison/logical/null-test node is
// boolean-valued; arithmetic and columns/integer-literals are integer-valued.
type exprNode struct {
	Kind        exprKind
	Column      string       // ExprColumn, ExprQualifiedColumn (the column name)
	Qualifier   string       // ExprQualifiedColumn (the relation label)
	Param       uint64       // ExprParam (the 1-based bind-parameter index)
	Literal     *literal     // ExprLiteral
	TypeLitName string       // ExprTypedLiteral (the named type, e.g. "integer", "interval")
	TypeLitText string       // ExprTypedLiteral (the literal's string, coerced to the type at resolve)
	Cast        *castExpr    // ExprCast
	Extract     *extractExpr // ExprExtract
	Collate     *collateExpr // ExprCollate
	Unary       *unaryExpr   // ExprUnary
	Binary      *binaryExpr
	IsNullOf    *isNullExpr     // ExprIsNull
	IsJsonOf    *isJsonExpr     // ExprIsJson
	JsonCtorOf  *jsonCtorExpr   // ExprJsonCtor
	JsonExists  *jsonExistsExpr // ExprJsonExists
	JsonValue   *jsonValueExpr  // ExprJsonValue
	JsonQuery   *jsonQueryExpr  // ExprJsonQuery
	IsDistinct  *isDistinctExpr // ExprIsDistinct
	FuncCall    *funcCallExpr   // ExprFuncCall
	In          *inExpr         // ExprIn
	Between     *betweenExpr    // ExprBetween
	Like        *likeExpr       // ExprLike
	Regex       *regexExpr      // ExprRegex
	Case        *caseExpr       // ExprCase
	Subquery    *queryExpr      // ExprScalarSubquery, ExprExists (the inner query)
	InSubquery  *inSubqueryExpr // ExprInSubquery
	Quantified  *quantifiedExpr // ExprQuantified

	QuantifiedSubquery *quantifiedSubqueryExpr // ExprQuantifiedSubquery
	RowItems           []exprNode              // ExprRow (the ROW(...) field expressions, in order)
	// Base is the operand of a field-selection postfix (ExprFieldAccess / ExprFieldStar): the
	// composite expression `(base).field` / `(base).*` selects from (spec/design/composite.md §S4).
	Base *exprNode
	// Field is the selected field name of an ExprFieldAccess (the `.field` part); lookup is
	// case-insensitive at resolve.
	Field string
	// Subscripts are the bracketed specs of an ExprSubscript (`Base[..][..]`) — one or more index /
	// slice specs (spec/design/array.md §6). Base holds the array operand.
	Subscripts []subscriptSpec
}

// InSubqueryExpr is `Lhs [NOT] IN ( Query )` (spec/design/grammar.md §26) — membership of Lhs in
// Query's single output column (three-valued, like a literal IN).
type inSubqueryExpr struct {
	Lhs     exprNode
	Query   queryExpr
	Negated bool
}

// QuantifiedExpr is `Lhs Op ANY/SOME/ALL ( Array )` — a quantified array comparison
// (spec/design/array-functions.md §11), the array spelling of IN. Op is a comparison
// (OpEq/OpLt/OpGt/OpLe/OpGe); All is true for ALL, false for ANY/SOME (SOME folds to ANY at
// parse). Array resolves to an array type; the three-valued fold over its flattened elements
// reuses the IN-list membership semantics, generalized to all five operators and both quantifiers.
type quantifiedExpr struct {
	Op    binaryOp
	All   bool
	Lhs   exprNode
	Array exprNode
}

// QuantifiedSubqueryExpr is `Lhs Op ANY/SOME/ALL ( Query )` — the SUBQUERY form of the quantified
// comparison (spec/design/array-functions.md §11.6), the subquery spelling of IN. Op/All as in
// QuantifiedExpr; Query's single column (42601 if >1) folds through the same three-valued fold.
type quantifiedSubqueryExpr struct {
	Op    binaryOp
	All   bool
	Lhs   exprNode
	Query queryExpr
}

// CastExpr is CAST(Inner AS TypeName). TypeMod is the optional numeric(p[,s]) modifier.
type castExpr struct {
	Inner    exprNode
	TypeName string
	TypeMod  *typeMod
}

// ExtractExpr is EXTRACT(Field FROM Source) (spec/design/timezones.md §9.2, grammar.md §50) — the
// datetime field special form. Field is the syntactic field name (identifier or string literal,
// lowercased at parse); Source is the datetime expression. Resolves to a numeric.
type extractExpr struct {
	Field  string
	Source exprNode
}

// CollateExpr is `Inner COLLATE "Collation"` — the postfix collation operator
// (spec/design/collation.md §1). It sets an EXPLICIT collation on a text expression for the
// surrounding comparison / ORDER BY; it binds at the postfix/typecast level (tighter than || and
// the comparisons — PG precedence). Collation is a quoted identifier (case-sensitive, e.g. "C",
// "en-US"). A non-text Inner is 42804, an unloaded name 42704, two different explicit collations
// in one comparison 42P21.
type collateExpr struct {
	Inner     exprNode
	Collation string
}

// UnaryExpr is Op applied to Operand.
type unaryExpr struct {
	Op      unaryOp
	Operand exprNode
}

// BinaryExpr is Lhs Op Rhs.
type binaryExpr struct {
	Op  binaryOp
	Lhs exprNode
	Rhs exprNode
}

// IsNullExpr is `Operand IS [NOT] NULL`.
type isNullExpr struct {
	Operand exprNode
	Negated bool
}

// JsonPredicateKind is the optional kind word of an IS JSON predicate
// (spec/design/json-sql-functions.md §5).
type jsonPredicateKind int

const (
	// JPKValue is `IS JSON` / `IS JSON VALUE` — any well-formed JSON.
	jPKValue jsonPredicateKind = iota
	// JPKScalar is `IS JSON SCALAR` — a JSON scalar (string/number/boolean/null), not an object/array.
	jPKScalar
	// JPKArray is `IS JSON ARRAY` — a JSON array.
	jPKArray
	// JPKObject is `IS JSON OBJECT` — a JSON object.
	jPKObject
)

// IsJsonExpr is `Operand IS [NOT] JSON [Kind] [(WITH|WITHOUT) UNIQUE [KEYS]]` — the SQL/JSON
// well-formedness predicate (spec/design/json-sql-functions.md §5). Negated carries the NOT keyword;
// Kind is the optional kind word (default JPKValue); UniqueKeys is true for `WITH UNIQUE [KEYS]`
// (the default `WITHOUT` is false). A non-string/json operand → 42804; a NULL operand → NULL.
type isJsonExpr struct {
	Operand    exprNode
	Negated    bool
	Kind       jsonPredicateKind
	UniqueKeys bool
}

// JsonCtorExpr is `JSON(Operand [(WITH|WITHOUT) UNIQUE [KEYS]])` — the SQL/JSON JSON() constructor
// (spec/design/json-sql-functions.md §5): validate a character string as JSON and return it verbatim
// as a `json` value. UniqueKeys is true for `WITH UNIQUE [KEYS]` (the default `WITHOUT` is false). A
// malformed string → 22P02; a duplicate object key under UniqueKeys → 22030; a non-text operand →
// 42804; a NULL operand → SQL NULL.
type jsonCtorExpr struct {
	Operand    exprNode
	UniqueKeys bool
}

// JsonOnBehavior is a constant `ON EMPTY` / `ON ERROR` behavior for the SQL/JSON query functions
// (json-sql-functions.md §5.2). `DEFAULT expr` is the deferred S3 follow-on (§5.3).
type jsonOnBehavior int

const (
	// JOBNull is `NULL` — substitute SQL NULL.
	jOBNull jsonOnBehavior = iota
	// JOBError is `ERROR` — raise the underlying SQL/JSON error.
	jOBError
	// JOBTrue / JOBFalse / JOBUnknown — only valid for JSON_EXISTS's `ON ERROR`.
	jOBTrue
	jOBFalse
	jOBUnknown
	// JOBEmptyArray / JOBEmptyObject — substitute an empty JSON array / object (JSON_QUERY).
	jOBEmptyArray
	jOBEmptyObject
)

// JsonWrapper is JSON_QUERY's array-wrapper mode (json-sql-functions.md §5.2).
type jsonWrapper int

const (
	// JWWithout is `WITHOUT [ARRAY] WRAPPER` (default) — the sequence must be a singleton.
	jWWithout jsonWrapper = iota
	// JWUnconditional is `WITH [UNCONDITIONAL] [ARRAY] WRAPPER` — always wrap the sequence in an array.
	jWUnconditional
	// JWConditional is `WITH CONDITIONAL [ARRAY] WRAPPER` — wrap only when the sequence is not a single item.
	jWConditional
)

// JsonExistsExpr is `JSON_EXISTS(Ctx, Path [behavior ON ERROR])` — the SQL/JSON existence predicate
// (json-sql-functions.md §5, S2). OnError is nil when no `ON ERROR` clause is present (default FALSE).
type jsonExistsExpr struct {
	Ctx     exprNode
	Path    exprNode
	OnError *jsonOnBehavior
}

// JsonValueExpr is `JSON_VALUE(Ctx, Path [RETURNING type] [ON EMPTY] [ON ERROR])` (json-sql-functions.md
// §5). Returning is nil for the default `text`; OnEmpty/OnError are nil when absent.
type jsonValueExpr struct {
	Ctx       exprNode
	Path      exprNode
	Returning *string
	OnEmpty   *jsonOnBehavior
	OnError   *jsonOnBehavior
}

// JsonQueryExpr is `JSON_QUERY(Ctx, Path [RETURNING type] [wrapper] [quotes] [ON EMPTY] [ON ERROR])`
// (json-sql-functions.md §5). Returning is nil for the default `jsonb`; KeepQuotes is true for `KEEP
// QUOTES` (default) / false for `OMIT QUOTES`; OnEmpty/OnError are nil when absent.
type jsonQueryExpr struct {
	Ctx        exprNode
	Path       exprNode
	Returning  *string
	Wrapper    jsonWrapper
	KeepQuotes bool
	OnEmpty    *jsonOnBehavior
	OnError    *jsonOnBehavior
}

// IsDistinctExpr is `Lhs IS [NOT] DISTINCT FROM Rhs` — NULL-safe equality. Negated
// carries the NOT keyword: Negated == true is `IS NOT DISTINCT FROM` (NULL-safe `=`),
// false is `IS DISTINCT FROM` (its negation). Always boolean-valued, never unknown
// (spec/design/functions.md §3).
type isDistinctExpr struct {
	Lhs     exprNode
	Rhs     exprNode
	Negated bool
}

// FuncCallExpr is a function call — the shared aggregate/scalar call syntax
// (spec/design/grammar.md §17). Name is the spelling as written, resolved case-insensitively:
// an aggregate (COUNT/SUM/MIN/MAX/AVG), a scalar function (abs/round, kind = "function",
// spec/design/functions.md §9), or 42883 (undefined_function). Star is the COUNT(*) row-count
// form (then Args is empty); otherwise Args is the comma-separated argument list — aggregates
// and abs take one, round one or two. Distinct carries a leading DISTINCT inside the parens
// (COUNT(DISTINCT x), aggregates.md §5). An aggregate in WHERE/ON or nested in another aggregate
// is 42803 (spec/design/aggregates.md); a scalar function is legal anywhere an expression is.
//
// ArgNames carries PostgreSQL named notation (name => value, grammar.md §17): nil ⇒ every
// argument positional (the common case); otherwise it is parallel to Args, with a non-nil
// *string for a named slot and nil for a positional one. The parser rejects a positional arg
// after a named one.
// Variadic is true when the final argument was prefixed with the VARIADIC keyword
// (num_nulls(VARIADIC arr), array-functions.md §12 / grammar.md §17): the array is passed
// directly to a variadic parameter rather than spreading individual arguments. false for every
// ordinary call (the all-positional/spread fast path).
type funcCallExpr struct {
	Name     string
	Args     []*exprNode
	ArgNames []*string
	Star     bool
	// Distinct is true when the argument was prefixed with DISTINCT (COUNT(DISTINCT x) —
	// aggregates.md §5): the aggregate folds only the distinct non-NULL argument values. Only an
	// aggregate accepts it — DISTINCT on a scalar function is 42809, on a window function 0A000,
	// and f(DISTINCT *) / f(DISTINCT) is a 42601 syntax error.
	Distinct bool
	// Filter is the FILTER (WHERE cond) condition when present (SUM(x) FILTER (WHERE y > 0) —
	// aggregates.md §11): the aggregate folds only the input rows for which cond is TRUE. nil for a
	// plain call. Only an aggregate accepts it — FILTER on a scalar function is 42809, on a window
	// function 0A000; an aggregate inside cond is 42803 and a non-boolean cond is 42804.
	Filter   *exprNode
	Variadic bool
	// Over is set when the call carries a trailing OVER (...) window clause (a WINDOW-function
	// call — spec/design/window.md). nil for an ordinary scalar/aggregate/SRF call. A window-only
	// function (row_number/…) with Over == nil is 42809; an aggregate with Over set is a window
	// aggregate (S3, deferred).
	Over *windowDef
	// OverName is the referenced named window when the call is `f(...) OVER name` (the WINDOW
	// clause — spec/design/window.md §5); "" for an inline `OVER (...)` or a non-window call. A
	// desugaring pass replaces it with the named definition (into Over) before resolution; exactly
	// one of Over/OverName is set on a window call.
	OverName string
	// WithinGroup is the WITHIN GROUP (ORDER BY …) order keys when the call is an ordered-set
	// aggregate (mode/percentile_cont/percentile_disc — spec/design/aggregates.md §13); nil for an
	// ordinary call. The parenthesized Args are the per-group direct argument (the percentile
	// fraction; empty for mode); these keys are the aggregated argument, the value sorted over.
	// Column-only, like the query ORDER BY (the parser keeps the whole list so the resolver can
	// reject a second key, 42883).
	WithinGroup []orderKey
}

// InExpr is `Lhs IN (List)` / `Lhs NOT IN (List)` — membership over a non-empty value list
// (spec/design/grammar.md §20). Desugared at resolve into the OR-chain PostgreSQL defines it
// as (`x IN (a,b)` is `x = a OR x = b`; NOT IN is its negation), inheriting the three-valued
// NULL semantics and per-element operand typing from `=`/OR/NOT. The parser guarantees List is
// non-empty (`IN ()` is 42601).
type inExpr struct {
	Lhs     exprNode
	List    []exprNode
	Negated bool
}

// BetweenExpr is `Lhs BETWEEN Lo AND Hi` / `Lhs NOT BETWEEN Lo AND Hi` — a range test
// (spec/design/grammar.md §21). Desugared at resolve into `Lhs >= Lo AND Lhs <= Hi` (NOT
// BETWEEN negates), inheriting the three-valued NULL semantics from the comparisons and the
// Kleene AND. The bounds parse at the additive level so the structural `AND` is not the
// logical connective.
type betweenExpr struct {
	Lhs     exprNode
	Lo      exprNode
	Hi      exprNode
	Negated bool
}

// LikeExpr is `Lhs LIKE Rhs` / `Lhs NOT LIKE Rhs` — a text pattern match (spec/design/grammar.md
// §22). `%` matches any run of characters, `_` one code point, with the default `\` escape. Both
// operands must be text; NULL propagates. A genuine operator (not desugared) with a hand-written
// matcher. Negated carries the NOT keyword; Insensitive carries ILIKE (case-insensitive matching,
// both sides simple-lowercased under the casing regime — collation.md §16).
type likeExpr struct {
	Lhs         exprNode
	Rhs         exprNode
	Negated     bool
	Insensitive bool
}

// RegexExpr is `Lhs ~ Rhs` / `~*` / `!~` / `!~*` — a regular-expression match (grammar.md §22b,
// regex.md). jed's own RE2-able flavor (not PostgreSQL-compatible), matched by a hand-written
// linear-time Pike VM. UNANCHORED (matches a substring). Both operands must be text; NULL
// propagates. Negated carries `!~`/`!~*`; Insensitive carries `~*`/`!~*` (case-insensitive, both
// sides simple-lowercased like ILIKE — collation.md §16).
type regexExpr struct {
	Lhs         exprNode
	Rhs         exprNode
	Negated     bool
	Insensitive bool
}

// CaseExpr is a CASE expression (spec/design/grammar.md §23). Searched form: Operand is nil and
// each When.Cond must be boolean. Simple form: Operand is non-nil and each branch matches when
// `Operand = When.Cond`. Whens has ≥1 entry. Els is the ELSE result, or nil for an implicit
// `ELSE NULL`. Lazily evaluated: the first TRUE branch wins; result-arm types unify.
type caseExpr struct {
	Operand *exprNode
	Whens   []caseWhen
	Els     *exprNode
}

// CaseWhen is one `WHEN cond THEN result` branch of a CaseExpr (Cond is the searched predicate,
// or the simple form's value compared for equality to the operand).
type caseWhen struct {
	Cond   exprNode
	Result exprNode
}

// OrderKey is one ORDER BY sort key, in one of three modes resolved at parse time
// (spec/design/grammar.md §10): an output-column ordinal (Ordinal != nil), a general expression
// (Expr != nil), or a column reference (Qualifier/Column, the fast path that keeps PK-scan elision).
// Plus a sort direction and a resolved NULL placement. NullsFirst is resolved at parse time — an
// explicit NULLS FIRST|LAST, else the direction default (Descending: ASC -> last, DESC -> first, the
// PostgreSQL model where NULL is the largest value) — and is applied independently of the
// Descending value flip.
type orderKey struct {
	// Ordinal is an output-column ordinal (`ORDER BY 1`): non-nil is the 1-based position of a
	// select-list item (resolved by position, the value validated as 42P10 if out of range —
	// grammar.md §10), and then Expr/Qualifier/Column are unused. An optional leading `-` is folded
	// in, so a negative position reaches 42P10, not a syntax error. Ordinals are parsed only in the
	// query / set-operation ORDER BY, never in WITHIN GROUP.
	Ordinal *int64
	// Expr is a general-expression key (`ORDER BY a + 1`): non-nil is the key expression, evaluated
	// per row and sorted by the computed value (grammar.md §10); Ordinal/Qualifier/Column are then
	// unused. The parser sets this only when the key is neither a bare ordinal nor a bare (optionally
	// COLLATE-wrapped) column reference, so a column key still takes the fast path below.
	Expr *exprNode
	// Qualifier is an optional relation qualifier (`ORDER BY t.a`); "" is a bare column. Used only by
	// a column-reference key (Ordinal and Expr both unset).
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

// WindowOrderKey is one window ORDER BY sort key (spec/design/window.md §3/§5.1). Unlike the query
// OrderKey (column references only), a window sort key is a general expression (`ORDER BY a + b`,
// `ORDER BY sum(x)` in a grouped query) — the deferred general-expression-key follow-on. A bare
// column is resolved to its row slot directly (unchanged); a compound expression is materialized
// into a synthetic window-key column before the window stage. Collation / Descending / NullsFirst
// carry the same meaning as OrderKey (the latter resolved at parse).
type windowOrderKey struct {
	Expr exprNode
	// Collation is an explicit `COLLATE "name"` on this key; "" means the key expression's (text)
	// collation. A COLLATE on a non-text key is 42804; an unknown name is 42704.
	Collation  string
	Descending bool
	NullsFirst bool
}

// WindowDef is a window definition — the body of an OVER (...) clause (spec/design/window.md §3).
// Carries an optional base-window name, PARTITION BY, ORDER BY, and a frame clause. Both Partition
// and Order are general expressions (`PARTITION BY a + b`, `ORDER BY a % 2`, `ORDER BY sum(x)` in a
// grouped query — spec/design/window.md §5.1); a bare column resolves to its row slot directly, a
// compound expression is materialized into a synthetic window-key column before the window stage.
//
// Base is an optional leading base-window name (`OVER (w ORDER BY …)`, `WINDOW w2 AS (w …)` — §5):
// the definition extends the named base, inheriting its PARTITION BY (and its ORDER BY if any) and
// supplying its own frame. A resolve-time pass (resolveWindowClause / desugarNamedWindows) merges
// the base in and clears Base to "", so every definition is inline (Base == "") at the window stage.
type windowDef struct {
	Base      string
	Partition []exprNode
	Order     []windowOrderKey
	Frame     *windowFrame
}

// WindowFrame is a window frame clause (spec/design/window.md §6).
type windowFrame struct {
	Mode    frameMode
	Start   frameBound
	End     frameBound
	Exclude frameExclusion
}

// FrameExclusion is the EXCLUDE clause (spec/design/window.md §6): which rows to drop from the
// computed [lo, hi) frame, per current row. FrameExcludeNoOthers (the default / no EXCLUDE) drops
// nothing.
type frameExclusion int

const (
	// FrameExcludeNoOthers drops nothing (EXCLUDE NO OTHERS / no clause).
	frameExcludeNoOthers frameExclusion = iota
	// FrameExcludeCurrentRow drops the current row.
	frameExcludeCurrentRow
	// FrameExcludeGroup drops the current row's whole peer group.
	frameExcludeGroup
	// FrameExcludeTies drops the current row's peers but not the row itself.
	frameExcludeTies
)

// FrameMode is the frame unit: ROWS, RANGE, or GROUPS. S4 supports ROWS only.
type frameMode int

const (
	// FrameRows is a physical-row frame (ROWS).
	frameRows frameMode = iota
	// FrameRange is a value-range frame (RANGE) — parsed, deferred 0A000.
	frameRange
	// FrameGroups is a peer-group frame (GROUPS) — parsed, deferred 0A000.
	frameGroups
)

// FrameBoundKind distinguishes the five frame-boundary forms.
type frameBoundKind int

const (
	// FrameUnboundedPreceding is UNBOUNDED PRECEDING.
	frameUnboundedPreceding frameBoundKind = iota
	// FramePreceding is `expr PRECEDING`; Offset carries the offset expression.
	framePreceding
	// FrameCurrentRow is CURRENT ROW.
	frameCurrentRow
	// FrameFollowing is `expr FOLLOWING`; Offset carries the offset expression.
	frameFollowing
	// FrameUnboundedFollowing is UNBOUNDED FOLLOWING.
	frameUnboundedFollowing
)

// FrameBound is one frame boundary. Offset carries the offset expression for FramePreceding /
// FrameFollowing (a non-negative integer in ROWS/GROUPS; a value offset in RANGE), nil otherwise.
type frameBound struct {
	Kind   frameBoundKind
	Offset exprNode
}

// LiteralKind distinguishes the literal forms.
type literalKind int

const (
	// LiteralNull is the NULL literal.
	literalNull literalKind = iota
	// LiteralInt is an integer literal.
	literalInt
	// LiteralBool is a boolean literal (TRUE / FALSE).
	literalBool
	// LiteralText is a single-quoted text literal (Str holds the decoded content). Its
	// type is always text (collation C); it does not adapt to context like an integer
	// literal does (spec/design/types.md §11).
	literalText
	// LiteralDecimal is a decimal literal (Dec holds the constructed value, sign folded). An
	// untyped decimal constant that adapts to context; caps are checked at resolve
	// (spec/design/grammar.md §14, decimal.md §6).
	literalDecimal
)

// Literal is a literal value as written in SQL. A bare integer literal is an *untyped
// constant* that adapts to its context — the target column on INSERT/UPDATE, a sibling
// operand, the compared column in a WHERE predicate — and traps 22003 if it does not
// fit; with no context it defaults to i64 (spec/design/types.md §6). A boolean
// literal is expression-only this slice (it cannot be stored).
type literal struct {
	Kind literalKind
	Int  int64
	Bool bool
	Str  string  // LiteralText
	Dec  Decimal // LiteralDecimal
}
