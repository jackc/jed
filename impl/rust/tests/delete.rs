//! Step 6: DELETE — predicate-matched removal, no-WHERE clears, three-valued logic,
//! and the no-PK monotonic-rowid regression (DELETE then INSERT must not collide).

use jed::{CreateOptions, Database, Session, SessionOptions};

fn db_with(stmts: &[&str]) -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for s in stmts {
        db.query_outcome(s, &[])
            .unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn setup() -> Session {
    db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, v i16)",
        "INSERT INTO t VALUES (1, 10)",
        "INSERT INTO t VALUES (2, 20)",
        "INSERT INTO t VALUES (3, 30)",
        "INSERT INTO t VALUES (4, NULL)",
    ])
}

#[test]
fn delete_from_missing_table_traps() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        db.query_outcome("DELETE FROM nope", &[])
            .unwrap_err()
            .code(),
        "42P01"
    );
}

#[test]
fn delete_unknown_column_traps() {
    let mut db = setup();
    assert_eq!(
        db.query_outcome("DELETE FROM t WHERE nope = 1", &[])
            .unwrap_err()
            .code(),
        "42703"
    );
}

/// `(count(*), sum(id), sum(v))` of `t` — enough to tell exactly which rows survive.
fn shape(db: &mut Session) -> (i64, i64, i64) {
    match db
        .query_outcome("SELECT count(*), sum(id)::i64, sum(v)::i64 FROM t", &[])
        .unwrap()
    {
        jed::Outcome::Query { rows, .. } => {
            let int = |v: &jed::value::Value| match v {
                jed::value::Value::Int(n) => *n,
                other => panic!("expected an int, got {other:?}"),
            };
            (int(&rows[0][0]), int(&rows[0][1]), int(&rows[0][2]))
        }
        other => panic!("expected a query, got {other:?}"),
    }
}

#[test]
fn delete_after_savepoint_never_mutates_a_frame_node() {
    // DELETE edits a uniquely owned dirty B+tree path in place (pmap.rs). A savepoint frame holds
    // `Arc` clones of the snapshot, so nodes dirtied before the SAVEPOINT are shared with the frame
    // and must be copied, not edited: ROLLBACK TO must restore every row the frame saw. The table
    // spans many leaves, so the deletes drain and merge leaves on paths dirtied before the frame.
    let mut db = db_with(&[
        "CREATE TABLE t (id i64 PRIMARY KEY, v i64, note text)",
        "INSERT INTO t SELECT g, g * 7, 'note-' || g FROM generate_series(1, 20000) AS g",
    ]);
    let all = shape(&mut db);
    assert_eq!(all.0, 20000);
    let run = |db: &mut Session, s: &str| {
        db.query_outcome(s, &[])
            .unwrap_or_else(|e| panic!("{s:?}: {}", e.message));
    };
    run(&mut db, "BEGIN");
    // Dirty (and uniquely own) paths across the tree before the frame exists.
    run(&mut db, "DELETE FROM t WHERE id % 10 = 0");
    run(&mut db, "DELETE FROM t WHERE id BETWEEN 5001 AND 6000");
    let before = shape(&mut db);
    run(&mut db, "SAVEPOINT a");
    run(&mut db, "DELETE FROM t WHERE id BETWEEN 1 AND 9000");
    run(&mut db, "DELETE FROM t WHERE id % 3 = 0");
    assert_ne!(shape(&mut db), before);
    run(&mut db, "ROLLBACK TO a");
    assert_eq!(shape(&mut db), before);
    // The frame survives ROLLBACK TO: a second round of in-place deletes must not reach it either.
    run(&mut db, "DELETE FROM t WHERE id > 100");
    run(&mut db, "DELETE FROM t WHERE id > 100"); // now on paths dirtied after the rollback
    run(&mut db, "ROLLBACK TO a");
    assert_eq!(shape(&mut db), before);
    // A nested frame over paths dirtied inside the outer one.
    run(&mut db, "DELETE FROM t WHERE id BETWEEN 15001 AND 16000");
    let mid = shape(&mut db);
    run(&mut db, "SAVEPOINT b");
    run(&mut db, "DELETE FROM t WHERE id > 12000");
    run(&mut db, "ROLLBACK TO b");
    assert_eq!(shape(&mut db), mid);
    run(&mut db, "RELEASE a");
    run(&mut db, "COMMIT");
    assert_eq!(shape(&mut db), mid);
}
