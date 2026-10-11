//! Offset-less timestamptz input in the session zone — the parts the corpus cannot express
//! (spec/design/timezones.md §9.3). The PG-agreeing surface (literals, casts, assignment, DEFAULT,
//! array/range/record text) is oracle-checked in suites/types/timestamptz_session_input.test; this
//! file covers the two deliberate divergences — a prepared statement re-reads the session zone on
//! every execution (PG freezes the prepare-time zone) and an index expression or predicate holding
//! an offset-less literal is 42P17 (PG folds it into the definition) — plus a text value bound to a
//! timestamptz parameter (host-API surface). Mirrors impl/go/timestamptz_session_input_test.go.

use jed::value::Value;
use jed::{CreateOptions, Database, Session, SessionOptions};

fn mem() -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.query_outcome("CREATE TABLE ev (id i32 PRIMARY KEY, at timestamptz)", &[])
        .unwrap();
    // 12:00 UTC, and 12:00 at UTC+2 (= 10:00 UTC).
    db.query_outcome(
        "INSERT INTO ev VALUES (1, '2024-07-15 12:00:00+00'), (2, '2024-07-15 10:00:00+00')",
        &[],
    )
    .unwrap();
    db
}

fn ids(db: &mut Session, stmt: &jed::PreparedStatement, params: &[Value]) -> Vec<Value> {
    db.query_prepared(stmt, params)
        .unwrap()
        .map(|row| row[0].clone())
        .collect()
}

#[test]
fn prepared_statement_rereads_the_session_zone() {
    let mut db = mem();
    let stmt = db
        .prepare("SELECT id FROM ev WHERE at = '2024-07-15 12:00:00'")
        .unwrap();
    assert_eq!(ids(&mut db, &stmt, &[]), vec![Value::Int(1)], "UTC session");
    // The POSIX fixed-offset spelling '-02:00' is UTC+2 (timezones.md §6): 12:00 local = 10:00 UTC.
    db.set_time_zone("-02:00").unwrap();
    assert_eq!(
        ids(&mut db, &stmt, &[]),
        vec![Value::Int(2)],
        "a cached plan must not keep the instant folded under the prepare-time zone"
    );
    // An explicit offset does not depend on the zone either way.
    let fixed = db
        .prepare("SELECT id FROM ev WHERE at = '2024-07-15 12:00:00+00'")
        .unwrap();
    assert_eq!(ids(&mut db, &fixed, &[]), vec![Value::Int(1)]);
}

#[test]
fn prepared_insert_rereads_the_session_zone() {
    let mut db = mem();
    let ins = db
        .prepare("INSERT INTO ev VALUES ($1, '2024-07-15 12:00:00')")
        .unwrap();
    db.execute_prepared(&ins, &[Value::Int(3)]).unwrap();
    db.set_time_zone("-02:00").unwrap();
    db.execute_prepared(&ins, &[Value::Int(4)]).unwrap();
    db.set_time_zone("UTC").unwrap();
    let q = db
        .prepare("SELECT id FROM ev WHERE at = '2024-07-15 10:00:00' ORDER BY id")
        .unwrap();
    assert_eq!(ids(&mut db, &q, &[]), vec![Value::Int(2), Value::Int(4)]);
}

#[test]
fn bound_text_parameter_reads_the_session_zone() {
    let mut db = mem();
    db.set_time_zone("-02:00").unwrap();
    let stmt = db.prepare("SELECT id FROM ev WHERE at = $1").unwrap();
    let local = Value::Text("2024-07-15 12:00:00".into());
    assert_eq!(ids(&mut db, &stmt, &[local]), vec![Value::Int(2)]);
    let explicit = Value::Text("2024-07-15 12:00:00+00".into());
    assert_eq!(ids(&mut db, &stmt, &[explicit]), vec![Value::Int(1)]);
}

#[test]
fn offset_less_literal_in_an_index_is_42p17() {
    let mut db = mem();
    for sql in [
        "CREATE INDEX ON ev (id) WHERE at > '2024-01-01 00:00:00'",
        "CREATE INDEX ON ev ((at > timestamptz '2024-01-01'))",
        "CREATE INDEX ON ev (id) WHERE at < ('{\"2024-01-01 00:00:00\"}'::timestamptz[])[1]",
    ] {
        let err = db.query_outcome(sql, &[]).unwrap_err();
        assert_eq!(err.code(), "42P17", "{sql}");
    }
    // An explicit offset never consults the zone, so it stays immutable and indexable.
    db.query_outcome(
        "CREATE INDEX ON ev (id) WHERE at > '2024-01-01 00:00:00+00'",
        &[],
    )
    .unwrap();
    db.query_outcome("CREATE INDEX ON ev ((at > timestamptz 'infinity'))", &[])
        .unwrap();
}
