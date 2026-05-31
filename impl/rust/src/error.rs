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
    /// 42601 — syntax error. (Local class 42; not yet in the registry — see note.)
    SyntaxError,
    /// 42P01 — undefined table.
    UndefinedTable,
    /// 42703 — undefined column.
    UndefinedColumn,
    /// 42804 — datatype mismatch / undefined type name.
    DatatypeMismatch,
    /// 0A000 — feature not supported (used by not-yet-implemented surface).
    FeatureNotSupported,
}

impl SqlState {
    pub fn code(self) -> &'static str {
        match self {
            SqlState::NumericValueOutOfRange => "22003",
            SqlState::SyntaxError => "42601",
            SqlState::UndefinedTable => "42P01",
            SqlState::UndefinedColumn => "42703",
            SqlState::DatatypeMismatch => "42804",
            SqlState::FeatureNotSupported => "0A000",
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
