//! S1 session surface (spec/design/session.md §2): the `Database`-owned **stateful default
//! session**, **additional** sessions minted by `db.session(opts)` (shared committed storage,
//! independent settings + transaction state, run sequentially via the swap), the relocated
//! settings, and the explicit `Idle`/`Open`/`Failed` transaction state machine. These are per-core
//! API behaviors the shared corpus cannot express (it is single-handle SQL-in/rows-out — CLAUDE.md
//! §10), so they live here.

use jed::value::Value;
use jed::{Database, Outcome, SessionOptions, TxStatus};

fn rows(o: Outcome) -> Vec<Vec<Value>> {
    match o {
        Outcome::Query { rows, .. } => rows,
        other => panic!("expected a query, got {other:?}"),
    }
}

#[test]
fn default_session_is_stateful_across_calls() {
    // The Database-owned default session holds an open BEGIN block across *separate* calls
    // (the PG/SQLite connection model, §2.1), and `db.status()` exposes the explicit state machine.
    let mut db = Database::new();
    assert_eq!(db.status(), TxStatus::Idle);

    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    db.execute("BEGIN", &[]).unwrap();
    assert_eq!(db.status(), TxStatus::Open);
    db.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
    assert_eq!(db.status(), TxStatus::Open); // still open across the separate call
    db.execute("COMMIT", &[]).unwrap();
    assert_eq!(db.status(), TxStatus::Idle);

    assert_eq!(
        rows(db.execute("SELECT id FROM t", &[]).unwrap()),
        vec![vec![Value::Int(1)]]
    );
}

#[test]
fn failed_block_is_the_failed_state() {
    // A statement error inside a block poisons it: status is `Failed`, every later statement but
    // ROLLBACK/COMMIT is 25P02 (§2.2 / transactions.md §6), and ROLLBACK returns to Idle.
    let mut db = Database::new();
    db.execute("BEGIN", &[]).unwrap();
    assert_eq!(
        db.execute("SELECT * FROM missing", &[])
            .err()
            .unwrap()
            .code(),
        "42P01"
    );
    assert_eq!(db.status(), TxStatus::Failed);
    assert_eq!(db.execute("SELECT 1", &[]).err().unwrap().code(), "25P02");
    db.execute("ROLLBACK", &[]).unwrap();
    assert_eq!(db.status(), TxStatus::Idle);
}

#[test]
fn additional_session_shares_storage_with_independent_settings() {
    let mut db = Database::new();
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10)", &[]).unwrap();

    // Mint a second session with its own cost ceiling — the default is untouched.
    let mut s = db.session(SessionOptions {
        max_cost: 5,
        ..SessionOptions::default()
    });
    assert_eq!(s.max_cost(), 5);
    assert_eq!(db.max_cost(), 0);

    // It sees the default session's committed data (committed storage is shared).
    assert_eq!(
        rows(s.execute(&mut db, "SELECT id, v FROM t", &[]).unwrap()),
        vec![vec![Value::Int(1), Value::Int(10)]]
    );

    // A write through the second session is visible to the default session.
    s.execute(&mut db, "INSERT INTO t VALUES (2, 20)", &[])
        .unwrap();
    assert_eq!(
        rows(db.execute("SELECT id FROM t ORDER BY id", &[]).unwrap()),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]]
    );

    // The swap restored the default session: still Idle, still unlimited.
    assert_eq!(db.status(), TxStatus::Idle);
    assert_eq!(db.max_cost(), 0);
}

#[test]
fn additional_session_cost_ceiling_is_enforced_via_swap() {
    // Proves the swap installs the additional session's *settings* into the execution path:
    // a tiny ceiling aborts the scan with 54P01, while the unlimited default runs it fine.
    let mut db = Database::new();
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1), (2), (3)", &[])
        .unwrap();

    db.execute("SELECT * FROM t", &[]).unwrap(); // default: unlimited

    let mut s = db.session(SessionOptions {
        max_cost: 1,
        ..SessionOptions::default()
    });
    assert_eq!(
        s.execute(&mut db, "SELECT * FROM t", &[])
            .err()
            .unwrap()
            .code(),
        "54P01"
    );

    // The default session is unaffected.
    db.execute("SELECT * FROM t", &[]).unwrap();
    assert_eq!(db.max_cost(), 0);
}

#[test]
fn additional_session_update_closure_commits_to_shared_storage() {
    let mut db = Database::new();
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();

    let mut s = db.session(SessionOptions::default());
    s.update(&mut db, |tx| {
        tx.execute("INSERT INTO t VALUES (1)", &[])?;
        tx.execute("INSERT INTO t VALUES (2)", &[])?;
        Ok(())
    })
    .unwrap();

    assert_eq!(
        rows(db.execute("SELECT count(*) FROM t", &[]).unwrap()),
        vec![vec![Value::Int(2)]]
    );
    assert_eq!(db.status(), TxStatus::Idle);
}
