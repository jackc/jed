//! Host-API behaviors of the `view`/`update` closure, failed-block poisoning at the parse stage, and
//! the never-waiting host operations (spec/design/api.md §2.2/§2.3, transactions.md §6) — per-core
//! because the corpus is single-handle SQL-in/rows-out (CLAUDE.md §10). Mirrors
//! impl/go/closure_txn_test.go and impl/ts/tests/closure_txn.test.ts.
//!
//! - Transaction control inside a closure is `2D000` (invalid_transaction_termination): SQL
//!   `COMMIT`/`ROLLBACK` through the closure's `Transaction` aborts the block (PostgreSQL's rule for a
//!   procedure called within a block), and the handle's own `commit`/`rollback` is refused without
//!   aborting. The closure's writes are never silently discarded: a closure that swallows the error
//!   gets `25P02` from `update`, not an `Ok` that committed nothing.
//! - A parse error (`42601`), the `54000` input-size limit, and a `prepare` parse error inside an
//!   open block poison it like an execution error; `BEGIN` in a failed block is `25P02`.
//! - `close` / `attach` / `detach` never wait for the writer gate: `55006` while a write block is open
//!   on this thread (which waiting would deadlock), and fine once it ends.

use jed::value::Value;
use jed::{AttachSource, CreateOptions, Database, Outcome, Session, SessionOptions};

fn mem() -> Database {
    Database::create(CreateOptions::default()).unwrap()
}

fn count(s: &mut Session, table: &str) -> usize {
    match s
        .query_outcome(&format!("SELECT * FROM {table}"), &[])
        .unwrap()
    {
        Outcome::Query { rows, .. } => rows.len(),
        _ => panic!("expected a query result"),
    }
}

fn code<T>(r: Result<T, jed::EngineError>) -> String {
    match r {
        Ok(_) => panic!("expected an error"),
        Err(e) => e.code().to_string(),
    }
}

#[test]
fn update_closure_sql_commit_is_2d000_and_never_discards_writes() {
    let db = mem();
    let mut s = db.session(SessionOptions::default());
    s.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();

    // The closure propagates the 2D000: update rolls back and reports it.
    let err: Result<(), _> = s.update(|tx| {
        tx.query_outcome("INSERT INTO t VALUES (1)", &[])?;
        let e = tx.query_outcome("COMMIT", &[]).unwrap_err();
        assert_eq!(e.code(), "2D000");
        // the block is failed: a later statement is 25P02
        assert_eq!(
            code(tx.query_outcome("INSERT INTO t VALUES (5)", &[])),
            "25P02"
        );
        Err(e)
    });
    assert_eq!(code(err), "2D000");
    assert!(!s.in_transaction());
    assert_eq!(count(&mut s, "t"), 0);

    // The closure swallows the error and returns Ok: update still reports that nothing committed.
    let err = s.update(|tx| {
        let _ = tx.query_outcome("INSERT INTO t VALUES (1)", &[]);
        let _ = tx.query_outcome("ROLLBACK", &[]);
        let _ = tx.query_outcome("INSERT INTO t VALUES (5)", &[]);
        Ok(())
    });
    assert_eq!(code(err), "25P02");
    assert_eq!(count(&mut s, "t"), 0);

    // The gate was released: this session and another both write again.
    s.query_outcome("INSERT INTO t VALUES (2)", &[]).unwrap();
    let mut other = db.session(SessionOptions::default());
    other
        .query_outcome("INSERT INTO t VALUES (3)", &[])
        .unwrap();
    assert_eq!(count(&mut other, "t"), 2);
}

#[test]
fn closure_handle_commit_rollback_is_2d000_without_aborting() {
    let db = mem();
    let mut s = db.session(SessionOptions::default());
    s.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    s.update(|tx| {
        tx.query_outcome("INSERT INTO t VALUES (1)", &[])?;
        // `Transaction::commit`/`rollback` consume the handle, so the closure cannot call them on its
        // `&mut Transaction` — the 2D000 of the SQL forms (above) is the Rust-reachable case; the
        // handle-method refusal is pinned where the handle is owned (Go/TS).
        tx.query_outcome("INSERT INTO t VALUES (2)", &[])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(count(&mut s, "t"), 2);

    let err = s.view(|tx| {
        assert_eq!(code(tx.query_outcome("COMMIT", &[])), "2D000");
        Ok(())
    });
    assert_eq!(code(err), "25P02");
}

#[test]
fn savepoints_work_inside_update_closure() {
    let db = mem();
    let mut s = db.session(SessionOptions::default());
    s.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    s.update(|tx| {
        for sql in [
            "INSERT INTO t VALUES (1)",
            "SAVEPOINT a",
            "INSERT INTO t VALUES (2)",
            "ROLLBACK TO a",
            "INSERT INTO t VALUES (3)",
            "RELEASE a",
        ] {
            tx.query_outcome(sql, &[])?;
        }
        // a nested BEGIN stays 25001 and does not abort
        assert_eq!(code(tx.query_outcome("BEGIN", &[])), "25001");
        Ok(())
    })
    .unwrap();
    match s
        .query_outcome("SELECT id FROM t ORDER BY id", &[])
        .unwrap()
    {
        Outcome::Query { rows, .. } => {
            assert_eq!(rows, vec![vec![Value::Int(1)], vec![Value::Int(3)]]);
        }
        _ => panic!("expected a query result"),
    }
}

#[test]
fn parse_stage_errors_poison_an_open_block() {
    let db = mem();
    let mut s = db.session(SessionOptions::default());
    s.set_max_sql_length(64);
    s.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    fn expect_poisoned(s: &mut Session, what: &str) {
        assert_eq!(
            code(s.query_outcome("SELECT id FROM t", &[])),
            "25P02",
            "after {what}"
        );
        assert_eq!(
            code(s.query_outcome("BEGIN", &[])),
            "25P02",
            "BEGIN after {what}"
        );
        s.query_outcome("ROLLBACK", &[]).unwrap();
        s.query_outcome("SELECT id FROM t", &[]).unwrap();
    }

    s.query_outcome("BEGIN", &[]).unwrap();
    assert_eq!(code(s.query_outcome("SELECT 1 FROM", &[])), "42601");
    expect_poisoned(&mut s, "a 42601");

    s.query_outcome("BEGIN", &[]).unwrap();
    let long = format!("SELECT '{}'", "x".repeat(80));
    assert_eq!(code(s.query_outcome(&long, &[])), "54000");
    expect_poisoned(&mut s, "a 54000");

    s.query_outcome("BEGIN", &[]).unwrap();
    assert_eq!(code(s.prepare("SELEC 1")), "42601");
    expect_poisoned(&mut s, "a prepare 42601");

    // the closure's Transaction too
    let err = s.update(|tx| {
        assert_eq!(code(tx.query_outcome("SELECT 1 FROM", &[])), "42601");
        assert_eq!(code(tx.query_outcome("SELECT id FROM t", &[])), "25P02");
        Ok(())
    });
    assert_eq!(code(err), "25P02");
    assert!(!s.in_transaction());
}

#[test]
fn close_attach_detach_never_wait_for_an_open_write_block() {
    let db = mem();
    db.attach("aux", AttachSource::memory(), false).unwrap();
    let mut s = db.session(SessionOptions::default());
    s.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    s.begin(true).unwrap();
    s.query_outcome("INSERT INTO t VALUES (1)", &[]).unwrap();
    // Each would deadlock on this thread if it waited for the gate the open block holds.
    // `close(self)` consumes the handle either way; a clone keeps this test's handle alive.
    assert_eq!(code(db.clone().close()), "55006");
    assert_eq!(
        code(db.attach("more", AttachSource::memory(), false)),
        "55006"
    );
    assert_eq!(code(db.detach("aux")), "55006");
    // The refusals disturbed nothing: the block commits and the operations then succeed.
    s.commit().unwrap();
    assert_eq!(count(&mut s, "t"), 1);
    db.attach("more", AttachSource::memory(), false).unwrap();
    db.detach("aux").unwrap();
    drop(s);
    db.close().unwrap();
}
