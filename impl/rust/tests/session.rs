//! Session surface (spec/design/session.md §2): the `Engine`-owned **stateful default session**
//! (the bare single-handle path), and — after the §2.4 convergence — **additional** sessions minted
//! by `db.session(opts)` over a shared [`Database`] core (each owns its private `Engine`, shares
//! committed storage through the core, carries an independent envelope, and runs autocommit with the
//! lazy gate — no swap). Also the explicit `Idle`/`Open`/`Failed` transaction state machine. These
//! are per-core API behaviors the shared corpus cannot express (it is single-handle SQL-in/rows-out
//! — CLAUDE.md §10), so they live here.

use jed::value::Value;
use jed::{Database, Engine, Outcome, SessionOptions, TxStatus};

fn rows(o: Outcome) -> Vec<Vec<Value>> {
    match o {
        Outcome::Query { rows, .. } => rows,
        other => panic!("expected a query, got {other:?}"),
    }
}

#[test]
fn default_session_is_stateful_across_calls() {
    // The Engine-owned default session holds an open BEGIN block across *separate* calls
    // (the PG/SQLite connection model, §2.1), and `db.status()` exposes the explicit state machine.
    let mut db = Engine::new();
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
    let mut db = Engine::new();
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
    // Two sessions over one shared Database core: each owns its private Engine, but committed storage
    // is shared through the core (§2.4) — no swap. Settings (the cost ceiling) are independent.
    let db = Database::new_in_memory();
    let mut a = db.session(SessionOptions::default());
    a.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    a.execute("INSERT INTO t VALUES (1, 10)", &[]).unwrap();

    // A second session with its own cost ceiling — `a`'s is untouched.
    let mut s = db.session(SessionOptions {
        max_cost: 5,
        ..SessionOptions::default()
    });
    assert_eq!(s.max_cost(), 5);
    assert_eq!(a.max_cost(), 0);

    // It sees `a`'s committed data (committed storage is shared via the core).
    assert_eq!(
        rows(s.execute("SELECT id, v FROM t", &[]).unwrap()),
        vec![vec![Value::Int(1), Value::Int(10)]]
    );

    // A write through the second session (autocommit, lazy gate) is visible to `a`'s next read.
    s.execute("INSERT INTO t VALUES (2, 20)", &[]).unwrap();
    assert_eq!(
        rows(a.execute("SELECT id FROM t ORDER BY id", &[]).unwrap()),
        vec![vec![Value::Int(1)], vec![Value::Int(2)]]
    );

    // Each session keeps its own state/settings: `a` is still Idle and unlimited.
    assert_eq!(a.status(), TxStatus::Idle);
    assert_eq!(a.max_cost(), 0);
}

#[test]
fn additional_session_cost_ceiling_is_enforced() {
    // The session's *settings* drive the execution path: a tiny ceiling aborts the scan with 54P01,
    // while an unlimited session runs it fine — both over the same shared core.
    let db = Database::new_in_memory();
    let mut a = db.session(SessionOptions::default());
    a.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    a.execute("INSERT INTO t VALUES (1), (2), (3)", &[])
        .unwrap();

    a.execute("SELECT * FROM t", &[]).unwrap(); // unlimited

    let mut s = db.session(SessionOptions {
        max_cost: 1,
        ..SessionOptions::default()
    });
    assert_eq!(
        s.execute("SELECT * FROM t", &[]).err().unwrap().code(),
        "54P01"
    );

    // The unlimited session is unaffected.
    a.execute("SELECT * FROM t", &[]).unwrap();
    assert_eq!(a.max_cost(), 0);
}

#[test]
fn additional_session_update_closure_commits_to_shared_storage() {
    let db = Database::new_in_memory();
    let mut a = db.session(SessionOptions::default());
    a.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();

    let mut s = db.session(SessionOptions::default());
    s.update(|tx| {
        tx.execute("INSERT INTO t VALUES (1)", &[])?;
        tx.execute("INSERT INTO t VALUES (2)", &[])?;
        Ok(())
    })
    .unwrap();

    // The update closure committed through the shared core; another session sees both rows.
    assert_eq!(
        rows(a.execute("SELECT count(*) FROM t", &[]).unwrap()),
        vec![vec![Value::Int(2)]]
    );
    assert_eq!(a.status(), TxStatus::Idle);
}
