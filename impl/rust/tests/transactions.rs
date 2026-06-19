//! Phase 5 (P5.2): explicit transactions — the host `Transaction` API (spec/design/api.md §2.2 /
//! §6, transactions.md §4.4). The SQL `BEGIN`/`COMMIT`/`ROLLBACK` surface and its visibility /
//! rollback / read-only / failed-block semantics are pinned by the shared conformance corpus
//! (suites/transactions/); these per-core tests cover the programmatic surface the corpus does
//! not exercise: `db.begin(writable)`, the `db.view`/`db.update` closure wrappers, the Drop
//! rollback safety net, and `db.commit`/`db.rollback` as the same mechanism.

use jed::{Database, Outcome, execute};

fn db_with(sql: &[&str]) -> Database {
    let mut db = Database::new();
    for s in sql {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

/// Count rows of `SELECT * FROM t` against the committed/visible state.
fn count(db: &mut Database, table: &str) -> usize {
    match execute(db, &format!("SELECT * FROM {table}")).unwrap() {
        Outcome::Query { rows, .. } => rows.len(),
        _ => panic!("expected a query result"),
    }
}

#[test]
fn begin_execute_commit_is_visible() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    let mut tx = db.begin(true).unwrap();
    tx.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
    tx.execute("INSERT INTO t VALUES (2)", &[]).unwrap();
    // read-your-writes within the transaction
    match tx.query("SELECT id FROM t", &[]).unwrap().count() {
        2 => {}
        n => panic!("expected 2 rows visible inside the tx, got {n}"),
    }
    tx.commit().unwrap();
    assert_eq!(count(&mut db, "t"), 2);
}

#[test]
fn begin_execute_rollback_discards() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1)",
    ]);
    let mut tx = db.begin(true).unwrap();
    tx.execute("INSERT INTO t VALUES (2)", &[]).unwrap();
    tx.rollback().unwrap();
    assert_eq!(count(&mut db, "t"), 1);
}

#[test]
fn dropping_an_unfinished_tx_rolls_back() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1)",
    ]);
    {
        let mut tx = db.begin(true).unwrap();
        tx.execute("INSERT INTO t VALUES (2)", &[]).unwrap();
        // tx dropped here without commit/rollback — the bbolt safety net rolls it back
    }
    assert!(!db.in_transaction(), "tx must be closed after drop");
    assert_eq!(count(&mut db, "t"), 1);
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
    let mut tx = db.begin(true).unwrap();
    tx.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
    // a SQL BEGIN inside an already-open transaction is 25001
    assert_eq!(tx.execute("BEGIN", &[]).err().unwrap().code(), "25001");
    // the original block survives the rejected nested BEGIN
    tx.commit().unwrap();
    assert_eq!(count(&mut db, "t"), 1);
}

#[test]
fn commit_and_rollback_are_noops_in_autocommit() {
    let mut db = db_with(&["CREATE TABLE t (id i32 PRIMARY KEY)"]);
    // no open transaction: both are lenient no-op successes (transactions.md §4.2)
    db.commit().unwrap();
    db.rollback().unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1)").unwrap();
    db.rollback().unwrap(); // does not undo the autocommitted insert
    assert_eq!(count(&mut db, "t"), 1);
}
