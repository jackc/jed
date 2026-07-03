//! Phase 5 (P5.2): explicit transactions — the host `Session` transaction API (spec/design/api.md
//! §2.2 / §6, transactions.md §4.4). The SQL `BEGIN`/`COMMIT`/`ROLLBACK` surface and its visibility /
//! rollback / read-only / failed-block semantics are pinned by the shared conformance corpus
//! (suites/transactions/); these per-core tests cover the programmatic surface the corpus does
//! not exercise: `s.begin(writable)`, the `s.view`/`s.update` closure wrappers, the Drop
//! rollback safety net, and `s.commit`/`s.rollback` as the same mechanism.

use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn db_with(sql: &[&str]) -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for s in sql {
        db.execute(s, &[])
            .unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

/// Count rows of `SELECT * FROM t` against the committed/visible state.
fn count(db: &mut Session, table: &str) -> usize {
    match db.execute(&format!("SELECT * FROM {table}"), &[]).unwrap() {
        Outcome::Query { rows, .. } => rows.len(),
        _ => panic!("expected a query result"),
    }
}

#[test]
fn begin_execute_commit_is_visible() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    db.begin(true).unwrap();
    db.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
    db.execute("INSERT INTO t VALUES (2)", &[]).unwrap();
    // read-your-writes within the transaction
    match db.query("SELECT id FROM t", &[]).unwrap().count() {
        2 => {}
        n => panic!("expected 2 rows visible inside the tx, got {n}"),
    }
    db.commit().unwrap();
    assert_eq!(count(&mut db, "t"), 2);
}

#[test]
fn begin_execute_rollback_discards() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1)",
    ]);
    db.begin(true).unwrap();
    db.execute("INSERT INTO t VALUES (2)", &[]).unwrap();
    db.rollback().unwrap();
    assert_eq!(count(&mut db, "t"), 1);
}

#[test]
fn dropping_a_session_with_an_open_block_rolls_back() {
    // The bbolt safety net: an unfinished transaction never silently commits. With the Session API
    // the guard is the Session itself — dropping a session that left a block open rolls the block
    // back, so a fresh session over the same shared core sees only the pre-block committed state.
    let db = Database::create(CreateOptions::default()).unwrap();
    {
        let mut s = db.session(SessionOptions::default());
        s.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
            .unwrap();
        s.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
    }
    {
        let mut s = db.session(SessionOptions::default());
        s.begin(true).unwrap();
        s.execute("INSERT INTO t VALUES (2)", &[]).unwrap();
        // s dropped here without commit/rollback — the safety net rolls the block back
    }
    let mut s = db.session(SessionOptions::default());
    assert!(!s.in_transaction(), "no block is open on a fresh session");
    assert_eq!(count(&mut s, "t"), 1);
}

#[test]
fn update_closure_commits_on_ok() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    let n = db
        .update(|tx| {
            tx.execute("INSERT INTO t VALUES (1)", &[])?;
            tx.execute("INSERT INTO t VALUES (2)", &[])?;
            Ok(42)
        })
        .unwrap();
    assert_eq!(n, 42);
    assert!(!db.in_transaction());
    assert_eq!(count(&mut db, "t"), 2);
}

#[test]
fn update_closure_rolls_back_on_err() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1)",
    ]);
    let r: jed::Result<()> = db.update(|tx| {
        tx.execute("INSERT INTO t VALUES (2)", &[])?;
        // a duplicate key fails the closure -> the whole update auto-rolls-back
        tx.execute("INSERT INTO t VALUES (1)", &[])?;
        Ok(())
    });
    assert_eq!(r.err().unwrap().code(), "23505");
    assert!(!db.in_transaction());
    // both the failing insert AND the earlier successful one are discarded
    assert_eq!(count(&mut db, "t"), 1);
}

#[test]
fn view_is_read_only() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1), (2)",
    ]);
    // a read inside a view works and returns its value
    let n = db
        .view(|tx| Ok(tx.query("SELECT id FROM t", &[])?.count()))
        .unwrap();
    assert_eq!(n, 2);
    // a write inside a view is 25006, and the view auto-rolls-back
    let r: jed::Result<()> = db.view(|tx| {
        tx.execute("INSERT INTO t VALUES (3)", &[])?;
        Ok(())
    });
    assert_eq!(r.err().unwrap().code(), "25006");
    assert_eq!(count(&mut db, "t"), 2);
}

#[test]
fn nested_begin_is_25001() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    db.begin(true).unwrap();
    db.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
    // a SQL BEGIN inside an already-open transaction is 25001
    assert_eq!(db.execute("BEGIN", &[]).err().unwrap().code(), "25001");
    // the original block survives the rejected nested BEGIN
    db.commit().unwrap();
    assert_eq!(count(&mut db, "t"), 1);
}

#[test]
fn commit_and_rollback_are_noops_in_autocommit() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    // no open transaction: both are lenient no-op successes (transactions.md §4.2)
    db.commit().unwrap();
    db.rollback().unwrap();
    db.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
    db.rollback().unwrap(); // does not undo the autocommitted insert
    assert_eq!(count(&mut db, "t"), 1);
}
