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
    /// 22003 — numeric value out of range (integer overflow; CLAUDE.md §8).
    NumericValueOutOfRange,
    /// 22012 — division (or modulo) by zero.
    DivisionByZero,
    /// 23502 — not-null constraint violation.
    NotNullViolation,
    /// 23505 — unique (primary key) constraint violation.
    UniqueViolation,
    /// 42601 — syntax error.
    SyntaxError,
    /// 42P01 — undefined table.
    UndefinedTable,
    /// 42703 — undefined column.
    UndefinedColumn,
    /// 42704 — undefined object (e.g. an unknown type name).
    UndefinedObject,
    /// 42804 — datatype mismatch (a value's type is wrong for its context).
    DatatypeMismatch,
    /// 42P07 — duplicate table (CREATE TABLE of an existing name).
    DuplicateTable,
    /// 42701 — duplicate column (two columns with the same name).
    DuplicateColumn,
    /// 42P16 — invalid table definition (e.g. more than one primary key).
    InvalidTableDefinition,
    /// 0A000 — feature not supported (used by not-yet-implemented surface).
    FeatureNotSupported,
    /// XX001 — data corrupted (a malformed on-disk database file; CLAUDE.md §8).
    DataCorrupted,
}

impl SqlState {
    pub fn code(self) -> &'static str {
        match self {
            SqlState::NumericValueOutOfRange => "22003",
            SqlState::DivisionByZero => "22012",
            SqlState::NotNullViolation => "23502",
            SqlState::UniqueViolation => "23505",
            SqlState::SyntaxError => "42601",
            SqlState::UndefinedTable => "42P01",
            SqlState::UndefinedColumn => "42703",
            SqlState::UndefinedObject => "42704",
            SqlState::DatatypeMismatch => "42804",
            SqlState::DuplicateTable => "42P07",
            SqlState::DuplicateColumn => "42701",
            SqlState::InvalidTableDefinition => "42P16",
            SqlState::FeatureNotSupported => "0A000",
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
