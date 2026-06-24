//! Abstract syntax for the step-1 SQL surface. Boring, explicit shapes
//! (CLAUDE.md §10); the hand-written parser produces these.

use crate::decimal::Decimal;

/// A parsed top-level statement.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Statement {
    CreateTable(CreateTable),
    DropTable(DropTable),
    CreateIndex(CreateIndex),
    DropIndex(DropIndex),
    CreateType(CreateType),
    DropType(DropType),
    CreateSequence(CreateSequence),
    AlterSequence(AlterSequence),
    DropSequence(DropSequence),
    Insert(Insert),
    Select(Select),
    /// A set operation (`UNION`/`INTERSECT`/`EXCEPT`) combining two query expressions
    /// (spec/design/grammar.md §25). A lone `SELECT` stays `Statement::Select` — this variant
    /// appears only when at least one set operator is present, so the plain-query path and the
    /// host API are untouched.
    SetOp(SetOp),
    /// A query prefixed by a `WITH` clause defining one or more common table expressions
    /// (spec/design/cte.md). Appears only when a `WITH` is present; a plain query stays
    /// `Select`/`SetOp`, so the host API and the no-CTE paths are untouched.
    With(WithQuery),
    Update(Update),
    Delete(Delete),
    /// `BEGIN [TRANSACTION|WORK] [READ ONLY|READ WRITE]` / `START TRANSACTION [...]` — open an
    /// explicit transaction block (spec/design/grammar.md §27). `writable` is the *requested*
    /// access mode: `Some(true)` READ WRITE, `Some(false)` READ ONLY, `None` unspecified —
    /// which defaults to READ WRITE on a normal handle and READ ONLY on a read-only handle
    /// (api.md §2.1; a write inside a READ ONLY block → 25006). A nested `BEGIN` is 25001
    /// (transactions.md §4.2).
    Begin {
        writable: Option<bool>,
    },
    /// `COMMIT [TRANSACTION|WORK]` / `END [...]` — publish the open block durably and return to
    /// autocommit; a `COMMIT` with no open block is a no-op success (transactions.md §4.2).
    Commit,
    /// `ROLLBACK [TRANSACTION|WORK]` — discard the open block's working set and return to
    /// autocommit; a `ROLLBACK` with no open block is a no-op success (transactions.md §4.2).
    Rollback,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CreateTable {
    pub name: String,
    /// Whether `TEMP` / `TEMPORARY` preceded `TABLE` — a temporary table (spec/design/temp-tables.md).
    /// A temp table makes ZERO writes to the database file (it lives outside the serialized
    /// `Snapshot`) and is dropped at session / database close. Its DDL is gated by `allow_temp_ddl`
    /// (session-local) or `allow_shared_temp_ddl` (shared) rather than `allow_ddl` (temp-tables.md §5).
    /// `shared` implies `temp` (a `SHARED` table is always temporary).
    pub temp: bool,
    /// Whether `SHARED` preceded `TEMP`/`TEMPORARY` — a DATABASE-WIDE shared temporary table
    /// (spec/design/temp-tables.md §4): one set of rows visible to and writable by every session of
    /// the open `Database`, but still never written to the file. `shared = true` always has `temp =
    /// true` (the parser rejects `SHARED` not followed by `TEMP`/`TEMPORARY` as 42601). When `false`
    /// (and `temp = true`) the table is session-local (private to the creating session).
    pub shared: bool,
    pub columns: Vec<ColumnDef>,
    /// The table-level `PRIMARY KEY (a, b, …)` constraints, each a list of member column
    /// names in key order (spec/design/grammar.md §28). The parser collects every one it
    /// sees; CREATE TABLE's execution resolves them (42703/42701) and rejects more than one
    /// primary key across both forms (42P16) — spec/design/constraints.md §3.
    pub table_pks: Vec<Vec<String>>,
    /// Every `[CONSTRAINT name] CHECK ( expr )` of the statement — column-level and
    /// table-level forms are semantically identical, so both collect here, in **textual
    /// definition order** (it drives validation and naming — spec/design/constraints.md §4).
    /// CREATE TABLE's execution validates each (0A000/42803/42P02/42703/42804) and names
    /// the unnamed ones (42710 on a collision).
    pub checks: Vec<CheckDef>,
    /// Every `[CONSTRAINT name] UNIQUE [(cols)]` of the statement — the column-level form
    /// collects as a one-member list — in **textual definition order** (it drives member
    /// resolution, the dedup/PK fold, and naming — spec/design/constraints.md §5). Each
    /// survivor becomes a unique secondary index (spec/design/indexes.md §8).
    pub uniques: Vec<UniqueDef>,
    /// Every `FOREIGN KEY (cols) REFERENCES …` of the statement — the column-level
    /// `REFERENCES` form collects as a one-member list — in **textual definition order** (it
    /// drives resolution and naming — spec/design/constraints.md §6). CREATE TABLE's execution
    /// resolves each (42703/42701/42P01/42830/42804), rejects unsupported actions (0A000), and
    /// names the unnamed ones (42710).
    pub foreign_keys: Vec<ForeignKeyDef>,
}

/// A referential action for `ON DELETE` / `ON UPDATE` (spec/design/constraints.md §6.6). Only
/// `NoAction` (the default) and `Restrict` are supported — identical in jed (no deferrable
/// constraints); the write-actions parse but are rejected `0A000` at CREATE TABLE.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RefAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

/// One parsed `FOREIGN KEY` / `REFERENCES` constraint (spec/design/grammar.md §43): the optional
/// explicit `CONSTRAINT` name, the local (referencing) column names in list order, the referenced
/// (parent) table name, the optional referenced column names (`None` = the parent's primary key),
/// and the `ON DELETE` / `ON UPDATE` actions. Execution resolves it
/// (42703/42701/42P01/42830/42804) and names the unnamed ones (42710) — spec/design/constraints.md
/// §6.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ForeignKeyDef {
    pub name: Option<String>,
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Option<Vec<String>>,
    pub on_delete: RefAction,
    pub on_update: RefAction,
}

/// One parsed `UNIQUE` constraint (spec/design/grammar.md §31): the optional explicit
/// `CONSTRAINT` name (it names the backing index) and the member column names in list
/// order. Execution resolves the members (42703/42701/0A000) and names the index
/// (42P07/42710) — spec/design/constraints.md §5.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct UniqueDef {
    pub name: Option<String>,
    pub columns: Vec<String>,
}

/// One parsed `CHECK` constraint (spec/design/grammar.md §29): the optional explicit
/// `CONSTRAINT` name, the expression, and the expression's persisted text — the source
/// token sequence between the parentheses re-rendered per the closed table in
/// spec/fileformat/format.md "Check-expression text".
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CheckDef {
    pub name: Option<String>,
    pub expr: Expr,
    pub text: String,
}

/// A parsed `DEFAULT <expr>` column constraint (spec/design/constraints.md §2): the default
/// expression and its persisted text (the source token sequence re-rendered per the closed
/// table in spec/fileformat/format.md "Check-expression text", as a `CHECK` is). Execution
/// classifies it: a bare `Expr::Literal` is a constant (pre-evaluated at CREATE TABLE), any
/// other expression is stored as text and evaluated per row at INSERT.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DefaultDef {
    pub expr: Expr,
    pub text: String,
}

/// `DROP TABLE <name>`. Removes a table — its definition and all its rows — from the
/// catalog. Dropping a table that does not exist is an error (42P01); there is no
/// `IF EXISTS` this slice. Single table only; no `CASCADE` / `RESTRICT` (no dependent
/// objects exist yet). See spec/design/grammar.md §13.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DropTable {
    pub name: String,
}

/// `CREATE [UNIQUE] INDEX [name] ON <table> ( col [, col]* )` — a secondary index
/// (spec/design/indexes.md, grammar.md §30). `name: None` is the unnamed form; the
/// executor derives PostgreSQL's auto-name. Key columns are bare names (no expression /
/// ordered / partial keys this slice); a column may repeat (PG allows it). Execution
/// validates in PG's order: table 42P01, columns 42703/0A000, name collision 42P07. A
/// `unique` index additionally verifies the existing rows at build (23505) and enforces
/// uniqueness thereafter (spec/design/indexes.md §8).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CreateIndex {
    pub name: Option<String>,
    pub table: String,
    pub columns: Vec<String>,
    pub unique: bool,
    /// The `USING <method>` access method as written, or `None` for the default ordered B-tree.
    /// Resolved at execution: `None`/`btree` → B-tree, `gin` → GIN, else 42704 (gin.md §3).
    pub using: Option<String>,
}

/// `DROP INDEX <name>` — remove one secondary index (spec/design/indexes.md §2).
/// Missing → 42704; a table's name → 42809.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DropIndex {
    pub name: String,
}

/// `CREATE TYPE <name> AS ( field type [NOT NULL] [, …] )` — a user-defined composite (row)
/// type (spec/design/composite.md, grammar.md). Execution resolves each field's type (built-in
/// scalar or a previously-defined composite — 42704 if unknown), rejects a duplicate type name
/// (42710), and registers it in the catalog. Named composites only this slice; anonymous
/// `record` is not supported.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CreateType {
    pub name: String,
    pub fields: Vec<TypeFieldDef>,
}

/// One field of a `CREATE TYPE` definition: its name, its type as written (a built-in scalar
/// alias or a composite type name), an optional `numeric(p,s)` modifier, and an explicit
/// `NOT NULL`. Resolved at execution (mirrors `ColumnDef`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TypeFieldDef {
    pub name: String,
    pub type_name: String,
    pub type_mod: Option<TypeMod>,
    pub not_null: bool,
}

/// `DROP TYPE [IF EXISTS] <name> [RESTRICT]` — remove a composite type (spec/design/composite.md
/// §7). RESTRICT (the default and only behavior this slice) fails with 2BP01 if a table column
/// or another composite type still references it; `CASCADE` is `0A000`. A missing type without
/// `IF EXISTS` is 42704.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DropType {
    pub name: String,
    pub if_exists: bool,
}

/// The parsed, order-free sequence options shared by `CREATE SEQUENCE` and an IDENTITY column's
/// optional `( seq_options )` (spec/design/sequences.md §13). Each `None` means "use the default"
/// (resolved at execution against the INCREMENT sign); execution validates the set (22023).
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct SeqOptions {
    /// The `AS <type>` value type as written (the raw type name, e.g. `"smallint"` / `"int4"`),
    /// resolved to a `SeqDataType` at execution (spec/design/sequences.md §14); `None` = `bigint`
    /// default. A non-integer type is `22023`. Inside an IDENTITY column's options a set `data_type`
    /// is `42601` (the column type fixes it).
    pub data_type: Option<String>,
    pub increment: Option<i64>,
    /// `Some(Some(v))` = MINVALUE v; `Some(None)` = NO MINVALUE (the type default); `None` = unset.
    pub min_value: Option<Option<i64>>,
    pub max_value: Option<Option<i64>>,
    pub start: Option<i64>,
    pub cache: Option<i64>,
    pub cycle: Option<bool>,
}

/// `CREATE SEQUENCE [IF NOT EXISTS] <name> [options]` — a named, persisted i64 generator
/// (spec/design/sequences.md). Execution validates the option set (22023), rejects a
/// relation-namespace collision (42P07 unless `if_not_exists`), and registers the sequence.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CreateSequence {
    pub name: String,
    pub if_not_exists: bool,
    pub options: SeqOptions,
}

/// A column's `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [( seq_options )]` constraint
/// (spec/design/sequences.md §13). `always` distinguishes ALWAYS (true) from BY DEFAULT (false);
/// `options` tunes the auto-created owned sequence (defaults to the standard ascending i64).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct IdentitySpec {
    pub always: bool,
    pub options: SeqOptions,
}

/// `DROP SEQUENCE [IF EXISTS] <name> [, …] [RESTRICT]` — remove one or more sequences
/// (spec/design/sequences.md §1). A missing sequence without `IF EXISTS` is 42P01; `CASCADE` is
/// `0A000` (RESTRICT is the default and only mode this slice).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct DropSequence {
    pub names: Vec<String>,
    pub if_exists: bool,
}

/// `ALTER SEQUENCE [IF EXISTS] <name> <action>` (spec/design/sequences.md §4/§15). A missing
/// sequence without `IF EXISTS` is 42P01. The two action forms are in `AlterSeqAction`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AlterSequence {
    pub name: String,
    pub if_exists: bool,
    pub action: AlterSeqAction,
}

/// The two `ALTER SEQUENCE` action forms (spec/design/sequences.md §15).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AlterSeqAction {
    /// The definition-changing option set: the same order-free `CREATE` options (minus `AS`, which
    /// is 0A000 at execution) plus an interleavable `RESTART`. Only the written options change; the
    /// counter is preserved unless `restart` is given. `restart` is `None` (no `RESTART`),
    /// `Some(None)` (bare `RESTART` → the stored `START`), or `Some(Some(n))` (`RESTART WITH n`).
    /// The parser requires ≥ 1 action (a bare `ALTER SEQUENCE s` is 42601).
    SetOptions {
        options: SeqOptions,
        restart: Option<Option<i64>>,
    },
    /// `RENAME TO <new_name>` — move the catalog key; an owned sequence's owning-column `nextval`
    /// default is rewritten (§15.3). A collision with an existing relation is 42P07.
    Rename(String),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ColumnDef {
    pub name: String,
    /// The type name as written (canonical or alias); resolved during analysis.
    pub type_name: String,
    /// An optional parenthesized type modifier, `numeric(p[,s])` — the first parameterized
    /// type. Meaningful only for decimal; validated at resolve (spec/design/grammar.md §14).
    pub type_mod: Option<TypeMod>,
    pub primary_key: bool,
    /// An explicit `NOT NULL` column constraint. A PRIMARY KEY column is implicitly NOT NULL
    /// regardless of this flag; the executor ORs the two (spec/design/constraints.md).
    pub not_null: bool,
    /// An optional `DEFAULT <expr>` — the value for this column when a row omits it (or uses
    /// the `DEFAULT` keyword). A constant literal is pre-evaluated at CREATE TABLE; any other
    /// expression is evaluated per row at INSERT (spec/design/constraints.md §2).
    pub default: Option<DefaultDef>,
    /// An optional `GENERATED { ALWAYS | BY DEFAULT } AS IDENTITY [( opts )]` constraint
    /// (spec/design/sequences.md §13). Desugars like `serial` (an owned sequence + a `nextval`
    /// default + NOT NULL) plus the persisted ALWAYS/BY DEFAULT distinction.
    pub identity: Option<IdentitySpec>,
    /// An optional `COLLATE "name"` column modifier (spec/design/collation.md §1) — a quoted,
    /// case-sensitive collation name. Only valid on a `text` column (else 42804); the name must be a
    /// loaded collation or `"C"` (else 42704). Absent ⇒ the column inherits the per-database default
    /// collation. The effective collation is frozen into the column at CREATE TABLE.
    pub collation: Option<String>,
}

/// A parsed type modifier: a precision and an optional scale, as written
/// (`numeric(p)` → `scale = None`, `numeric(p,s)` → `scale = Some(s)`). The values are the
/// raw lexed magnitudes; range validation (1..=1000, 0..=p; else 22023) is at resolve.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TypeMod {
    pub precision: u64,
    pub scale: Option<u64>,
}

/// `INSERT INTO <table> [(col, ..)] ( VALUES (..)[, (..)]* | <select> )`. The rows come from
/// either a `VALUES` list (each value a literal or the `DEFAULT` keyword) or a `SELECT`
/// (spec/design/grammar.md §24). An INSERT is two-phase / all-or-nothing — every row is
/// validated before any is stored (spec/design/grammar.md §12).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Insert {
    pub table: String,
    /// An optional explicit column list (`INSERT INTO t (a, c) VALUES ...` / `... SELECT ...`).
    /// `None` is the positional form — every column, in declaration order. Names resolve at
    /// execution time (unknown → 42703, duplicate → 42701); an unlisted column takes its default
    /// else NULL (spec/design/constraints.md §2).
    pub columns: Option<Vec<String>>,
    /// The optional `OVERRIDING { SYSTEM | USER } VALUE` clause (spec/design/sequences.md §13),
    /// governing IDENTITY columns. `None` is the default (no override).
    pub overriding: Option<Overriding>,
    /// Where the rows come from: a `VALUES` list or a `SELECT`.
    pub source: InsertSource,
    /// The optional `ON CONFLICT` clause (UPSERT — spec/design/upsert.md), between the source
    /// and `RETURNING`. `None` = no clause (a conflict traps 23505 as usual).
    pub on_conflict: Option<OnConflict>,
    /// The optional terminal `RETURNING` clause (spec/design/grammar.md §32): project each
    /// stored row, turning the statement into a query result. `None` = no clause.
    pub returning: Option<SelectItems>,
}

/// The `ON CONFLICT [target] action` clause (spec/design/upsert.md §1).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OnConflict {
    /// The optional conflict target (the arbiter). `None` is legal only with `DoNothing`
    /// (any uniqueness conflict is then skipped); `DoUpdate` with `None` is 42601.
    pub target: Option<ConflictTarget>,
    pub action: ConflictAction,
}

/// The arbiter constraint named by an `ON CONFLICT` target (spec/design/upsert.md §2).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConflictTarget {
    /// `( col [, ...] )` — index inference: matched as a column SET against a unique index /
    /// the primary key (order-independent; no match → 42P10).
    Columns(Vec<String>),
    /// `ON CONSTRAINT name` — a unique-index name, or the synthesized `<table>_pkey` (miss → 42704).
    Constraint(String),
}

/// The action an `ON CONFLICT` takes on a conflicting row (spec/design/upsert.md §1).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ConflictAction {
    /// `DO NOTHING` — skip the offending row.
    DoNothing,
    /// `DO UPDATE SET … [WHERE …]` — update the existing conflicting row. The `excluded`
    /// pseudo-relation (resolved in the executor) names the proposed row.
    DoUpdate {
        assignments: Vec<Assignment>,
        filter: Option<Expr>,
    },
}

/// The INSERT `OVERRIDING { SYSTEM | USER } VALUE` clause (spec/design/sequences.md §13): `System`
/// lets an explicit value land in a `GENERATED ALWAYS` identity column; `User` discards a supplied
/// value for any identity column and uses its sequence instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Overriding {
    System,
    User,
}

/// The source of an INSERT's rows (spec/design/grammar.md §24): a literal `VALUES` list, or a
/// query whose result rows are inserted.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InsertSource {
    /// `VALUES (..)[, (..)]*` — one or more rows, in statement order; each inner vec is one
    /// row's values in the order of `columns` (or column order when `columns` is `None`).
    /// Always non-empty.
    Values(Vec<Vec<InsertValue>>),
    /// `SELECT ...` — the rows the query produces, in its output order. Boxed to keep `Insert`
    /// (and so `Statement`) small, since the SELECT source is the rarer form.
    Select(Box<Select>),
}

/// One value slot in an INSERT `VALUES` row: a literal, a bind parameter (`$N`, bound at
/// execute time — spec/design/api.md §5), or the `DEFAULT` keyword — which substitutes the
/// target column's declared default (or NULL if it has none). The `DEFAULT` keyword is not
/// reserved (spec/design/grammar.md §3). See spec/design/constraints.md §2.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum InsertValue {
    Lit(Literal),
    /// A bind parameter `$N` (1-based); typed against the target column at resolve.
    Param(u32),
    Default,
    /// A `ROW(…)` constructor in a VALUES slot (spec/design/composite.md §1) — a composite value
    /// for a composite target column. Fields are themselves `InsertValue`s (a literal, a `$N`, or a
    /// nested `ROW(…)`); `DEFAULT` is not a valid field (only a top-level slot takes a default).
    Row(Vec<InsertValue>),
    /// An `ARRAY[…]` constructor in a VALUES slot (spec/design/array.md §1) — an array value for an
    /// array target column. Elements are themselves `InsertValue`s (a literal or a `$N`).
    Array(Vec<InsertValue>),
}

/// `UPDATE <table> SET <col> = <expr> [, ...] [WHERE <expr>]`. Each assignment's
/// right-hand side is evaluated against the *pre-update* row (so `SET a = b, b = a`
/// swaps). Assigning a PRIMARY KEY column re-keys the row — the storage key is recomputed
/// and the row moves (see the executor). The WHERE expression must resolve to boolean.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Update {
    pub table: String,
    pub assignments: Vec<Assignment>,
    pub filter: Option<Expr>,
    /// The optional terminal `RETURNING` clause (spec/design/grammar.md §32): project each
    /// matched row's NEW (post-assignment) values.
    pub returning: Option<SelectItems>,
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
    /// The optional terminal `RETURNING` clause (spec/design/grammar.md §32): project each
    /// deleted row's OLD values.
    pub returning: Option<SelectItems>,
}

/// A table reference in a FROM clause: a table name with an optional alias (`orders o`
/// or `orders AS o`). The alias, or the table name when there is none, is the relation's
/// **label** — it qualifies columns (`o.col`) and must be distinct within one query
/// (a self-join needs aliases; a duplicate label is `42712`). See spec/design/grammar.md §15.
///
/// When `args` is `Some`, the reference is instead a **set-returning function** call used as a
/// row source (`generate_series(1, 5)`): `name` is the function name and `args` its argument
/// expressions. The label is then the alias, or the function name when there is none
/// (spec/design/grammar.md §35). `None` = an ordinary base table.
///
/// A `subquery` of `Some(body)` instead marks a **derived table** — a parenthesized subquery used
/// as a relation, `FROM (SELECT …) AS t` (spec/design/grammar.md §42). The alias is then mandatory
/// (the parser enforces 42601), so `name` and `alias` both carry it and `args` is `None`;
/// `column_aliases` is the optional column-rename list (`AS t (a, b)`). A derived table is
/// mechanically an anonymous, always-inlined single-reference CTE — the planner reuses the CTE
/// synthetic-relation seam.
///
/// `values` carries a **VALUES-body** derived table — `FROM (VALUES (e11,…),(e21,…)) AS v(c1,…)`
/// (spec/design/grammar.md §42): a parenthesized `VALUES` list used as a relation, a computed
/// relation of literal rows. It is the FROM-position alternative body to `subquery` (the two are
/// mutually exclusive — at most one is `Some` on a derived table). Each value is a general
/// constant expression (resolved `parent = None`, non-`LATERAL` unless this `TableRef` is marked
/// `lateral`); the rows share arity and the columns' types unify across rows like a set operation.
/// The outer `Vec` is the rows, each inner `Vec` one row's values, left to right.
///
/// `lateral` is set when the FROM item is preceded by the `LATERAL` keyword (spec/design/grammar.md
/// §44): the derived-table body / SRF arguments may then reference columns of the FROM relations
/// that appear BEFORE this one (a dependent / correlated join). It is meaningful only for a derived
/// table or a table function; a table function is *implicitly* lateral, so the planner correlates an
/// SRF's args to the earlier siblings whether or not this flag is set.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct TableRef {
    pub name: String,
    pub alias: Option<String>,
    pub args: Option<Vec<Expr>>,
    pub subquery: Option<Box<QueryExpr>>,
    pub values: Option<Vec<Vec<Expr>>>,
    pub column_aliases: Option<Vec<String>>,
    /// A FROM-clause **column-definition list** `AS t(col type, …)` (C0, json-table.md §1): the typed
    /// columns a record-returning function (`json[b]_to_record(set)`) declares. Mutually exclusive
    /// with `column_aliases` (a rename-only list). `None` for an ordinary table / SRF.
    pub column_defs: Option<Vec<TypeFieldDef>>,
    pub lateral: bool,
}

/// The kind of a join. `Inner` and `Cross` execute this slice; the `Left`/`Right`/`Full`
/// outer kinds parse and are carried in the AST but executing one is a documented `0A000`
/// narrowing (the OUTER family is a fast-follow — spec/design/grammar.md §15).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JoinKind {
    Inner,
    Cross,
    Left,
    Right,
    Full,
}

/// One `JOIN` step in the left-deep FROM chain: the join kind, the right-hand table
/// reference, and the optional `ON` predicate (`None` for `CROSS JOIN`; `Some(expr)` for
/// the `INNER`/outer kinds, which require an `ON`). See spec/design/grammar.md §15.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JoinClause {
    pub kind: JoinKind,
    pub table: TableRef,
    pub on: Option<Expr>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Select {
    /// `SELECT DISTINCT` — deduplicate the projected output rows (NULL-safe), applied
    /// after ORDER BY and before LIMIT/OFFSET (spec/design/grammar.md §11).
    pub distinct: bool,
    /// Projected expressions, or `*` for all (`SelectItems::All`).
    pub items: SelectItems,
    /// The first table reference of the FROM clause, or `None` for a FROM-less SELECT —
    /// the select list evaluates over one virtual zero-column row (spec/design/grammar.md §34).
    pub from: Option<TableRef>,
    /// Zero or more left-deep JOINs after `from` (empty = a single-table SELECT; always
    /// empty when `from` is `None` — joins exist only inside a FROM clause).
    /// spec/design/grammar.md §15.
    pub joins: Vec<JoinClause>,
    /// The WHERE expression (must resolve to boolean), if any.
    pub filter: Option<Expr>,
    /// GROUP BY grouping terms — `GroupItem::Set` for plain keys (`GROUP BY a, b` →
    /// `[Set([a]), Set([b])]`) plus the `ROLLUP`/`CUBE`/`GROUPING SETS` forms that expand to
    /// *multiple* grouping sets (spec/design/aggregates.md §12). Empty means no GROUP BY. Every
    /// grouping column is a bare/qualified `Column` (the parser restricts each to `column_ref`).
    pub group_by: Vec<GroupItem>,
    /// The HAVING predicate (a boolean filter over the grouped rows), if any. May reference
    /// aggregates and grouping keys; evaluated after aggregation, before ORDER BY. HAVING makes
    /// a query an aggregate query even with no GROUP BY (spec/design/grammar.md §19).
    pub having: Option<Expr>,
    /// ORDER BY sort keys, applied left to right; empty means no ORDER BY
    /// (spec/design/grammar.md §10).
    pub order_by: Vec<OrderKey>,
    /// `LIMIT n` — cap the result at `n` rows (a non-negative count). Applied after
    /// ORDER BY, before projection (spec/design/grammar.md §9).
    pub limit: Option<i64>,
    /// `OFFSET m` — skip the first `m` rows of the result (a non-negative count).
    pub offset: Option<i64>,
    /// Named windows from a `WINDOW name AS (definition)` clause (spec/design/window.md §5,
    /// grammar.ebnf `window_clause`), referenced by `OVER name`. Empty when absent. Resolved by a
    /// desugaring pass that rewrites each `OVER name` to its definition before resolution.
    pub windows: Vec<(String, WindowDef)>,
}

/// A query expression — the operand of a set operation (spec/design/grammar.md §25). Either a
/// single `SELECT` core or a nested set operation, so chains like `a UNION b INTERSECT c` form a
/// tree. Boxed at each arm to keep the recursive type sized.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum QueryExpr {
    Select(Box<Select>),
    SetOp(Box<SetOp>),
    /// A nested `WITH` clause prefixing a query expression, in a subquery / derived-table / CTE-body
    /// position (spec/design/cte.md §7) — as opposed to the top-level [`WithQuery`] (which may prefix
    /// a data-modifying primary). The CTEs are visible only within this node's own body (and to each
    /// other, forward-only); the enclosing statement's CTE bindings are NOT inherited — a documented
    /// narrowing (cte.md §7). Boxed to keep the recursive `QueryExpr` type sized.
    With(Box<WithExpr>),
}

/// A nested `WITH … query_expr` (spec/design/cte.md §7): the CTE list `ctes` (forward-only
/// visibility; self-referencing when `recursive`) prefixing the inner query expression `body`. A
/// data-modifying CTE here is rejected at planning (`0A000` — PostgreSQL restricts a DML-`WITH` to
/// the statement top level). Built by the parser wherever a parenthesized query expression is
/// expected; planned into a [`crate::executor`] `QueryPlan::With` that establishes its own CTE
/// scope.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WithExpr {
    pub ctes: Vec<Cte>,
    pub recursive: bool,
    pub body: Box<QueryExpr>,
}

/// The three set operators (spec/design/grammar.md §25).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SetOpKind {
    Union,
    Intersect,
    Except,
}

/// A set operation combining two query expressions (spec/design/grammar.md §25). `all` is the
/// `ALL` (multiset) flag — `false` is the deduplicating default. The optional trailing
/// `ORDER BY` / `LIMIT` / `OFFSET` apply to the WHOLE combined result and live on the OUTERMOST
/// node only (an operand carries none — a deferred narrowing); `order_by` keys resolve against
/// the output column names (the left operand's). Precedence is handled by the parser:
/// `INTERSECT` binds tighter than `UNION`/`EXCEPT`, which are left-associative.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SetOp {
    pub op: SetOpKind,
    pub all: bool,
    pub lhs: QueryExpr,
    pub rhs: QueryExpr,
    /// Trailing ORDER BY over the combined result (empty = none); keys resolve by output name.
    pub order_by: Vec<OrderKey>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
}

/// The body of a CTE, or the `WITH`-prefixed primary statement (spec/design/writable-cte.md): an
/// ordinary query expression, or a **data-modifying** statement (a writable CTE). The
/// data-modifying variants are boxed to keep `CteBody` (and so `WithQuery` / `Statement`) small.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CteBody {
    Query(QueryExpr),
    Insert(Box<Insert>),
    Update(Box<Update>),
    Delete(Box<Delete>),
}

impl CteBody {
    /// The query expression, if this body is a plain query — `None` for a data-modifying body.
    /// Used by the recursive-CTE analysis (only a query body can be a recursive `UNION`) and the
    /// pure-query `WITH` path.
    pub fn as_query(&self) -> Option<&QueryExpr> {
        match self {
            CteBody::Query(q) => Some(q),
            _ => None,
        }
    }

    /// Whether this body is a data-modifying statement (an `INSERT`/`UPDATE`/`DELETE`).
    pub fn is_data_modifying(&self) -> bool {
        !matches!(self, CteBody::Query(_))
    }
}

/// One common table expression in a `WITH` list (spec/design/cte.md). A named, statement-local
/// relation backed by a query or (spec/design/writable-cte.md) a data-modifying statement.
/// `columns` is the optional column-rename list (renames the body's output columns left to right;
/// a count mismatch is 42P10). `materialized` is the explicit evaluation hint: `Some(true)` =
/// `MATERIALIZED`, `Some(false)` = `NOT MATERIALIZED`, `None` = PostgreSQL's default (inline a
/// single-reference CTE, materialize a multi-reference one — cost.md §3; a data-modifying CTE is
/// always materialized, the hint inert). The body is a `cte_body`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Cte {
    pub name: String,
    pub columns: Option<Vec<String>>,
    pub materialized: Option<bool>,
    pub body: CteBody,
}

/// A top-level query prefixed by a `WITH` clause (spec/design/cte.md). `ctes` is the non-empty
/// list of common table expressions (each visible to later CTEs and to `body`); `body` is the
/// main query expression. Built only when a `WITH` is present — a plain query stays
/// `Statement::Select`/`Statement::SetOp`, so those paths are untouched (the `SetOp` precedent).
/// `recursive` is the `WITH RECURSIVE` flag (spec/design/recursive-cte.md): a flag on the whole
/// list that ENABLES a CTE to reference itself (lifting the forward-only `42P01`); a CTE that does
/// not reference itself is still an ordinary non-recursive CTE.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WithQuery {
    pub ctes: Vec<Cte>,
    /// The main statement the CTEs prefix: a query, or (spec/design/writable-cte.md) a
    /// data-modifying `INSERT`/`UPDATE`/`DELETE` primary.
    pub body: CteBody,
    pub recursive: bool,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SelectItems {
    All,
    /// Projected expressions, one per output column.
    Items(Vec<SelectItem>),
}

/// A `GROUP BY` grouping term (grammar.md §18, spec/design/aggregates.md §12). Most queries use
/// only `Set` with one column each (plain `GROUP BY a, b` parses as `[Set([a]), Set([b])]`); the
/// `ROLLUP`/`CUBE`/`GROUPING SETS` forms produce *multiple* grouping sets the resolver expands and
/// cross-products. Each `Expr` inside is a bare/qualified `Column` (the parser enforces it).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum GroupItem {
    /// A single grouping set's column list: a bare column `a` (`Set([a])`), a parenthesized group
    /// `(a, b)` (`Set([a, b])`), or the empty set `()` (`Set([])`).
    Set(Vec<Expr>),
    /// `ROLLUP (g1, …, gn)` — n+1 grouping sets: the prefixes of the column groups, longest first
    /// down to the empty set. Each `gi` is a column group (one or more columns).
    Rollup(Vec<Vec<Expr>>),
    /// `CUBE (g1, …, gn)` — 2^n grouping sets: every subset of the column groups.
    Cube(Vec<Vec<Expr>>),
    /// `GROUPING SETS (e1, …, en)` — the concatenation of each element's expansion; an element may
    /// itself be a `Set`/`Rollup`/`Cube`/nested `GroupingSets`.
    GroupingSets(Vec<GroupItem>),
}

impl GroupItem {
    /// Visit every column `Expr` contained anywhere in this grouping term — used by the analysis
    /// walks that scan a SELECT's expressions (privilege collection, sublink/sequence detection).
    pub fn for_each_expr<'a>(&'a self, f: &mut impl FnMut(&'a Expr)) {
        match self {
            GroupItem::Set(cols) => cols.iter().for_each(|e| f(e)),
            GroupItem::Rollup(groups) | GroupItem::Cube(groups) => {
                groups.iter().flatten().for_each(|e| f(e));
            }
            GroupItem::GroupingSets(elems) => elems.iter().for_each(|e| e.for_each_expr(f)),
        }
    }
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

/// One subscript spec inside an [`Expr::Subscript`] (spec/design/array.md §6): an index `[i]` or a
/// slice `[m:n]`. A slice's lower/upper bound may be omitted (`[:n]`, `[m:]`, `[:]`), defaulting to
/// the array's own lower / upper bound at evaluation.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SubscriptSpec {
    Index(Expr),
    Slice(Option<Expr>, Option<Expr>),
}

/// A general expression, shared by the SELECT list, WHERE, and UPDATE ... SET. The
/// productions are layered by precedence in the parser (spec/grammar/grammar.ebnf
/// `expr`); this is the flat resulting tree. A comparison/logical/null-test node is
/// boolean-valued; arithmetic and columns/integer-literals are integer-valued.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Expr {
    Column(String),
    /// A qualified column reference `rel.col`, where `rel` is a relation label in the FROM
    /// clause (its alias, else its table name). Resolved against exactly that one relation —
    /// never ambiguous (spec/design/grammar.md §15). Bare `Column` stays the unqualified form.
    QualifiedColumn {
        qualifier: String,
        name: String,
    },
    Literal(Literal),
    /// A `ROW(e1, e2, …)` composite constructor (spec/design/composite.md §1). Builds a row value
    /// from the field expressions; `ROW(x)` is a one-field row, `ROW()` the zero-field row. The
    /// bare parenthesized `(a, b)` form is deferred (`0A000`) — only the keyword form is parsed.
    Row(Vec<Expr>),
    /// An `ARRAY[e1, e2, …]` array constructor (spec/design/array.md §1). Builds a 1-D array value
    /// from the element expressions, unified to a common element type at resolve; `ARRAY[]` is the
    /// empty array (its element type comes from an enclosing cast/column context).
    Array(Vec<Expr>),
    /// Field selection `(expr).field` (spec/design/composite.md §S4) — the value of one named field
    /// of a composite `base`. The parser produces this for a `.name` postfix on a parenthesized /
    /// `ROW(…)` / cast / qualified-column base; a bare `a.b` stays `QualifiedColumn` and only falls
    /// back to field access at resolve when `a` is no relation but a composite column (the ambiguity
    /// rule — table.column first, then column.field). Field lookup is case-insensitive; an unknown
    /// field is 42703, a non-composite base 42809.
    FieldAccess {
        base: Box<Expr>,
        field: String,
    },
    /// Whole-row expansion `(expr).*` (spec/design/composite.md §S4) — expands a composite `base`
    /// into one output column per field, in declaration order. Valid only in a SELECT/RETURNING
    /// projection list (where `*` expands); in any scalar expression position it is 42601.
    FieldStar {
        base: Box<Expr>,
    },
    /// Array subscript `base[..][..]` (spec/design/array.md §6) — one or more bracketed specs
    /// applied to an array `base`. Each spec is an index `[i]` or a slice `[m:n]` (with optionally-
    /// omitted bounds: `[:n]`, `[m:]`, `[:]`). All-index access reads a single **1-based** element
    /// (the element type); if any spec is a slice the access returns a sub-array (the array type),
    /// and a scalar index `i` then means `1:i` (PG). An out-of-bounds / NULL subscript yields NULL
    /// (PG, not an error); subscripting a non-array base is 42804 at resolve. The parser collects
    /// consecutive `[…]` postfixes on any base into one node (so `a[1][2]` is one access, two specs).
    Subscript {
        base: Box<Expr>,
        subscripts: Vec<SubscriptSpec>,
    },
    /// A typed string literal `type '...'` (spec/design/grammar.md §36) — PostgreSQL's
    /// `type 'string'` form, equal to `CAST('string' AS type)` over a string-literal operand.
    /// `type_name` names the target scalar (resolved by `ScalarType::from_name`; unknown → 42704)
    /// and `text` is the literal's string. The keyword names the type, so the literal carries it in
    /// any expression position (`SELECT INTERVAL '1 day'`, `SELECT INTEGER '42'`); the string is
    /// coerced to the type at resolve — 22P02 malformed / 22003 out of range / the type's parse code.
    TypedLiteral {
        type_name: String,
        text: String,
    },
    /// A bind parameter `$N` (1-based index). Like an integer/string literal it is an
    /// *adaptable* operand: its type is inferred from context at resolve (sibling operand,
    /// target column, or CAST target), and the host binds a value at execute time
    /// (spec/design/api.md §5). An indeterminate type is 42P18.
    Param(u32),
    Cast {
        inner: Box<Expr>,
        type_name: String,
        /// An optional `numeric(p[,s])` type modifier on the CAST target.
        type_mod: Option<TypeMod>,
    },
    /// `EXTRACT(field FROM source)` (spec/design/timezones.md §9.2, grammar.md §50) — the datetime
    /// field special form. `field` is the syntactic field name (an identifier or a string literal,
    /// lowercased here); `source` is the datetime expression. Distinct from a function call because
    /// of the `field FROM source` syntax; resolves to a `numeric` value.
    Extract {
        field: String,
        source: Box<Expr>,
    },
    /// `expr COLLATE "name"` — the postfix collation operator (spec/design/collation.md §1). Sets
    /// an EXPLICIT collation on a text expression for the surrounding comparison / `ORDER BY`. Binds
    /// at the postfix/typecast level (tighter than `||` and the comparisons — PG precedence). The
    /// collation name is a quoted identifier (case-sensitive, e.g. `"C"`, `"en-US"`); `"C"` is always
    /// available, any other must be loaded (`db.ImportCollation`) else 42704. Resolving over a
    /// non-collatable (non-text) inner type is 42809; combining two different explicit collations in
    /// one comparison is 42P22.
    Collate {
        inner: Box<Expr>,
        collation: String,
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
    /// `operand IS [NOT] JSON [VALUE|SCALAR|ARRAY|OBJECT] [(WITH|WITHOUT) UNIQUE [KEYS]]` — the
    /// SQL/JSON well-formedness predicate (spec/design/json-sql-functions.md §5): is `operand` (a
    /// character string / json / jsonb) well-formed JSON of the optional `kind`, with optionally
    /// unique object keys. A non-string/json operand → 42804; a NULL operand → NULL; never raises.
    IsJson {
        operand: Box<Expr>,
        negated: bool,
        kind: JsonPredicateKind,
        unique_keys: bool,
    },
    /// `JSON(expr [(WITH|WITHOUT) UNIQUE [KEYS]])` — the SQL/JSON `JSON()` constructor
    /// (spec/design/json-sql-functions.md §5): parse a character string to a `json` value (verbatim).
    /// Malformed → 22P02; `WITH UNIQUE KEYS` on a duplicate object key → 22030. STRICT.
    JsonCtor {
        operand: Box<Expr>,
        unique_keys: bool,
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
    /// `lhs IN (list)` / `lhs NOT IN (list)` — membership over a non-empty value list
    /// (grammar.md §20). Desugared at resolve into the OR-chain PostgreSQL defines it as
    /// (`x IN (a,b)` ≡ `x = a OR x = b`; `NOT IN` is its negation), so the three-valued NULL
    /// semantics and per-element operand typing are inherited from `=`/OR/NOT. The parser
    /// guarantees `list` is non-empty (`IN ()` is 42601).
    In {
        lhs: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    /// `lhs BETWEEN lo AND hi` / `lhs NOT BETWEEN lo AND hi` — range test (grammar.md §21).
    /// Desugared at resolve into `lhs >= lo AND lhs <= hi` (NOT BETWEEN negates), inheriting the
    /// three-valued NULL semantics from the comparisons and Kleene AND. The bounds are parsed at
    /// the additive level so the structural `AND` is not the logical connective.
    Between {
        lhs: Box<Expr>,
        lo: Box<Expr>,
        hi: Box<Expr>,
        negated: bool,
    },
    /// `lhs LIKE rhs` / `lhs NOT LIKE rhs` — text pattern match (grammar.md §22). `%` matches
    /// any run of characters, `_` one code point, with the default `\` escape. Both operands
    /// must be text; NULL propagates. A genuine operator (not desugared) with a hand-written
    /// matcher. `negated` carries the NOT keyword; `insensitive` carries `ILIKE` (case-insensitive
    /// matching, both sides simple-lowercased under the casing regime — collation.md §16).
    Like {
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        negated: bool,
        insensitive: bool,
    },
    /// `lhs ~ rhs` / `~*` / `!~` / `!~*` — regular-expression match (grammar.md §22b, regex.md).
    /// jed's own RE2-able flavor (not PostgreSQL-compatible), matched by a hand-written linear-time
    /// Pike VM. UNANCHORED (matches a substring). Both operands must be text; NULL propagates.
    /// `negated` carries `!~`/`!~*`; `insensitive` carries `~*`/`!~*` (case-insensitive, both sides
    /// simple-lowercased like ILIKE — collation.md §16).
    Regex {
        lhs: Box<Expr>,
        rhs: Box<Expr>,
        negated: bool,
        insensitive: bool,
    },
    /// A `CASE` expression (grammar.md §23). Searched form: `operand` is `None`, each `whens`
    /// condition must be boolean. Simple form: `operand` is `Some(x)`, each branch matches when
    /// `x = val`. `whens` is `(condition_or_value, result)` pairs (≥1). `els` is the `ELSE`
    /// result, or `None` for an implicit `ELSE NULL`. Lazily evaluated: the first TRUE branch
    /// wins; result-arm types unify to a common type.
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        els: Option<Box<Expr>>,
    },
    /// A function call — the shared aggregate/scalar call syntax (grammar.md §17). `name` is
    /// the spelling as written, resolved case-insensitively: an aggregate (COUNT/SUM/MIN/MAX/
    /// AVG, kind = "aggregate"), a scalar function (abs/round, kind = "function",
    /// spec/design/functions.md §9), or 42883 (undefined_function). `star` is the `COUNT(*)`
    /// row-count form (then `args` is empty); otherwise `args` is the comma-separated argument
    /// list — aggregates and `abs` take one, `round` one or two. `distinct` carries a leading
    /// `DISTINCT` inside the parens (`COUNT(DISTINCT x)`, aggregates.md §5). An aggregate in
    /// WHERE/ON or nested in another aggregate is 42803 (spec/design/aggregates.md); a scalar
    /// function is legal anywhere an expression is.
    /// `arg_names` carries PostgreSQL named notation (`name => value`, grammar.md §17): `None`
    /// ⇒ every argument positional (the common case — no allocation, and the hot `Expr` enum
    /// stays small); `Some(boxed)` is a per-argument name vector parallel to `args` (`Some(name)`
    /// for a named slot, `None` for a positional one). Boxed so a plain call does not grow `Expr`.
    /// The parser enforces that no positional arg follows a named one.
    FuncCall {
        name: String,
        args: Vec<Expr>,
        arg_names: Option<Box<Vec<Option<String>>>>,
        star: bool,
        /// `true` when the argument was prefixed with `DISTINCT` (`COUNT(DISTINCT x)` —
        /// aggregates.md §5): the aggregate folds only the distinct non-NULL argument values.
        /// Only an aggregate accepts it — `DISTINCT` on a scalar function is 42809, on a window
        /// function 0A000, and `f(DISTINCT *)` / `f(DISTINCT)` is a 42601 syntax error.
        distinct: bool,
        /// `Some(cond)` when the call carries a trailing `FILTER (WHERE cond)` clause
        /// (`SUM(x) FILTER (WHERE y > 0)` — aggregates.md §11): the aggregate folds only the input
        /// rows for which `cond` is TRUE (NULL/FALSE rows contribute nothing). Only an aggregate
        /// accepts it — `FILTER` on a scalar function is 42809, on a window function 0A000; an
        /// aggregate inside `cond` is 42803 and a non-boolean `cond` is 42804. Boxed so a plain
        /// call does not grow `Expr`.
        filter: Option<Box<Expr>>,
        /// `true` when the final argument was prefixed with the `VARIADIC` keyword
        /// (`num_nulls(VARIADIC arr)`, array-functions.md §12 / grammar.md §17): the array is
        /// passed directly to a variadic parameter rather than spreading individual arguments.
        /// `false` for every ordinary call (the all-positional/spread fast path).
        variadic: bool,
        /// `Some` when the call carries a trailing `OVER (...)` window clause (a WINDOW-function
        /// call — spec/design/window.md). `None` for an ordinary scalar/aggregate/SRF call. A
        /// window-only function (row_number/…) with `over = None` is 42P20; an aggregate with
        /// `over = Some` is a window aggregate (S3).
        over: Option<Box<WindowDef>>,
        /// `Some(name)` when the call is `f(...) OVER name` referencing a named window (the WINDOW
        /// clause — spec/design/window.md §5). A desugaring pass replaces it with the named
        /// definition (into `over`) before resolution; exactly one of `over`/`over_name` is set on
        /// a window call. `None` for an inline `OVER (...)` or a non-window call.
        over_name: Option<String>,
    },
    /// A scalar subquery `( query_expr )` in expression position (grammar.md §26). `resolve`
    /// plans it once against the scope chain; an uncorrelated one is then folded to a constant,
    /// a correlated one is re-executed per outer row. A `$N` inside is a `0A000`.
    ScalarSubquery(Box<QueryExpr>),
    /// `EXISTS ( query_expr )` (grammar.md §26) — the bare existence predicate (a leading `NOT`
    /// is the ordinary unary connective wrapping this).
    Exists(Box<QueryExpr>),
    /// `lhs [NOT] IN ( query_expr )` (grammar.md §26) — membership of `lhs` in the subquery's
    /// single output column (three-valued, like a literal `IN`).
    InSubquery {
        lhs: Box<Expr>,
        query: Box<QueryExpr>,
        negated: bool,
    },
    /// `lhs op ANY/SOME/ALL ( array )` — a quantified array comparison (grammar.md §41,
    /// array-functions.md §11). `op` is a comparison (`= < > <= >=`); `all` is true for `ALL`,
    /// false for `ANY`/`SOME` (SOME folds to ANY at parse). The array operand resolves to an
    /// array type; the three-valued fold over its flattened elements reuses the `IN`-list
    /// membership semantics (`x = ANY(arr)` ≡ `x IN (the elements)`), generalized to all five
    /// operators and both quantifiers.
    Quantified {
        op: BinaryOp,
        all: bool,
        lhs: Box<Expr>,
        array: Box<Expr>,
    },
    /// `lhs op ANY/SOME/ALL ( query_expr )` — the SUBQUERY form of the quantified comparison
    /// (array-functions.md §11.6), the subquery spelling of `IN`. Parallel to `InSubquery`: the
    /// body's single column (42601 if >1) folds through the SAME three-valued fold as `Quantified`
    /// (`= ANY` ≡ `IN`), with no `21000` cardinality limit. Uncorrelated folds to a constant-array
    /// `Quantified`; correlated re-executes per outer row.
    QuantifiedSubquery {
        op: BinaryOp,
        all: bool,
        lhs: Box<Expr>,
        query: Box<QueryExpr>,
    },
}

/// The optional kind word of an `IS JSON` predicate (spec/design/json-sql-functions.md §5).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JsonPredicateKind {
    /// `IS JSON` / `IS JSON VALUE` — any well-formed JSON.
    Value,
    /// `IS JSON SCALAR` — a JSON scalar (string/number/boolean/null), not an object or array.
    Scalar,
    /// `IS JSON ARRAY` — a JSON array.
    Array,
    /// `IS JSON OBJECT` — a JSON object.
    Object,
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
    // not-equal `<>` (alias `!=`): the 3VL negation of Eq, propagating NULL like Eq.
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    // logical (boolean operands → boolean result, Kleene)
    And,
    Or,
    // array concatenation `||` (spec/design/array-functions.md §8): array∥array (array_cat),
    // array∥element (array_append), element∥array (array_prepend). Resolved polymorphically.
    Concat,
    // array containment / overlap (spec/design/array-functions.md §10): `@>` contains, `<@`
    // contained-by, `&&` overlaps. Each `anyarray <op> anyarray → boolean`, resolved polymorphically.
    // The range surface (spec/design/range-functions.md §3) reuses these three (range operands route
    // to the range axis) and adds the five positional/adjacency operators below.
    Contains,
    ContainedBy,
    Overlaps,
    // range boolean operators (spec/design/range-functions.md §3, RF3): `<<` strictly-left, `>>`
    // strictly-right, `&<` not-extend-right, `&>` not-extend-left, `-|-` adjacent. Range-only,
    // `anyrange <op> anyrange → boolean`.
    StrictlyLeft,
    StrictlyRight,
    NotExtendRight,
    NotExtendLeft,
    Adjacent,
    // jsonb accessor operators (spec/design/json-sql-functions.md §1, J4): `->` get field/element,
    // `->>` get as text, `#>` get at path, `#>>` get at path as text. The result type and the
    // field-vs-index split are decided at resolve from the operand types.
    JsonGet,
    JsonGetText,
    JsonGetPath,
    JsonGetPathText,
    // jsonb key-existence operators (spec/design/json-sql-functions.md §1, J5): `?` a key exists,
    // `?|` any key of a text[] exists, `?&` all keys exist. `boolean` result.
    JsonHasKey,
    JsonHasAnyKey,
    JsonHasAllKeys,
    // jsonb delete-at-path operator (spec/design/json-sql-functions.md §1, J6): `#-`. (The `||`
    // concat reuses `Concat`, and `-` delete reuses `Sub` — both dispatched by operand type.)
    JsonDeletePath,
    /// The `@?` jsonpath-exists operator (`jsonb @? jsonpath` = `jsonb_path_exists`) — jsonpath.md §6.
    JsonPathExists,
    /// The `@@` jsonpath-match operator (`jsonb @@ jsonpath` = `jsonb_path_match`) — jsonpath.md §6.
    JsonPathMatch,
}

/// One ORDER BY sort key: a bare table column, a sort direction, and a resolved NULL
/// placement. `nulls_first` is resolved at parse time — an explicit `NULLS FIRST|LAST`,
/// else the direction default (`descending`: ASC → last, DESC → first, the PostgreSQL
/// model where NULL is the largest value) — so the executor never re-derives it. Placement
/// is applied independently of the `descending` value flip (spec/design/grammar.md §10).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct OrderKey {
    /// An optional relation qualifier (`ORDER BY t.a`); `None` is a bare column.
    pub qualifier: Option<String>,
    pub column: String,
    /// An optional explicit `COLLATE "name"` on this sort key (`ORDER BY name COLLATE "en-US"`,
    /// spec/design/collation.md §1). `None` ⇒ the column's collation (the database default, `C`,
    /// until per-column collation lands in slice 1d). A non-`C` name orders this key by that
    /// collation's UCA sort key; an unknown name is 42704, a non-text column with a COLLATE is 42809.
    pub collation: Option<String>,
    pub descending: bool,
    pub nulls_first: bool,
}

/// One window `ORDER BY` sort key (spec/design/window.md §3/§5.1). Unlike the query `OrderKey`
/// (column references only), a window sort key is a **general expression** (`ORDER BY a + b`,
/// `ORDER BY sum(x)` in a grouped query) — the deferred general-expression-key follow-on. A bare
/// column expression is resolved to its row slot directly (unchanged); a compound expression is
/// materialized into a synthetic window-key column before the window stage. `collation` /
/// `descending` / `nulls_first` carry the same meaning as `OrderKey` (the latter resolved at parse).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WindowOrderKey {
    pub expr: Expr,
    /// An explicit `COLLATE "name"` on this key; `None` ⇒ the key expression's (text) collation. A
    /// COLLATE on a non-text key is 42804; an unknown name is 42704 (spec/design/window.md §5.1).
    pub collation: Option<String>,
    pub descending: bool,
    pub nulls_first: bool,
}

/// A window definition — the body of an `OVER (...)` clause (spec/design/window.md §3). Carries an
/// optional base-window name, `PARTITION BY`, `ORDER BY`, and a frame clause. Both `partition` and
/// `order` are **general expressions** (`PARTITION BY a + b`, `ORDER BY a % 2`, `ORDER BY sum(x)` in
/// a grouped query — spec/design/window.md §5.1); a bare column resolves to its row slot directly, a
/// compound expression is materialized into a synthetic window-key column before the window stage.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WindowDef {
    /// An optional leading base-window name (`OVER (w ORDER BY …)`, `WINDOW w2 AS (w …)` —
    /// spec/design/window.md §5). The definition extends the named base: it inherits the base's
    /// `PARTITION BY` (and its `ORDER BY` if any) and supplies its own frame. A resolve-time pass
    /// (`resolve_window_clause` / `desugar_named_windows`) merges the base in and clears this to
    /// `None`, so every definition is inline (`base = None`) by the time the window stage runs.
    pub base: Option<String>,
    pub partition: Vec<Expr>,
    pub order: Vec<WindowOrderKey>,
    /// An explicit frame clause (`ROWS BETWEEN … AND …`), else `None` for the default frame
    /// (spec/design/window.md §6). S4 supports `ROWS` mode; explicit `RANGE`/`GROUPS` and `EXCLUDE`
    /// are parsed but rejected `0A000` at resolve.
    pub frame: Option<WindowFrame>,
}

/// A window frame clause (spec/design/window.md §6).
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct WindowFrame {
    pub mode: FrameMode,
    pub start: FrameBound,
    pub end: FrameBound,
    pub exclude: FrameExclusion,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameMode {
    Rows,
    Range,
    Groups,
}

/// Frame exclusion (`EXCLUDE …` — spec/design/window.md §6): which rows to drop from the computed
/// `[lo, hi)` frame, per current row. `NoOthers` (the default / no `EXCLUDE`) drops nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FrameExclusion {
    NoOthers,
    CurrentRow,
    Group,
    Ties,
}

/// A frame boundary. `Preceding`/`Following` carry the offset expression (a non-negative integer
/// in `ROWS`/`GROUPS`; a value offset in `RANGE`).
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum FrameBound {
    UnboundedPreceding,
    Preceding(Box<Expr>),
    CurrentRow,
    Following(Box<Expr>),
    UnboundedFollowing,
}

/// A literal value as written in SQL.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Literal {
    Null,
    /// An integer literal. Stored as i64 (the parser folds a leading unary minus into
    /// the value). A bare integer literal is an *untyped constant* that adapts to its
    /// context — the target column on INSERT/UPDATE, a sibling operand, the compared
    /// column in a WHERE predicate — and traps 22003 if it does not fit; with no
    /// context it defaults to i64. See spec/design/types.md §6.
    Int(i64),
    /// A boolean literal, `TRUE` or `FALSE`. boolean is expression-only this slice
    /// (spec/design/types.md §1): a boolean literal is well-formed but cannot be
    /// stored in a column.
    Bool(bool),
    /// A single-quoted text literal (decoded content). Its type is always `text`
    /// (the one collation, `C`); it does not adapt to context like an integer literal
    /// does (spec/design/types.md §11).
    Text(String),
    /// A decimal literal (a numeric literal with a `.`). Carries the constructed value (the
    /// parser folds a leading unary minus into its sign); it is an untyped decimal constant
    /// that adapts to context and whose precision/scale caps are checked at resolve
    /// (spec/design/grammar.md §14, decimal.md §6).
    Decimal(Decimal),
}
