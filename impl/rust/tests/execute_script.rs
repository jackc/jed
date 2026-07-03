//! S2 `execute_script` host-API surface (spec/design/session.md §4.2): the multi-statement
//! migration/import convenience — split, run each in order, discard rows, return the `O(1)`
//! `ScriptSummary`. All-or-nothing when the session is `Idle`, join-when-`Open`, in-script
//! transaction control `0A000`. These are host-API behaviors the single-statement corpus cannot
//! call (CLAUDE.md §10); the splitter's own boundary correctness is unit-tested in `src/split.rs`.

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions, TxStatus};

fn count(db: &mut Session) -> i64 {
    match db.execute("SELECT count(*) FROM t", &[]).unwrap() {
        Outcome::Query { rows, .. } => match rows[0][0] {
            Value::Int(n) => n,
            ref other => panic!("expected an int count, got {other:?}"),
        },
        other => panic!("expected a query, got {other:?}"),
    }
}

#[test]
fn script_summary_counts_and_commits_atomically_when_idle() {
    // An Idle session wraps the whole run in one implicit transaction; the summary carries only
    // counts (rows_affected_total sums the DML command tags — DDL and SELECT contribute nothing).
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    let summary = db
        .execute_script(
            "CREATE TABLE t (id i32 PRIMARY KEY, v i32);
             INSERT INTO t VALUES (1, 10);
             INSERT INTO t VALUES (2, 20), (3, 30);
             UPDATE t SET v = v + 1 WHERE id >= 2;
             DELETE FROM t WHERE id = 1;",
        )
        .unwrap();

    assert_eq!(summary.statements_run, 5);
    assert_eq!(summary.rows_affected_total, 1 + 2 + 2 + 1); // insert+insert+update+delete; DDL = 0
    assert!(summary.cost > 0);

    // The implicit transaction committed: the rows survive on the default session, which is Idle.
    assert_eq!(db.status(), TxStatus::Idle);
    assert_eq!(count(&mut db), 2); // ids 2 and 3 remain
}

#[test]
fn script_is_all_or_nothing_on_error() {
    // The third INSERT duplicates PK 1 (23505). Because the run is one implicit transaction, the
    // first two inserts roll back too — the table is left empty and the session returns to Idle.
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();

    let err = db
        .execute_script(
            "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); INSERT INTO t VALUES (1)",
        )
        .err()
        .unwrap();
    assert_eq!(err.code(), "23505");
    assert_eq!(db.status(), TxStatus::Idle);
    assert_eq!(count(&mut db), 0); // nothing committed — all-or-nothing
}

#[test]
fn script_select_rows_are_discarded_but_the_statement_is_counted() {
    // A SELECT in a script runs (its cost accrues, it counts as a statement) but its rows are
    // discarded and it adds nothing to rows_affected_total.
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    let summary = db
        .execute_script(
            "CREATE TABLE t (id i32 PRIMARY KEY);
             INSERT INTO t VALUES (1), (2);
             SELECT * FROM t;",
        )
        .unwrap();
    assert_eq!(summary.statements_run, 3);
    assert_eq!(summary.rows_affected_total, 2); // only the INSERT; the SELECT contributes 0
    assert_eq!(count(&mut db), 2);
}

#[test]
fn empty_script_is_a_no_op_success() {
    // Whitespace/comment-only input yields no statements — a clean zero summary, no transaction left
    // open.
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    let summary = db
        .execute_script("  -- just a comment\n /* and a block */ ;;; ")
        .unwrap();
    assert_eq!(summary.statements_run, 0);
    assert_eq!(summary.rows_affected_total, 0);
    assert_eq!(summary.cost, 0);
    assert_eq!(db.status(), TxStatus::Idle);
}

#[test]
fn in_script_transaction_control_is_feature_not_supported() {
    // The implicit wrapper owns the boundary, so BEGIN/COMMIT/ROLLBACK inside a script is 0A000 and
    // the partial run rolls back (the v1 narrowing — partitioning is deferred, §4.2/§11).
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();

    for script in [
        "INSERT INTO t VALUES (1); COMMIT; INSERT INTO t VALUES (2)",
        "INSERT INTO t VALUES (1); BEGIN; INSERT INTO t VALUES (2)",
        "INSERT INTO t VALUES (1); ROLLBACK",
    ] {
        let err = db.execute_script(script).err().unwrap();
        assert_eq!(err.code(), "0A000", "script: {script}");
        assert_eq!(db.status(), TxStatus::Idle);
        assert_eq!(count(&mut db), 0); // the wrapper rolled back the partial run
    }
}

#[test]
fn script_joins_an_open_transaction_without_committing() {
    // Run while the session is already Open: the script joins that transaction (no wrapper, no
    // auto-commit). The caller still owns the boundary, so the staged rows are visible inside the
    // block but vanish on the caller's ROLLBACK.
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();

    db.execute("BEGIN", &[]).unwrap();
    let summary = db
        .execute_script("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)")
        .unwrap();
    assert_eq!(summary.statements_run, 2);
    assert_eq!(db.status(), TxStatus::Open); // NOT auto-committed — the caller's block stays open
    assert_eq!(count(&mut db), 2); // visible inside the block

    db.execute("ROLLBACK", &[]).unwrap();
    assert_eq!(db.status(), TxStatus::Idle);
    assert_eq!(count(&mut db), 0); // the caller rolled the joined work back
}

#[test]
fn script_error_inside_an_open_transaction_leaves_it_failed_for_the_caller() {
    // Joining an open block, a mid-run error poisons the block (Failed) and returns the error —
    // execute_script does NOT roll back a transaction it does not own; the caller does.
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();

    db.execute("BEGIN", &[]).unwrap();
    let err = db
        .execute_script("INSERT INTO t VALUES (1); INSERT INTO t VALUES (1)")
        .err()
        .unwrap();
    assert_eq!(err.code(), "23505");
    assert_eq!(db.status(), TxStatus::Failed);

    db.execute("ROLLBACK", &[]).unwrap();
    assert_eq!(db.status(), TxStatus::Idle);
    assert_eq!(count(&mut db), 0);
}

#[test]
fn additional_session_runs_a_script_over_the_shared_core() {
    // execute_script on an *additional* session (spec/design/session.md §2.1/§2.4) shares committed
    // storage through the Database core and commits the run all-or-nothing — another session sees it.
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut a = db.session(SessionOptions::default());
    a.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();

    let mut s = db.session(SessionOptions::default());
    let summary = s
        .execute_script("INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)")
        .unwrap();
    assert_eq!(summary.statements_run, 2);

    // Committed through the additional session, visible to another session over the core.
    match a.execute("SELECT count(*) FROM t", &[]).unwrap() {
        Outcome::Query { rows, .. } => assert_eq!(rows[0][0], Value::Int(2)),
        other => panic!("expected a query, got {other:?}"),
    }
    assert_eq!(a.status(), TxStatus::Idle);
}
