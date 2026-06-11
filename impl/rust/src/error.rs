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
    /// 22003 — numeric value out of range (integer overflow; CLAUDE.md §8).
    NumericValueOutOfRange,
    /// 22007 — invalid datetime format (malformed timestamp / timestamptz input).
    InvalidDatetimeFormat,
    /// 22008 — datetime field overflow (an out-of-range datetime field or a value
    /// beyond the representable int64-microsecond range; spec/design/timestamp.md).
    DatetimeFieldOverflow,
    /// 22012 — division (or modulo) by zero.
    DivisionByZero,
    /// 22023 — invalid parameter value (e.g. a bad numeric typmod, `numeric(0)`).
    InvalidParameterValue,
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
    /// 25001 — a `BEGIN` issued while a transaction is already open (no nesting — there is no
    /// SAVEPOINT this slice; spec/design/transactions.md §4.2).
    ActiveSqlTransaction,
    /// 25006 — a write statement issued in a READ ONLY transaction (transactions.md §4.3).
    ReadOnlySqlTransaction,
    /// 25P02 — a statement (other than ROLLBACK/COMMIT) issued in a failed/aborted transaction
    /// block; it stays poisoned until the block ends (transactions.md §6).
    InFailedSqlTransaction,
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
    /// 0A000 — feature not supported (used by not-yet-implemented surface).
    FeatureNotSupported,
    /// 54P01 — cost limit exceeded: a query's accrued execution cost reached the caller-set
    /// `max_cost` ceiling and execution was aborted (CLAUDE.md §13; spec/design/cost.md §6).
    /// jed-specific (PostgreSQL has no execution-cost ceiling); class 54 program_limit_exceeded.
    CostLimitExceeded,
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
            SqlState::NumericValueOutOfRange => "22003",
            SqlState::InvalidDatetimeFormat => "22007",
            SqlState::DatetimeFieldOverflow => "22008",
            SqlState::DivisionByZero => "22012",
            SqlState::InvalidParameterValue => "22023",
            SqlState::InvalidTextRepresentation => "22P02",
            SqlState::InvalidEscapeSequence => "22025",
            SqlState::InvalidRowCountInLimitClause => "2201W",
            SqlState::InvalidRowCountInOffsetClause => "2201X",
            SqlState::NotNullViolation => "23502",
            SqlState::UniqueViolation => "23505",
            SqlState::CheckViolation => "23514",
            SqlState::ActiveSqlTransaction => "25001",
            SqlState::ReadOnlySqlTransaction => "25006",
            SqlState::InFailedSqlTransaction => "25P02",
            SqlState::SyntaxError => "42601",
            SqlState::UndefinedTable => "42P01",
            SqlState::UndefinedColumn => "42703",
            SqlState::AmbiguousColumn => "42702",
            SqlState::UndefinedObject => "42704",
            SqlState::InvalidColumnReference => "42P10",
            SqlState::DatatypeMismatch => "42804",
            SqlState::DuplicateTable => "42P07",
            SqlState::DuplicateColumn => "42701",
            SqlState::DuplicateAlias => "42712",
            SqlState::InvalidTableDefinition => "42P16",
            SqlState::GroupingError => "42803",
            SqlState::UndefinedFunction => "42883",
            SqlState::IndeterminateDatatype => "42P18",
            SqlState::UndefinedParameter => "42P02",
            SqlState::DuplicateObject => "42710",
            SqlState::WrongObjectType => "42809",
            SqlState::FeatureNotSupported => "0A000",
            SqlState::CostLimitExceeded => "54P01",
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
