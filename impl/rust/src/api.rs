//! Host API surface for the Rust core (spec/design/api.md §1): prepare a statement, execute or
//! query it (with optional `$N` bind parameters), and iterate result rows. Thin wrappers over
//! the parser + executor — the conformance contract still binds (the executor is unchanged).

use crate::ast::Statement;
use crate::cancel::CancellationToken;
use crate::cursor::Cursor;
use crate::error::{EngineError, Result, SqlState};
use crate::executor::{Engine, Outcome};
use crate::parser::Parser;
use crate::value::Value;

/// A parsed, reusable statement (spec/design/api.md §2.4). It owns only the parsed AST — the
/// database is supplied at execute/query time, so a `PreparedStatement` never holds a `Engine`
/// borrow (sidestepping the `&Engine` / `&mut Engine` aliasing problem; api.md §6).
pub struct PreparedStatement {
    ast: Statement,
}

impl PreparedStatement {
    /// The parsed statement. The public prepared-execution path is on
    /// [`Database`](crate::Database) / [`Session`](crate::Session)
    /// (`prepare` + `execute_prepared` / `query_prepared`), which dispatch this AST through the
    /// session's lazy-gate lifecycle (spec/design/session.md §2.4); this accessor lets that layer
    /// reach the AST.
    pub(crate) fn ast(&self) -> &Statement {
        &self.ast
    }

    /// Run this statement against a low-level [`Engine`] handle, binding `params` to its `$N`
    /// placeholders. Internal: the public path is [`Database::execute_prepared`](crate::Database::execute_prepared).
    pub(crate) fn execute(&self, db: &mut Engine, params: &[Value]) -> Result<Outcome> {
        db.execute_stmt_params(self.ast.clone(), params)
    }

    /// Run this **query** statement against a low-level [`Engine`] handle. Internal: the public
    /// path is [`Database::query_prepared`](crate::Database::query_prepared).
    pub(crate) fn query(&self, db: &mut Engine, params: &[Value]) -> Result<Rows> {
        Rows::from_outcome(self.execute(db, params)?)
    }
}

/// A cursor over a query's rows (spec/design/api.md §4). It is a thin wrapper over a
/// [`Cursor`](crate::cursor::Cursor) pull source and exposes the column names and the accrued
/// execution cost. The cursor is `Buffered` for the materialized `execute()` path (the conformance
/// corpus, byte-unchanged) and `Streaming` (S3, streaming.md §4) for a single-table no-blocking-op
/// `query()` — a lazy pull pipeline that yields one row at a time over a pinned snapshot. The seam
/// is the same either way, so callers are unchanged.
pub struct Rows {
    column_names: Vec<String>,
    cursor: Cursor,
    /// A mid-drain error from a streaming cursor (a `54P01` cost abort, `57014` cancellation, or an
    /// arithmetic trap). The `Iterator` yields `Option`, so an error mid-drain stops iteration and is
    /// stashed here; [`Rows::error`] / the ergonomic collectors surface it after draining
    /// (streaming.md §6). Always `None` for the buffered cursor (its work is already done).
    error: Option<EngineError>,
    /// The reader-liveness pin (the watermark registration, spec/design/streaming.md §5), released on
    /// `close`/`Drop`. Opaque (`Box<dyn Any>`) so `api.rs` stays free of the shared-core type; its
    /// `Drop` deregisters. `None` for a buffered cursor or a bare single-handle stream (no shared core
    /// to register against).
    _pin: Option<Box<dyn std::any::Any>>,
}

impl Rows {
    pub(crate) fn from_outcome(outcome: Outcome) -> Result<Rows> {
        match outcome {
            Outcome::Query {
                column_names,
                rows,
                cost,
                ..
            } => Ok(Rows {
                column_names,
                cursor: Cursor::buffered(rows, cost),
                error: None,
                _pin: None,
            }),
            Outcome::Statement { .. } => Err(EngineError::new(
                SqlState::SyntaxError,
                "query() called on a statement that produces no rows; use execute()",
            )),
        }
    }

    /// Wrap a lazy streaming pull source as a `Rows` cursor (S3, spec/design/streaming.md §4).
    pub(crate) fn from_streaming(
        column_names: Vec<String>,
        source: Box<dyn crate::cursor::RowStream>,
    ) -> Rows {
        Rows {
            column_names,
            cursor: Cursor::streaming(source),
            error: None,
            _pin: None,
        }
    }

    /// Attach the reader-liveness pin (the watermark registration, streaming.md §5) — the shared core
    /// registers the snapshot version and hands the deregistering guard here, so it is released when
    /// the cursor is closed or dropped.
    pub(crate) fn attach_pin(&mut self, pin: Box<dyn std::any::Any>) {
        self._pin = Some(pin);
    }

    /// The output column names of the query result.
    pub fn column_names(&self) -> &[String] {
        &self.column_names
    }

    /// The deterministic execution cost accrued by the query (CLAUDE.md §13). Final once the
    /// cursor is drained (streaming.md §6); for a buffered cursor it is final immediately.
    pub fn cost(&self) -> i64 {
        self.cursor.cost()
    }

    /// Surface a mid-drain streaming error (streaming.md §6): `Err` if iteration stopped on an error,
    /// else `Ok`. The `Iterator` impl yields `Option`, so a streaming consumer that needs the error
    /// (the ergonomic collectors do) calls this after draining. Always `Ok` for a buffered cursor.
    pub fn error(&mut self) -> Result<()> {
        match self.error.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    /// Release the read snapshot the cursor pins (spec/design/streaming.md §5): drops the watermark
    /// pin (deregistering, advancing the reclamation watermark) and marks the cursor closed.
    /// Idempotent; a no-op for a buffered cursor (it pins nothing). Draining to exhaustion and `Drop`
    /// are the other release paths.
    pub fn close(&mut self) {
        self.cursor.close();
        self._pin = None;
    }
}

impl Iterator for Rows {
    type Item = Vec<Value>;

    fn next(&mut self) -> Option<Vec<Value>> {
        if self.error.is_some() {
            return None;
        }
        match self.cursor.next_row() {
            Ok(row) => row,
            Err(e) => {
                // Stash the mid-drain error (streaming.md §6) and end iteration; `error()` surfaces it.
                self.error = Some(e);
                None
            }
        }
    }
}

/// An open explicit transaction (spec/design/api.md §2.2, transactions.md §4.4). Borrows the
/// `Engine` for the transaction's life; statements run through `execute`/`query`, and
/// `commit`/`rollback` end it. Dropping it without an explicit end **rolls back** — an unfinished
/// transaction never silently commits its work (the bbolt safety net).
pub struct Transaction<'a> {
    db: &'a mut Engine,
    done: bool,
}

impl<'a> Transaction<'a> {
    /// Construct a transaction handle that BORROWS `db` for a [`Session`](crate::Session)-driven
    /// block (spec/design/session.md §2.4). `done` is preset **true** so its `Drop` does **not**
    /// roll back — the owning `Session` ends the block (committing through the shared core / releasing
    /// the writer gate / publishing). The closure runs only `execute`/`query` against it.
    pub(crate) fn borrow(db: &'a mut Engine) -> Transaction<'a> {
        Transaction { db, done: true }
    }
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

    /// Run a statement within this transaction under a [`CancellationToken`] (spec/design/api.md
    /// §11.4): arm `cancel` for the statement's duration so a flipped token (from any thread) aborts it
    /// `57014` at the next cost-meter checkpoint — which, like any error, poisons the block (`25P02`).
    /// The prior token is restored on return.
    pub fn execute_cancelable(
        &mut self,
        sql: &str,
        params: &[Value],
        cancel: &CancellationToken,
    ) -> Result<Outcome> {
        cancel.check()?;
        let prev = self.db.session.cancel.replace(cancel.clone());
        let r = self.db.execute(sql, params);
        self.db.session.cancel = prev;
        r
    }

    /// Run a query within this transaction under a [`CancellationToken`] (spec/design/api.md §11.4) —
    /// the query sibling of [`execute_cancelable`](Transaction::execute_cancelable).
    pub fn query_cancelable(
        &mut self,
        sql: &str,
        params: &[Value],
        cancel: &CancellationToken,
    ) -> Result<Rows> {
        cancel.check()?;
        let prev = self.db.session.cancel.replace(cancel.clone());
        let r = self.db.query(sql, params);
        self.db.session.cancel = prev;
        r
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

impl Engine {
    /// Open an explicit transaction (spec/design/api.md §2.2). `writable` false is READ ONLY (a
    /// write inside → `25006`); true is READ WRITE — `25006` on a read-only handle (§2.1). A
    /// nested `begin` (a transaction is already open) is `25001`. Prefer `view`/`update`, which
    /// cannot forget to end the transaction.
    pub fn begin(&mut self, writable: bool) -> Result<Transaction<'_>> {
        self.begin_tx(Some(writable))?;
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

    /// Parse one statement from `sql`, first enforcing this handle's `max_sql_length` input-size
    /// limit (CLAUDE.md §13; spec/design/api.md §8, cost.md §7). The §13 input-size gate: an
    /// over-limit statement is rejected with `54000` **before** lexing, so unbounded untrusted
    /// input cannot exhaust parse memory/CPU (the cost meter cannot catch this — parsing precedes
    /// metering). `max_sql_length == 0` is unlimited. **Every** handle-bound parse path routes
    /// through here (`execute`/`execute_params`/`prepare`/the read handle), so the per-handle
    /// limit has no hole. The byte length is `sql.len()` (Rust `&str` is UTF-8).
    pub(crate) fn parse(&self, sql: &str) -> Result<Statement> {
        let max = self.session.max_sql_length;
        if max > 0 && sql.len() > max {
            return Err(EngineError::new(
                SqlState::ProgramLimitExceeded,
                format!("SQL statement exceeds the maximum length of {max} bytes"),
            ));
        }
        Parser::parse_sql(sql)
    }

    /// Parse `sql` once into a reusable prepared statement (spec/design/api.md §2.4). Parse
    /// errors (`42601`, …) and the `54000` input-size limit surface here.
    pub fn prepare(&self, sql: &str) -> Result<PreparedStatement> {
        Ok(PreparedStatement {
            ast: self.parse(sql)?,
        })
    }

    /// One-shot: parse + execute `sql`, binding `params` to its `$N` placeholders, returning the
    /// materialized outcome. (The free function `jed::execute(db, sql)` is the zero-parameter
    /// convenience kept for back-compat.)
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<Outcome> {
        let ast = self.parse(sql)?;
        self.execute_stmt_params(ast, params)
    }

    /// One-shot: parse + run a **query** `sql`, binding `params`, returning a row cursor. A
    /// single-table no-blocking-operator read is served by a **lazy streaming** cursor
    /// (spec/design/streaming.md §4, S3); a blocking read (`ORDER BY`/`DISTINCT`/aggregate/window/
    /// join) is served by a **lazy buffered** cursor (S4) that buffers the input but yields the output
    /// one row at a time. Both pull over a pinned snapshot with bounded peak *output* memory and a
    /// caller early-exit; a set-operation / `WITH` top level falls back to the materialized `execute()`
    /// path. (This is the bare single-handle [`Engine`]; the watermark pin lives on the shared-core
    /// [`Session`](crate::Session) path.)
    pub fn query(&mut self, sql: &str, params: &[Value]) -> Result<Rows> {
        let ast = self.parse(sql)?;
        if let Some(rows) = self.try_streaming_query(&ast, params)? {
            return Ok(rows);
        }
        if let Some(rows) = self.try_buffered_query(&ast, params)? {
            return Ok(rows);
        }
        Rows::from_outcome(self.execute_stmt_params(ast, params)?)
    }

    /// Run a multi-statement `sql` **script** on the default session (spec/design/session.md §4.2):
    /// split it with [`split_statements`](crate::split::split_statements), run each statement in
    /// order, **discard the result rows** (keeping only counts), and return the `O(1)`
    /// [`ScriptSummary`] (`statements_run` / `rows_affected_total` / `cost`). The dominant
    /// migration/import path — "run this script; I only care that it succeeded."
    ///
    /// - **`Idle` at entry** ⇒ the whole run is **one implicit transaction**, all-or-nothing: a
    ///   statement error rolls the wrapper back (nothing is committed) and returns that error.
    /// - **`Open` at entry** ⇒ the run **joins** that transaction (no wrapper, no auto-commit); a
    ///   mid-run error leaves the block `Failed` for the caller to roll back.
    /// - **In-script transaction control** (`BEGIN`/`COMMIT`/`ROLLBACK`) is **`0A000`** — the
    ///   implicit wrapper owns the boundary (partitioning is deferred, session.md §11). A host that
    ///   needs self-managed transactions writes its own `split_statements` loop instead.
    pub fn execute_script(&mut self, sql: &str) -> Result<crate::executor::ScriptSummary> {
        // We own an implicit wrapper iff the session is Idle at entry. `begin_tx(None)` honors the
        // handle's read-only mode (READ ONLY wrapper on a read-only handle — a write inside is
        // 25006, exactly like autocommit).
        let owns_wrapper = !self.in_transaction();
        if owns_wrapper {
            self.begin_tx(None)?;
        }
        match self.run_script_body(sql) {
            Ok(summary) => {
                if owns_wrapper {
                    self.commit()?; // publish the all-or-nothing run
                }
                Ok(summary)
            }
            Err(e) => {
                if owns_wrapper {
                    let _ = self.rollback(); // discard everything; surface the original error
                }
                Err(e)
            }
        }
    }

    /// The body of [`execute_script`](Engine::execute_script): split, then run each statement on
    /// the current transaction. Separated so the wrapper's commit/rollback in `execute_script` runs
    /// on either the `Ok` or the `Err` path with no duplication — and so [`Session::execute_script`]
    /// (crate::Session) can reuse it under a shared-core-aware wrapper (session.md §2.4).
    pub(crate) fn run_script_body(&mut self, sql: &str) -> Result<crate::executor::ScriptSummary> {
        use crate::ast::Statement;
        let mut summary = crate::executor::ScriptSummary::default();
        for span in crate::split::split_statements(sql) {
            let ast = self.parse(span.text())?;
            // Transaction control inside a script is the v1 narrowing (session.md §4.2): the implicit
            // wrapper owns the boundary, so BEGIN/COMMIT/ROLLBACK is 0A000 (partitioning deferred).
            if matches!(
                ast,
                Statement::Begin { .. } | Statement::Commit | Statement::Rollback
            ) {
                return Err(EngineError::new(
                    SqlState::FeatureNotSupported,
                    "transaction control (BEGIN/COMMIT/ROLLBACK) is not supported inside execute_script; \
                     use split_statements to run a self-managed multi-statement transaction",
                ));
            }
            let outcome = self.execute_stmt_params(ast, &[])?;
            summary.statements_run += 1;
            if let Outcome::Statement {
                rows_affected: Some(n),
                ..
            } = &outcome
            {
                summary.rows_affected_total += *n;
            }
            summary.cost += outcome.cost();
        }
        Ok(summary)
    }
}
