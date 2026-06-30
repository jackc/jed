//! Data-modifying (writable) CTEs (spec/design/writable-cte.md) — the per-core slice that the
//! PostgreSQL-clean conformance corpus (`cte/data_modifying.test`, `cte/with_dml.test`,
//! `cte/data_modifying_errors.test`) cannot express: the **command tag** of a data-modifying
//! primary (the `Outcome::Statement` affected-row count, which the corpus's `statement ok` does
//! not assert), and jed's **deterministic last-write-wins** resolution of an update/update or
//! update/delete of the SAME row — a documented divergence on a case PostgreSQL leaves unspecified
//! (§7). Mirrored in impl/go/writable_cte_test.go and impl/ts/tests/writable_cte.test.ts.

use jed::value::Value;
use jed::{Database, Outcome, Session, SessionOptions};

fn run(db: &mut Session, sql: &str) -> Outcome {
    db.execute(sql, &[]).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
}

fn exec(db: &mut Session, sql: &str) {
    run(db, sql);
}

fn rows(db: &mut Session, sql: &str) -> Vec<Vec<Value>> {
    match run(db, sql) {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

/// The affected-row count of a statement-shaped outcome (`None` for a query result).
fn affected(db: &mut Session, sql: &str) -> Option<i64> {
    match run(db, sql) {
        Outcome::Statement { rows_affected, .. } => rows_affected,
        Outcome::Query { .. } => panic!("expected a statement result for {sql:?}"),
    }
}

fn i32s(rows: Vec<Vec<Value>>) -> Vec<i64> {
    let mut v: Vec<i64> = rows
        .into_iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            ref other => panic!("expected an integer, got {other:?}"),
        })
        .collect();
    v.sort();
    v
}

fn setup(db: &mut Session) {
    exec(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
    exec(db, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)");
}

// --- the command tag of a data-modifying primary (the result is the PRIMARY's, §4) ------------

#[test]
fn with_on_insert_primary_no_returning_reports_affected_count() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    setup(&mut db);
    exec(&mut db, "CREATE TABLE dst (x i32)");
    // A WITH feeding an INSERT primary with no RETURNING is a STATEMENT whose count is the
    // primary's inserted-row count (a CTE's own count is never surfaced — §4).
    let n = affected(
        &mut db,
        "WITH src AS (SELECT id FROM t WHERE id <= 2) INSERT INTO dst SELECT id FROM src",
    );
    assert_eq!(n, Some(2));
    assert_eq!(i32s(rows(&mut db, "SELECT x FROM dst")), vec![1, 2]);
}

#[test]
fn with_on_delete_primary_no_returning_reports_affected_count() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    setup(&mut db);
    let n = affected(
        &mut db,
        "WITH old AS (SELECT id FROM t WHERE id >= 2) DELETE FROM t WHERE id IN (SELECT id FROM old)",
    );
    assert_eq!(n, Some(2));
    assert_eq!(i32s(rows(&mut db, "SELECT id FROM t")), vec![1]);
}

#[test]
fn with_on_update_primary_no_returning_reports_affected_count() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    setup(&mut db);
    let n = affected(
        &mut db,
        "WITH hi AS (SELECT id FROM t WHERE v >= 20) UPDATE t SET v = v + 1 WHERE id IN (SELECT id FROM hi)",
    );
    assert_eq!(n, Some(2));
}

#[test]
fn data_modifying_cte_count_not_surfaced_under_select_primary() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    setup(&mut db);
    // The data-modifying CTE inserts 1 row, but the SELECT primary's result is what is returned —
    // and it reads the PRE-statement table (the pin, §2), so count is 3, not 4.
    let r = rows(
        &mut db,
        "WITH ins AS (INSERT INTO t VALUES (4, 40) RETURNING *) SELECT count(*) FROM t",
    );
    assert_eq!(r, vec![vec![Value::Int(3)]]);
    // ...and the insert still landed (always to completion, §3).
    assert_eq!(
        rows(&mut db, "SELECT count(*) FROM t"),
        vec![vec![Value::Int(4)]]
    );
}

// --- jed's deterministic last-write-wins on a same-row conflict (PG-unspecified, §7) ----------

#[test]
fn same_row_two_updates_last_write_wins() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    setup(&mut db);
    // Two CTEs update id=1. Each reads the PIN (pre-statement v=10) and returns its own new value,
    // so BOTH return a row; the writes apply in lexical order, last-write-wins, so the table ends
    // at the SECOND CTE's value. PostgreSQL applies and returns only ONE (unspecified which) — the
    // documented divergence.
    let r = i32s(rows(
        &mut db,
        "WITH a AS (UPDATE t SET v = 100 WHERE id = 1 RETURNING v),
              b AS (UPDATE t SET v = 200 WHERE id = 1 RETURNING v)
         SELECT v FROM a UNION ALL SELECT v FROM b",
    ));
    assert_eq!(
        r,
        vec![100, 200],
        "both updates compute RETURNING from the pin"
    );
    // The committed value is the second (lexically later) write.
    assert_eq!(
        rows(&mut db, "SELECT v FROM t WHERE id = 1"),
        vec![vec![Value::Int(200)]]
    );
}

#[test]
fn same_row_update_then_delete_delete_wins() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    setup(&mut db);
    // CTE a updates id=1 to 100; CTE b deletes id=1. Both read the pin (the pre-statement row), so
    // a returns 100 and b returns the pre-statement old value 10; b's delete applies after a's
    // update, so the row is gone at the end (delete wins).
    let upd = i32s(rows(
        &mut db,
        "WITH a AS (UPDATE t SET v = 100 WHERE id = 1 RETURNING v) SELECT v FROM a",
    ));
    assert_eq!(upd, vec![100]);
    // Reset and run the combined conflict.
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    setup(&mut db);
    let r = i32s(rows(
        &mut db,
        "WITH a AS (UPDATE t SET v = 100 WHERE id = 1 RETURNING v),
              b AS (DELETE FROM t WHERE id = 1 RETURNING v)
         SELECT v FROM a UNION ALL SELECT v FROM b",
    ));
    assert_eq!(
        r,
        vec![10, 100],
        "a returns the new value, b the pre-statement old value"
    );
    // id=1 is gone (the delete applied last).
    assert_eq!(i32s(rows(&mut db, "SELECT id FROM t")), vec![2, 3]);
}
