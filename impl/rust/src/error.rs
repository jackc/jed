//! Structured engine errors (CLAUDE.md §5, §10).
//!
//! Errors carry a stable SQLSTATE code from the spec's error registry
//! (spec/errors/registry.toml), never free text for matching. The conformance
//! harness matches `statement error <sqlstate>` on `code`, not on the message.

use std::fmt;

/// The `SqlState` enum + its `code()` mapping are generated from spec/errors/registry.toml
/// (the codegen "middle path", CLAUDE.md §5 — see sqlstate.rs / spec/design/codegen.md).
/// Re-exported here so `crate::error::SqlState` and the crate-root re-export keep resolving;
/// the hand-written `EngineError` scaffolding below consumes it.
pub use crate::sqlstate::SqlState;

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
