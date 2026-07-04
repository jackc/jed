//! The shared execution core (spec/design/cli.md): script mode and the TUI both drive a
//! `Session`, so their behavior cannot drift. A CLI session wraps one engine [`jed::Session`]
//! handle and tracks the one piece of state it does not expose directly — whether the open
//! transaction has FAILED (a statement errored inside it, so everything but
//! COMMIT/ROLLBACK now answers 25P02).

use jed::{EngineError, Value};

/// One statement's result, shaped for display.
#[derive(Debug)]
pub enum ExecOutput {
    /// A statement with no result set. `tag` is `OK`, or the bare transaction-control
    /// word (`BEGIN`/`COMMIT`/`ROLLBACK`), which prints without a cost (cli.md §5).
    /// `rows` is the engine's affected-row count — present for DML without RETURNING,
    /// absent for DDL and transaction control (api.md §4).
    Statement {
        tag: &'static str,
        cost: i64,
        rows: Option<i64>,
    },
    Query {
        columns: Vec<String>,
        rows: Vec<Vec<Value>>,
        cost: i64,
    },
}

/// The display line for a no-result-set statement (cli.md §5): transaction-control tags
/// print bare; `OK` carries the affected-row count when the statement has one
/// (`OK, 3 rows (cost C)`), and the deterministic cost either way.
pub fn statement_line(tag: &str, cost: i64, rows: Option<i64>) -> String {
    match (tag, rows) {
        ("OK", Some(n)) => {
            let noun = if n == 1 { "row" } else { "rows" };
            format!("OK, {n} {noun} (cost {cost})")
        }
        ("OK", None) => format!("OK (cost {cost})"),
        _ => tag.to_string(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum TxState {
    Autocommit,
    Open,
    Failed,
}

pub struct Session {
    pub db: jed::Session,
    /// Display name: the file path, or "memory" for an in-memory database.
    pub source: String,
    tx_failed: bool,
}

impl Session {
    pub fn new(db: jed::Session, source: String) -> Session {
        Session {
            db,
            source,
            tx_failed: false,
        }
    }

    /// Execute one already-split statement. Errors pass through for the caller to
    /// display; transaction state is tracked either way.
    pub fn run(&mut self, sql: &str) -> Result<ExecOutput, EngineError> {
        let result = self.exec_output(sql);
        // Failed-transaction tracking (cli.md §6): an error while a transaction is open
        // poisons it (the engine answers 25P02 from then on); any path that leaves the
        // transaction (COMMIT/ROLLBACK, or autocommit all along) clears the flag.
        if !self.db.in_transaction() {
            self.tx_failed = false;
        } else if result.is_err() {
            self.tx_failed = true;
        }
        result
    }

    /// Run one statement through the one total `query` seam (spec/design/api.md §11) and materialize
    /// the cursor into the CLI's render shape: a cursor with output columns is a query; a no-column
    /// cursor IS a bare statement carrying its command tag. A **mid-drain** streaming error (a `54P01`
    /// cost abort, `57014` cancellation, or arithmetic trap) surfaces here via [`jed::Rows::error`].
    fn exec_output(&mut self, sql: &str) -> Result<ExecOutput, EngineError> {
        let mut rows = self.db.query(sql, &[])?;
        let columns = rows.column_names().to_vec();
        let drained: Vec<Vec<Value>> = rows.by_ref().collect();
        rows.error()?;
        let cost = rows.cost();
        if columns.is_empty() {
            Ok(ExecOutput::Statement {
                tag: statement_tag(sql),
                cost,
                rows: rows.rows_affected(),
            })
        } else {
            Ok(ExecOutput::Query {
                columns,
                rows: drained,
                cost,
            })
        }
    }

    pub fn tx_state(&self) -> TxState {
        if !self.db.in_transaction() {
            TxState::Autocommit
        } else if self.tx_failed {
            TxState::Failed
        } else {
            TxState::Open
        }
    }
}

/// The display tag for a no-result-set statement: the bare word for transaction
/// control, `OK` for everything else. Leading whitespace and comments are skipped so
/// `/* c */ BEGIN` still tags as BEGIN.
fn statement_tag(sql: &str) -> &'static str {
    let word = first_word(sql);
    match word.to_ascii_lowercase().as_str() {
        "begin" => "BEGIN",
        "commit" => "COMMIT",
        "rollback" => "ROLLBACK",
        _ => "OK",
    }
}

/// The first keyword of a statement, skipping whitespace and comments (the same lexical
/// rules as the engine — grammar.md §33).
fn first_word(sql: &str) -> &str {
    let b = sql.as_bytes();
    let mut i = 0;
    loop {
        if i >= b.len() {
            return "";
        }
        if b[i].is_ascii_whitespace() {
            i += 1;
        } else if b[i] == b'-' && b.get(i + 1) == Some(&b'-') {
            while i < b.len() && b[i] != b'\n' && b[i] != b'\r' {
                i += 1;
            }
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            let mut depth = 1;
            i += 2;
            while depth > 0 && i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        } else {
            break;
        }
    }
    let start = i;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
        i += 1;
    }
    &sql[start..i]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Session {
        Session::new(
            jed::Database::create(jed::CreateOptions::default())
                .expect("in-memory create is infallible")
                .session(jed::SessionOptions::default()),
            "memory".to_string(),
        )
    }

    #[test]
    fn tags_and_tx_state() {
        let mut s = mem();
        assert!(matches!(s.tx_state(), TxState::Autocommit));
        match s.run("CREATE TABLE t (id i32 PRIMARY KEY)").unwrap() {
            ExecOutput::Statement { tag, rows, .. } => {
                assert_eq!(tag, "OK");
                assert_eq!(rows, None);
            }
            _ => panic!("expected statement"),
        }
        match s.run("/* c */ BEGIN").unwrap() {
            ExecOutput::Statement { tag, .. } => assert_eq!(tag, "BEGIN"),
            _ => panic!("expected statement"),
        }
        assert!(matches!(s.tx_state(), TxState::Open));

        // An error inside the open transaction poisons it.
        assert_eq!(
            s.run("INSERT INTO t VALUES (1), (1)").unwrap_err().code(),
            "23505"
        );
        assert!(matches!(s.tx_state(), TxState::Failed));
        // Everything but COMMIT/ROLLBACK now answers 25P02 and the state stays failed.
        assert_eq!(s.run("SELECT id FROM t").unwrap_err().code(), "25P02");
        assert!(matches!(s.tx_state(), TxState::Failed));
        // ROLLBACK ends the block and clears the flag.
        match s.run("ROLLBACK").unwrap() {
            ExecOutput::Statement { tag, .. } => assert_eq!(tag, "ROLLBACK"),
            _ => panic!("expected statement"),
        }
        assert!(matches!(s.tx_state(), TxState::Autocommit));

        // An autocommit error never leaves a failed state behind.
        assert_eq!(s.run("SELECT nope FROM t").unwrap_err().code(), "42703");
        assert!(matches!(s.tx_state(), TxState::Autocommit));
    }

    #[test]
    fn statement_line_formats_tags_counts_and_costs() {
        assert_eq!(statement_line("OK", 5, None), "OK (cost 5)");
        assert_eq!(statement_line("OK", 5, Some(3)), "OK, 3 rows (cost 5)");
        assert_eq!(statement_line("OK", 0, Some(1)), "OK, 1 row (cost 0)");
        assert_eq!(statement_line("OK", 2, Some(0)), "OK, 0 rows (cost 2)");
        assert_eq!(statement_line("BEGIN", 0, None), "BEGIN");
    }

    #[test]
    fn dml_statements_carry_affected_rows() {
        let mut s = mem();
        s.run("CREATE TABLE t (id i32 PRIMARY KEY)").unwrap();
        match s.run("INSERT INTO t VALUES (1), (2), (3)").unwrap() {
            ExecOutput::Statement { tag, rows, .. } => {
                assert_eq!(tag, "OK");
                assert_eq!(rows, Some(3));
            }
            _ => panic!("expected statement"),
        }
        match s.run("DELETE FROM t WHERE id > 1").unwrap() {
            ExecOutput::Statement { rows, .. } => assert_eq!(rows, Some(2)),
            _ => panic!("expected statement"),
        }
    }

    #[test]
    fn query_output_carries_columns_rows_cost() {
        let mut s = mem();
        s.run("CREATE TABLE t (id i32 PRIMARY KEY, v i32)").unwrap();
        s.run("INSERT INTO t VALUES (1, 10)").unwrap();
        match s.run("SELECT id, v FROM t ORDER BY id").unwrap() {
            ExecOutput::Query {
                columns,
                rows,
                cost,
            } => {
                assert_eq!(columns, vec!["id", "v"]);
                assert_eq!(rows, vec![vec![Value::Int(1), Value::Int(10)]]);
                assert!(cost > 0);
            }
            _ => panic!("expected query"),
        }
    }
}
