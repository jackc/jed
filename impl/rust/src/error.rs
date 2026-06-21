//! Structured engine errors (CLAUDE.md §5, §10).
//!
//! Errors carry a stable SQLSTATE code from the spec's error registry
//! (spec/errors/registry.toml), never free text for matching. The conformance
//! harness matches `statement error <sqlstate>` on `code`, not on the message.

use std::fmt;

/// SQLSTATE codes used by this core. The string value is the canonical 5-char
/// code; it must match spec/errors/registry.toml (cross-checked in tests).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SqlState {
    /// 21000 — cardinality violation: a scalar subquery used as an expression returned
    /// more than one row (spec/design/grammar.md §26).
    CardinalityViolation,
    /// 22000 — the bare class-22 data exception. PostgreSQL raises it for "argument must be
    /// empty or one-dimensional array" — array_append/array_prepend on a multidimensional
    /// array (spec/design/array-functions.md §3.2).
    DataException,
    /// 22003 — numeric value out of range (integer overflow; CLAUDE.md §8).
    NumericValueOutOfRange,
    /// 22004 — null value not allowed. PostgreSQL raises it for "initial position must not be
    /// null" — array_position's optional start subscript (spec/design/array-functions.md §8).
    NullValueNotAllowed,
    /// 22007 — invalid datetime format (malformed timestamp / timestamptz input).
    InvalidDatetimeFormat,
    /// 22008 — datetime field overflow (an out-of-range datetime field or a value
    /// beyond the representable i64-microsecond range; spec/design/timestamp.md).
    DatetimeFieldOverflow,
    /// 22012 — division (or modulo) by zero.
    DivisionByZero,
    /// 22023 — invalid parameter value (e.g. a bad numeric typmod, `numeric(0)`).
    InvalidParameterValue,
    /// 2200H — sequence generator limit exceeded: `nextval` advanced past a sequence's
    /// MAXVALUE/MINVALUE bound without CYCLE (spec/design/sequences.md §4).
    SequenceGeneratorLimitExceeded,
    /// 2202E — array subscript error: a multidimensional array constructed/parsed with
    /// non-matching sub-array dimensions, or an array literal whose declared `[l:u]` bounds are
    /// inverted (`l > u`) — spec/design/array.md §11.
    ArraySubscriptError,
    /// 22P02 — invalid text representation (e.g. malformed bytea hex input).
    InvalidTextRepresentation,
    /// 22025 — invalid escape sequence (a LIKE pattern ending in a lone escape character).
    InvalidEscapeSequence,
    /// 2201W — invalid row count in a LIMIT clause (a negative LIMIT).
    InvalidRowCountInLimitClause,
    /// 2201X — invalid row count in an OFFSET clause (a negative OFFSET).
    InvalidRowCountInOffsetClause,
    /// 23502 — not-null constraint violation.
    NotNullViolation,
    /// 23505 — unique (primary key) constraint violation.
    UniqueViolation,
    /// 23514 — check constraint violation: a candidate row falsified a CHECK
    /// expression at INSERT/UPDATE (spec/design/constraints.md §4).
    CheckViolation,
    /// 23503 — foreign-key violation: a child INSERT/UPDATE whose key is absent in the parent,
    /// or a parent DELETE/UPDATE of a row still referenced by a child
    /// (spec/design/constraints.md §6).
    ForeignKeyViolation,
    /// 25001 — a `BEGIN` issued while a transaction is already open (no nesting — there is no
    /// SAVEPOINT this slice; spec/design/transactions.md §4.2).
    ActiveSqlTransaction,
    /// 25006 — a write statement issued in a READ ONLY transaction (transactions.md §4.3).
    ReadOnlySqlTransaction,
    /// 25P02 — a statement (other than ROLLBACK/COMMIT) issued in a failed/aborted transaction
    /// block; it stays poisoned until the block ends (transactions.md §6).
    InFailedSqlTransaction,
    /// 55000 — object not in prerequisite state: `currval`/`lastval` before `nextval` has defined
    /// the value in this session (spec/design/sequences.md §6).
    ObjectNotInPrerequisiteState,
    /// 42601 — syntax error.
    SyntaxError,
    /// 42P01 — undefined table.
    UndefinedTable,
    /// 42703 — undefined column.
    UndefinedColumn,
    /// 42702 — ambiguous column (a bare column name that matches more than one relation
    /// in scope; spec/design/grammar.md §15).
    AmbiguousColumn,
    /// 42704 — undefined object (e.g. an unknown type name).
    UndefinedObject,
    /// 42P10 — invalid column reference (a SELECT DISTINCT ORDER BY key not in the
    /// select list).
    InvalidColumnReference,
    /// 42804 — datatype mismatch (a value's type is wrong for its context).
    DatatypeMismatch,
    /// 42830 — invalid foreign key: the referenced columns are not the parent's PRIMARY KEY or a
    /// UNIQUE constraint, or the referencing/referenced column counts disagree
    /// (spec/design/constraints.md §6.2). A type mismatch is `DatatypeMismatch` (42804) instead.
    InvalidForeignKey,
    /// 42P07 — duplicate table/relation: CREATE TABLE or CREATE INDEX of a name already
    /// taken in the shared relation namespace (spec/design/indexes.md §2).
    DuplicateTable,
    /// 42701 — duplicate column (two columns with the same name).
    DuplicateColumn,
    /// 42712 — duplicate alias (two FROM relations share a label; a self-join needs
    /// distinct aliases; spec/design/grammar.md §15).
    DuplicateAlias,
    /// 42P16 — invalid table definition (e.g. more than one primary key).
    InvalidTableDefinition,
    /// 42803 — grouping error: a non-aggregated column not in GROUP BY, or an aggregate in
    /// a context that disallows one (WHERE / ON / nested in another aggregate;
    /// spec/design/aggregates.md §6).
    GroupingError,
    /// 42883 — undefined function (an unknown function name in a call;
    /// spec/design/aggregates.md §5).
    UndefinedFunction,
    /// 42P19 — invalid recursion: a `WITH RECURSIVE` CTE that references itself but is
    /// structurally ill-formed (not `non-recursive-term UNION [ALL] recursive-term`, a
    /// self-reference in the wrong place, or an aggregate in the recursive term;
    /// spec/design/recursive-cte.md §6).
    InvalidRecursion,
    /// 42P18 — indeterminate datatype: a bind parameter `$N` whose type cannot be inferred
    /// from context (spec/design/api.md §5).
    IndeterminateDatatype,
    /// 42P02 — undefined parameter: a bind parameter `$N` where none can exist (a CHECK
    /// expression; spec/design/constraints.md §4.1).
    UndefinedParameter,
    /// 42710 — duplicate object: a constraint name already taken on this table
    /// (spec/design/constraints.md §4.3).
    DuplicateObject,
    /// 42809 — wrong object type: DROP TABLE of an index name, DROP INDEX of a table name
    /// (spec/design/indexes.md §2).
    WrongObjectType,
    /// 42P21 — collation mismatch: two DIFFERENT explicit collations combined in one comparison
    /// (`'a' COLLATE "C" < 'b' COLLATE "en-US"`; spec/design/collation.md §1/§7).
    CollationMismatch,
    /// 42P22 — indeterminate collation: two DIFFERENT IMPLICIT collations meet in one comparison /
    /// ORDER BY without an explicit `COLLATE` to break the tie (two columns with different
    /// collations; spec/design/collation.md §1/§7). Reachable since slice 1d (per-column collations).
    IndeterminateCollation,
    /// 2BP01 — dependent objects still exist: `DROP TYPE ... RESTRICT` of a composite type a
    /// column or another composite field still references (spec/design/composite.md §7).
    DependentObjectsStillExist,
    /// 42622 — name too long: an identifier (table / column / type / alias / function name)
    /// exceeds the engine's fixed maximum length of `parser::MAX_IDENTIFIER_LENGTH` = 63 bytes
    /// (CLAUDE.md §13; spec/design/cost.md §7). Checked when the lexer builds an identifier token,
    /// so it bounds every identifier on every parse path. PG's `42622 name_too_long`, but jed
    /// errors where PG silently truncates (identifiers are ASCII-only, so bytes = characters).
    NameTooLong,
    /// 428C9 — generated_always: a write supplying an explicit value to a `GENERATED ALWAYS`
    /// identity column without `OVERRIDING SYSTEM VALUE` (an INSERT), or assigning one (an UPDATE)
    /// (spec/design/sequences.md §13).
    GeneratedAlways,
    /// 42501 — insufficient privilege: the session's authorization envelope withheld the privilege
    /// a statement needs — a per-table `SELECT`/`INSERT`/`UPDATE`/`DELETE`, a function `EXECUTE`, or
    /// DDL permission (`allow_ddl`) (spec/design/session.md §5.3). PostgreSQL's own
    /// permission-denied code; jed enforces it from the host-configured session envelope rather than
    /// an in-database role catalog (CLAUDE.md §3) — checked at name resolution, after the object
    /// resolves (a missing object stays `42P01`).
    InsufficientPrivilege,
    /// 0A000 — feature not supported (used by not-yet-implemented surface).
    FeatureNotSupported,
    /// 54000 — program limit exceeded: the input SQL text exceeded the per-handle `max_sql_length`
    /// byte limit (CLAUDE.md §13; spec/design/api.md §8, cost.md §7). The §13 input-size gate,
    /// sibling of `StatementTooComplex`: both bound the parse before any cost is metered (the
    /// `54P01` cost ceiling cannot catch parse-time blowup). A per-handle setting (default 1 MiB,
    /// `0` ⇒ unlimited), checked at the handle's parse entry; deterministic + cross-core (§8).
    ProgramLimitExceeded,
    /// 54001 — statement too complex: a statement's expression / subquery / set-operation nesting
    /// depth exceeds the engine's fixed maximum (`parser::MAX_EXPR_DEPTH`). The native-stack-safety
    /// gate for untrusted input — deeply-nested SQL would otherwise overflow the recursive-descent
    /// parser / resolve / eval walks BEFORE the cost meter runs, so `54P01` cannot catch it
    /// (CLAUDE.md §13; spec/design/cost.md §7). Borrows PG's `54001 statement_too_complex` name/code,
    /// but the trigger is a fixed depth (deterministic, cross-core) not PG's runtime stack probe.
    StatementTooComplex,
    /// 54P01 — cost limit exceeded: a query's accrued execution cost reached the caller-set
    /// `max_cost` ceiling and execution was aborted (CLAUDE.md §13; spec/design/cost.md §6).
    /// jed-specific (PostgreSQL has no execution-cost ceiling); class 54 program_limit_exceeded.
    CostLimitExceeded,
    /// 54P03 — temp storage limit exceeded: a session's temporary-table storage reached the
    /// `temp_buffers` byte budget (spec/design/temp-tables.md §7). The §13 gate on RETAINED temp
    /// bytes (the cost ceilings bound work per/across statements, not bytes held between them);
    /// measured in byte-identical on-disk record bytes, so the abort is cross-core-identical (§8).
    TempStorageLimitExceeded,
    /// 54P02 — session cost limit exceeded: the session's cumulative execution cost reached the
    /// caller-set `lifetime_max_cost` budget and the in-flight statement was aborted, or a further
    /// statement was rejected at admission because the budget is already spent (spec/design/session.md
    /// §5.4). Sibling to `54P01` (which bounds one statement); jed-specific, class 54.
    SessionCostLimitExceeded,
    /// 58030 — I/O error from the host file layer (read/write/fsync/rename;
    /// spec/design/api.md §2).
    IoError,
    /// 58P01 — undefined file: `open` of a database path that does not exist.
    UndefinedFile,
    /// 58P02 — duplicate file: `create` of a database path that already exists.
    DuplicateFile,
    /// XX001 — data corrupted (a malformed on-disk database file; CLAUDE.md §8).
    DataCorrupted,
}

impl SqlState {
    pub fn code(self) -> &'static str {
        match self {
            SqlState::CardinalityViolation => "21000",
            SqlState::DataException => "22000",
            SqlState::NumericValueOutOfRange => "22003",
            SqlState::NullValueNotAllowed => "22004",
            SqlState::InvalidDatetimeFormat => "22007",
            SqlState::DatetimeFieldOverflow => "22008",
            SqlState::DivisionByZero => "22012",
            SqlState::InvalidParameterValue => "22023",
            SqlState::SequenceGeneratorLimitExceeded => "2200H",
            SqlState::ArraySubscriptError => "2202E",
            SqlState::InvalidTextRepresentation => "22P02",
            SqlState::InvalidEscapeSequence => "22025",
            SqlState::InvalidRowCountInLimitClause => "2201W",
            SqlState::InvalidRowCountInOffsetClause => "2201X",
            SqlState::NotNullViolation => "23502",
            SqlState::UniqueViolation => "23505",
            SqlState::CheckViolation => "23514",
            SqlState::ForeignKeyViolation => "23503",
            SqlState::ActiveSqlTransaction => "25001",
            SqlState::ReadOnlySqlTransaction => "25006",
            SqlState::InFailedSqlTransaction => "25P02",
            SqlState::ObjectNotInPrerequisiteState => "55000",
            SqlState::SyntaxError => "42601",
            SqlState::UndefinedTable => "42P01",
            SqlState::UndefinedColumn => "42703",
            SqlState::AmbiguousColumn => "42702",
            SqlState::UndefinedObject => "42704",
            SqlState::InvalidColumnReference => "42P10",
            SqlState::DatatypeMismatch => "42804",
            SqlState::InvalidForeignKey => "42830",
            SqlState::DuplicateTable => "42P07",
            SqlState::DuplicateColumn => "42701",
            SqlState::DuplicateAlias => "42712",
            SqlState::InvalidTableDefinition => "42P16",
            SqlState::GroupingError => "42803",
            SqlState::UndefinedFunction => "42883",
            SqlState::IndeterminateDatatype => "42P18",
            SqlState::InvalidRecursion => "42P19",
            SqlState::UndefinedParameter => "42P02",
            SqlState::DuplicateObject => "42710",
            SqlState::WrongObjectType => "42809",
            SqlState::CollationMismatch => "42P21",
            SqlState::IndeterminateCollation => "42P22",
            SqlState::NameTooLong => "42622",
            SqlState::GeneratedAlways => "428C9",
            SqlState::DependentObjectsStillExist => "2BP01",
            SqlState::InsufficientPrivilege => "42501",
            SqlState::FeatureNotSupported => "0A000",
            SqlState::ProgramLimitExceeded => "54000",
            SqlState::StatementTooComplex => "54001",
            SqlState::CostLimitExceeded => "54P01",
            SqlState::SessionCostLimitExceeded => "54P02",
            SqlState::TempStorageLimitExceeded => "54P03",
            SqlState::IoError => "58030",
            SqlState::UndefinedFile => "58P01",
            SqlState::DuplicateFile => "58P02",
            SqlState::DataCorrupted => "XX001",
        }
    }
}

/// An engine error: a SQLSTATE plus an informational (never-matched) message.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EngineError {
    pub state: SqlState,
    pub message: String,
}

impl EngineError {
    pub fn new(state: SqlState, message: impl Into<String>) -> Self {
        EngineError {
            state,
            message: message.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        self.state.code()
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The SQLSTATE is rendered into the message text so that `statement error
        // <code>` also matches as a plain regex under the stock sqllogictest runner
        // (spec/design/conformance.md §2).
        write!(f, "{}: {}", self.state.code(), self.message)
    }
}

impl std::error::Error for EngineError {}

pub type Result<T> = std::result::Result<T, EngineError>;
