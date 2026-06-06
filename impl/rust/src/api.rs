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

/// An open explicit transaction (spec/design/api.md §2.2, transactions.md §4.4). Borrows the
/// `Database` for the transaction's life; statements run through `execute`/`query`, and
/// `commit`/`rollback` end it. Dropping it without an explicit end **rolls back** — an unfinished
/// transaction never silently commits its work (the bbolt safety net).
pub struct Transaction<'a> {
    db: &'a mut Database,
    done: bool,
}

impl Transaction<'_> {
    /// Run a (possibly mutating) statement within this transaction, binding `params`. A write in
    /// a READ ONLY transaction is `25006`; a statement error aborts the block (every later
    /// statement but commit/rollback is then `25P02`).
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<Outcome> {
        self.db.execute(sql, params)
    }

    /// Run a query within this transaction, returning a row cursor.
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<Rows> {
        self.db.query(sql, params)
    }

    /// Commit the transaction — publish + make durable (per `synchronous`). Consumes the handle.
    pub fn commit(mut self) -> Result<()> {
        self.done = true;
        self.db.commit()
    }

    /// Roll back the transaction — discard its working set. Consumes the handle.
    pub fn rollback(mut self) -> Result<()> {
        self.done = true;
        self.db.rollback()
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.done {
            // An un-ended transaction rolls back — durability is never implicit (bbolt's rule).
            let _ = self.db.rollback();
        }
    }
}

impl Database {
    /// Open an explicit transaction (spec/design/api.md §2.2). `writable` false is READ ONLY (a
    /// write inside → `25006`); true is READ WRITE. A nested `begin` (a transaction is already
    /// open) is `25001`. Prefer `view`/`update`, which cannot forget to end the transaction.
    pub fn begin(&mut self, writable: bool) -> Result<Transaction<'_>> {
        self.begin_tx(writable)?;
        Ok(Transaction {
            db: self,
            done: false,
        })
    }

    /// Run `f` in a READ ONLY transaction (bbolt-style): open it, run `f(tx)`, then auto-commit on
    /// success / auto-rollback on error or panic. A write inside is `25006`.
    pub fn view<R>(&mut self, f: impl FnOnce(&mut Transaction) -> Result<R>) -> Result<R> {
        self.with_tx(false, f)
    }

    /// Run `f` in a READ WRITE transaction (bbolt-style): open it, run `f(tx)`, then auto-commit on
    /// success / auto-rollback on error or panic — the safe default over a raw `begin`.
    pub fn update<R>(&mut self, f: impl FnOnce(&mut Transaction) -> Result<R>) -> Result<R> {
        self.with_tx(true, f)
    }

    fn with_tx<R>(
        &mut self,
        writable: bool,
        f: impl FnOnce(&mut Transaction) -> Result<R>,
    ) -> Result<R> {
        let mut tx = self.begin(writable)?;
        match f(&mut tx) {
            Ok(r) => {
                tx.commit()?;
                Ok(r)
            }
            Err(e) => {
                // Roll back the failed transaction (a panic is handled by `Drop`); surface the
                // original error, not the rollback's result.
                let _ = tx.rollback();
                Err(e)
            }
        }
    }

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
