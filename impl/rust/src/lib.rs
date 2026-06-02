//! Rust core of the engine (CLAUDE.md §2).
//!
//! A downstream consumer of /spec — the canonical source of truth. This crate
//! implements the step-1 surface (integer DDL/DML/SELECT) and ships a conformance
//! harness (`src/bin/conformance.rs`) that runs the shared corpus.
//!
//! Boring, explicit modules with small footprints (CLAUDE.md §10).

pub mod ast;
pub mod catalog;
pub mod cost;
pub mod costs;
pub mod encoding;
pub mod error;
pub mod executor;
pub mod format;
pub mod lexer;
pub mod operators;
pub mod parser;
pub mod storage;
pub mod token;
pub mod types;
pub mod value;

pub use cost::Meter;
pub use error::{EngineError, Result, SqlState};
pub use executor::{Database, Outcome};
pub use parser::Parser;
pub use value::Value;

/// The capabilities this implementation currently supports (spec/conformance:
/// the gating axis). The harness runs a corpus file iff every capability in the
/// file's `# requires:` header is in this set. GROWS as Phases B–E land; in the
/// Phase A scaffold the engine supports no SQL features yet, so this is empty and
/// zero conformance files run (the foundation tests still pass).
/// The capabilities this implementation currently supports (spec/conformance:
/// the gating axis). The harness runs a corpus file iff every capability in the
/// file's `# requires:` header is in this set. GROWS as Phases B–E land. A whole
/// corpus file only runs once *all* its required capabilities are present, so the
/// harness stays all-skip until the `core` profile is complete (Phase E); per-phase
/// correctness is driven by the in-crate unit tests until then.
pub const SUPPORTED_CAPABILITIES: &[&str] = &[
    // Phase B — CREATE TABLE with typed columns + single-column PRIMARY KEY.
    "ddl.create_table",
    "ddl.primary_key",
    // Phase C — INSERT ... VALUES with positional type-checking + overflow trap.
    "dml.insert",
    "error.overflow_trap",
    // Step 6 — row mutation: UPDATE (in-place) + DELETE.
    "dml.update",
    "dml.delete",
    // Phase D/E — SELECT, WHERE (=, ordering), ORDER BY, IS [NOT] NULL, 3VL, casts,
    // cross-type comparison via the promotion tower, and all three integer types.
    "query.select",
    "query.where_eq",
    "query.comparison_order",
    "query.is_null",
    "query.order_by",
    // Select-list output naming: SELECT *, AS aliases, and the ?column? rule (grammar.md §8).
    "query.select_star",
    "query.column_alias",
    "null.three_valued",
    "compare.promotion",
    "cast.explicit",
    "types.int16",
    "types.int32",
    "types.int64",
    // General expression substrate — integer arithmetic, the expression-only boolean
    // type, and the AND/OR/NOT Kleene connectives (the `expression` profile).
    "types.boolean",
    "expr.arithmetic",
    "expr.unary_minus",
    "expr.parens",
    "expr.precedence",
    "expr.comparison_value",
    "query.logical_connectives",
    "query.is_distinct_from",
    "error.division_by_zero",
    // Cost-accounting seam — the harness asserts the deterministic, cross-core-identical
    // accrued cost via the `# cost:` directive (CLAUDE.md §13).
    "resource.cost_metering",
];

/// Parse and execute one SQL statement against `db`.
pub fn execute(db: &mut Database, sql: &str) -> Result<Outcome> {
    let stmt = Parser::parse_sql(sql)?;
    db.execute_stmt(stmt)
}
