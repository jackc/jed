//! Host API surface for the Rust core (spec/design/api.md §1): prepare a statement, execute or
//! query it (with optional `$N` bind parameters), and iterate result rows. Thin wrappers over
//! the parser + executor — the conformance contract still binds (the executor is unchanged).

use crate::ast::Statement;
use crate::error::{EngineError, Result, SqlState};
use crate::executor::{Database, Outcome};
use crate::parser::Parser;
use crate::value::Value;

/// A parsed, reusable statement (spec/design/api.md §2.4). It owns only the parsed AST — the
/// database is supplied at execute/query time, so a `PreparedStatement` never holds a `Database`
/// borrow (sidestepping the `&Database` / `&mut Database` aliasing problem; api.md §6).
pub struct PreparedStatement {
    ast: Statement,
}

impl PreparedStatement {
    /// Run this statement against `db`, binding `params` to its `$N` placeholders (empty when it
    /// has none), returning the materialized outcome.
    pub fn execute(&self, db: &mut Database, params: &[Value]) -> Result<Outcome> {
        db.execute_stmt_params(self.ast.clone(), params)
    }

    /// Run this **query** statement against `db`, returning a row cursor. A non-query statement
    /// is a `42601` (use `execute`).
    pub fn query(&self, db: &mut Database, params: &[Value]) -> Result<Rows> {
        Rows::from_outcome(self.execute(db, params)?)
    }
}

/// A cursor over a query's rows (spec/design/api.md §4). It iterates the **materialized** result
/// one row at a time (true streaming is deferred per CLAUDE.md §9 — the iterator contract is the
/// seam that lets the source become lazy later without a caller change), and exposes the column
/// names and the accrued execution cost.
pub struct Rows {
    column_names: Vec<String>,
    iter: std::vec::IntoIter<Vec<Value>>,
    cost: i64,
}

impl Rows {
    fn from_outcome(outcome: Outcome) -> Result<Rows> {
        match outcome {
            Outcome::Query {
                column_names,
                rows,
                cost,
            } => Ok(Rows {
                column_names,
                iter: rows.into_iter(),
                cost,
            }),
            Outcome::Statement { .. } => Err(EngineError::new(
                SqlState::SyntaxError,
                "query() called on a statement that produces no rows; use execute()",
            )),
        }
    }

    /// The output column names of the query result.
    pub fn column_names(&self) -> &[String] {
        &self.column_names
    }

    /// The deterministic execution cost accrued by the query (CLAUDE.md §13).
    pub fn cost(&self) -> i64 {
        self.cost
    }
}

impl Iterator for Rows {
    type Item = Vec<Value>;

    fn next(&mut self) -> Option<Vec<Value>> {
        self.iter.next()
    }
}

impl Database {
    /// Parse `sql` once into a reusable prepared statement (spec/design/api.md §2.4). Parse
    /// errors (`42601`, …) surface here.
    pub fn prepare(&self, sql: &str) -> Result<PreparedStatement> {
        Ok(PreparedStatement {
            ast: Parser::parse_sql(sql)?,
        })
    }

    /// One-shot: parse + execute `sql`, binding `params` to its `$N` placeholders, returning the
    /// materialized outcome. (The free function `jed::execute(db, sql)` is the zero-parameter
    /// convenience kept for back-compat.)
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<Outcome> {
        let ast = Parser::parse_sql(sql)?;
        self.execute_stmt_params(ast, params)
    }

    /// One-shot: parse + run a **query** `sql`, binding `params`, returning a row cursor.
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<Rows> {
        Rows::from_outcome(Database::execute(self, sql, params)?)
    }
}
