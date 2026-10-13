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
    /// PostgreSQL's DETAIL sentence naming the offending values (`Key (id)=(1) already exists.`) —
    /// set on the class-23 violations, subject to the session's SELECT privilege
    /// (spec/design/error-fields.md §9). Never part of the rendered message.
    pub detail: Option<String>,
    /// The 1-based **character** offset into the statement text of the offending token —
    /// set for lexer/parser errors and name-resolution errors (spec/design/error-fields.md §10).
    pub position: Option<u32>,
    /// PostgreSQL's HINT suggestion (`Perhaps you meant to reference the column "t.name".`) —
    /// spec/design/error-fields.md §11. Never part of the rendered message.
    pub hint: Option<String>,
    /// Set once an expression node's resolution has decided this error's position (stamped or
    /// deliberately left absent), so an enclosing node never claims an error raised inside one of
    /// its operands (spec/design/error-fields.md §10.2). Internal bookkeeping, not a field.
    pub(crate) located: bool,
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
            detail: None,
            position: None,
            hint: None,
            located: false,
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

    /// Attach a `Detail` (spec/design/error-fields.md §9). `None` (a gated or inapplicable detail)
    /// leaves the field unset, so a raise site can pass a helper's result straight through.
    #[must_use]
    pub(crate) fn with_detail(mut self, detail: Option<String>) -> Self {
        self.detail = detail;
        self
    }

    /// Set `Position` unless one is already set: the innermost site that knows a location wins
    /// (spec/design/error-fields.md §10.2). A `0` position means "unknown" and is ignored.
    #[must_use]
    pub(crate) fn with_position(mut self, pos: u32) -> Self {
        if self.position.is_none() && pos > 0 {
            self.position = Some(pos);
        }
        self
    }

    /// Stamp an error leaving an expression node's resolution with `pos` unless an inner node already
    /// decided its position, then mark it decided (spec/design/error-fields.md §10.2).
    #[must_use]
    pub(crate) fn locate(mut self, pos: u32) -> Self {
        if !self.located {
            self = self.with_position(pos);
            self.located = true;
        }
        self
    }

    /// Position a DML target's `42P01` at the target's name (spec/design/error-fields.md §10.2); any
    /// other error passes through.
    #[must_use]
    pub(crate) fn at_target(self, pos: u32) -> Self {
        if self.state == SqlState::UndefinedTable {
            self.with_position(pos)
        } else {
            self
        }
    }

    /// Shift `Position` by `chars` characters — a statement's error moved into the coordinates of
    /// the script that contains it (spec/design/error-fields.md §10.1).
    #[must_use]
    pub(crate) fn shift_position(mut self, chars: u32) -> Self {
        if let Some(p) = self.position {
            self.position = Some(p + chars);
        }
        self
    }

    /// Set `Hint` unless one is already set (spec/design/error-fields.md §11).
    #[must_use]
    pub(crate) fn with_hint(mut self, hint: Option<String>) -> Self {
        if self.hint.is_none() {
            self.hint = hint;
        }
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

    /// 23503, child side under MATCH FULL — an INSERT/UPDATE on `table` writes a referencing
    /// tuple that mixes NULL and non-NULL values under foreign key `constraint`
    /// (constraints.md §6.3). PostgreSQL's DETAIL names no values, so it is never privilege-gated.
    pub(crate) fn fk_violation_match_full(table: &str, constraint: &str) -> Self {
        Self::fk_violation_insert(table, constraint).with_detail(Some(
            "MATCH FULL does not allow mixing of null and nonnull key values.".to_string(),
        ))
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
