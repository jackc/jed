//! Runtime text → date cast — the parts the PG-clean oracle corpus cannot express (the text→date
//! cast follow-on; spec/design/date.md §6, spec/types/casts.toml). The strict-ISO accepted grammar
//! AGREES with PostgreSQL and is oracle-checked in `suites/cast/text_to_date.test` (run on every
//! core, including the 42P17 index rejections — PG's `date_in` is stable too); this file covers
//! only the jed-stricter grammar DIVERGENCES: the DateStyle-dependent / non-ISO spellings
//! PostgreSQL accepts and jed rejects (22007), and the `:60` leap-second roll-forward PG performs
//! and jed rejects (22008) — identical to the literal path (date.md §2). Every cast below is on a
//! NON-LITERAL text column, so it exercises the per-row `eval_date_convert` path, not the
//! resolve-time literal fold. Mirrors impl/go/cast_text_date_test.go.

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

/// Build `t(id i32 pk, s text)` seeded with `rows` (id = 1.., s = each str).
fn seeded(rows: &[&str]) -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, s text)", &[])
        .unwrap();
    for (i, s) in rows.iter().enumerate() {
        db.query_outcome(&format!("INSERT INTO t VALUES ({}, '{}')", i + 1, s), &[])
            .unwrap();
    }
    db
}

/// The SQLSTATE of a per-row cast expected to error.
fn err_at(db: &mut Session, expr: &str, id: usize) -> String {
    match db.query_outcome(&format!("SELECT {expr} FROM t WHERE id = {id}"), &[]) {
        Err(e) => e.code().to_string(),
        Ok(_) => panic!("expected error for {expr} (id {id})"),
    }
}

#[test]
fn non_iso_spellings_trap_22007_at_runtime() {
    // PG (DateStyle MDY / month names / compact ISO) accepts all three; jed is strict ISO only.
    for s in ["01/15/2024", "Jan 15, 2024", "20240115"] {
        let mut db = seeded(&[s]);
        assert_eq!(err_at(&mut db, "s :: date", 1), "22007", "{s}");
    }
}

#[test]
fn leap_second_traps_22008_at_runtime() {
    // PG rolls `:60` forward; jed rejects leap seconds (the discarded time is still validated).
    let mut db = seeded(&["2024-01-01 12:30:60"]);
    assert_eq!(err_at(&mut db, "s :: date", 1), "22008");
}

#[test]
fn null_propagates_through_the_runtime_cast() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, s text)", &[])
        .unwrap();
    db.query_outcome("INSERT INTO t VALUES (1, NULL)", &[])
        .unwrap();
    match db
        .query_outcome("SELECT s :: date FROM t WHERE id = 1", &[])
        .unwrap()
    {
        Outcome::Query { rows, .. } => assert!(matches!(rows[0][0], Value::Null)),
        other => panic!("expected query, got {other:?}"),
    }
}
