//! Structured engine errors (CLAUDE.md §5, §10).
//!
//! Errors carry a stable SQLSTATE code from the spec's error registry
//! (spec/errors/registry.toml), never free text for matching. The conformance
//! harness matches `statement error <sqlstate>` on `code`, not on the message.
//!
//! Beyond the code + message, an error may carry **structured diagnostic fields**
//! modeled on PostgreSQL's protocol error fields (and pgx's `pgconn.PgError`):
//! the constraint / table / column / data-type name a host would otherwise have to
//! scrape from the (non-contractual) message text (spec/design/error-fields.md).

use std::fmt;

/// The `SqlState` enum + its `code()` mapping are generated from spec/errors/registry.toml
/// (the codegen "middle path", CLAUDE.md §5 — see sqlstate.rs / spec/design/codegen.md).
/// Re-exported here so `crate::error::SqlState` and the crate-root re-export keep resolving;
/// the hand-written `EngineError` scaffolding below consumes it.
pub use crate::sqlstate::SqlState;

/// An engine error: a SQLSTATE plus an informational (never-matched) message, and
/// optional structured diagnostic fields (spec/design/error-fields.md §3). The fields
/// mirror pgx's `pgconn.PgError` names: `None` means the field does not apply to this
/// error (the idiomatic Rust spelling of PostgreSQL's empty-string "absent").
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct EngineError {
    pub state: SqlState,
    pub message: String,
    /// The violated constraint's name — set for 23505/23514/23503/23P01.
    pub constraint_name: Option<String>,
    /// The relation the failing write targeted — set for the class-23 violations
    /// and stamped onto a column-store failure at the DML boundary.
    pub table_name: Option<String>,
    /// The column at fault — set for 23502 (and 22001 truncation).
    pub column_name: Option<String>,
    /// The data type at fault — set for 22003 / 22001.
    pub data_type_name: Option<String>,
}

impl EngineError {
    pub fn new(state: SqlState, message: impl Into<String>) -> Self {
        EngineError {
            state,
            message: message.into(),
            constraint_name: None,
            table_name: None,
            column_name: None,
            data_type_name: None,
        }
    }

    pub fn code(&self) -> &'static str {
        self.state.code()
    }

    // --- structured-field builders (spec/design/error-fields.md §5) -----------------
    // Chainable setters. `new()` leaves every field `None`, so existing call sites are
    // unaffected; only the raise sites that know an identifier opt in.

    #[must_use]
    pub(crate) fn with_constraint(mut self, name: impl Into<String>) -> Self {
        self.constraint_name = Some(name.into());
        self
    }

    #[must_use]
    pub(crate) fn with_table(mut self, name: impl Into<String>) -> Self {
        self.table_name = Some(name.into());
        self
    }

    #[must_use]
    pub(crate) fn with_column(mut self, name: impl Into<String>) -> Self {
        self.column_name = Some(name.into());
        self
    }

    #[must_use]
    pub(crate) fn with_data_type(mut self, name: impl Into<String>) -> Self {
        self.data_type_name = Some(name.into());
        self
    }

    // --- typed integrity-violation constructors (spec/design/error-fields.md §5) -----
    // One source per message template (mirrors spec/errors/registry.toml), so the prose
    // and the structured field can never drift apart. Message text is byte-identical to
    // the prior inline `format!`s — the conformance corpus matches on it.

    /// 23505 — a duplicate key for `constraint` (a unique index's name, or the derived
    /// `<table>_pkey`). `table` is the relation written.
    pub(crate) fn unique_violation(table: &str, constraint: impl Into<String>) -> Self {
        let constraint = constraint.into();
        EngineError::new(
            SqlState::UniqueViolation,
            format!("duplicate key value violates unique constraint: {constraint}"),
        )
        .with_table(table)
        .with_constraint(constraint)
    }

    /// 23514 — a row fails CHECK constraint `constraint` on `table`.
    pub(crate) fn check_violation(table: &str, constraint: &str) -> Self {
        EngineError::new(
            SqlState::CheckViolation,
            format!("new row for relation {table} violates check constraint {constraint}"),
        )
        .with_table(table)
        .with_constraint(constraint)
    }

    /// 23503, child side — an INSERT/UPDATE on `table` references a parent key absent under
    /// foreign key `constraint`.
    pub(crate) fn fk_violation_insert(table: &str, constraint: &str) -> Self {
        EngineError::new(
            SqlState::ForeignKeyViolation,
            format!(
                "insert or update on table {table} violates foreign key constraint {constraint}"
            ),
        )
        .with_table(table)
        .with_constraint(constraint)
    }

    /// 23503, parent side — a DELETE/UPDATE on `parent` strands a child of `child` still
    /// referencing it under foreign key `constraint`.
    pub(crate) fn fk_violation_delete(parent: &str, constraint: &str, child: &str) -> Self {
        EngineError::new(
            SqlState::ForeignKeyViolation,
            format!(
                "update or delete on table {parent} violates foreign key constraint {constraint} on table {child}"
            ),
        )
        .with_table(parent)
        .with_constraint(constraint)
    }

    /// 23P01 — a row conflicts with an existing one under EXCLUDE constraint `constraint`
    /// (the backing GiST index's name) on `table`.
    pub(crate) fn exclusion_violation(table: &str, constraint: &str) -> Self {
        EngineError::new(
            SqlState::ExclusionViolation,
            format!("conflicting key value violates exclusion constraint: {constraint}"),
        )
        .with_table(table)
        .with_constraint(constraint)
    }

    /// 23502 — a NULL into NOT NULL `column`. The table is stamped at the DML boundary
    /// (`with_table`), where the relation name is in scope.
    pub(crate) fn not_null_violation(column: &str) -> Self {
        EngineError::new(
            SqlState::NotNullViolation,
            format!("null value in column {column} violates not-null constraint"),
        )
        .with_column(column)
    }
}

impl fmt::Display for EngineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The SQLSTATE is rendered into the message text so that `statement error
        // <code>` also matches as a plain regex under the stock sqllogictest runner
        // (spec/design/conformance.md §2). The structured fields are metadata, not part
        // of the rendered line (spec/design/error-fields.md §3).
        write!(f, "{}: {}", self.state.code(), self.message)
    }
}

impl std::error::Error for EngineError {}

pub type Result<T> = std::result::Result<T, EngineError>;
