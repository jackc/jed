//! Session-zone rendering — the parts the corpus cannot express (spec/design/timezones.md §9.5). The
//! PG-agreeing surface (result cells in the record's zone, timestamptz nested in array / composite /
//! range output and in `array → text`, its cost and 42P17) is oracle-checked in
//! suites/types/timestamptz_session_render.test; this file covers the host render API — the
//! canonical `Value::render` stays UTC, `Session::render` / `Transaction::render` follow the
//! session's current zone, and a render at jed's out-of-PG-range extremes is 22008 — plus the
//! documented divergence that EXPLAIN keeps the canonical UTC spelling of a timestamptz constant.
//! Mirrors impl/go/timestamptz_session_render_test.go and
//! impl/ts/tests/timestamptz_session_render.test.ts.

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn mem() -> Session {
    Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default())
}

fn one(db: &mut Session, sql: &str) -> Value {
    match db.query_outcome(sql, &[]).unwrap() {
        Outcome::Query { rows, .. } => rows[0][0].clone(),
        other => panic!("expected query, got {other:?}"),
    }
}

#[test]
fn session_render_follows_the_current_zone() {
    let mut db = mem();
    let v = one(
        &mut db,
        "SELECT ARRAY[timestamptz '2024-01-15 12:00:00+00', timestamptz 'infinity']",
    );
    // The default session zone is UTC: the session render equals the canonical render.
    assert_eq!(db.render(&v).unwrap(), v.render());
    // The value is zone-free; the render reads the zone in effect when it is called. The POSIX
    // fixed-offset spelling '+05:00' is UTC-5.
    db.set_time_zone("+05:00").unwrap();
    assert_eq!(
        db.render(&v).unwrap(),
        r#"{"2024-01-15 07:00:00-05",infinity}"#
    );
    db.set_time_zone("-05:30").unwrap();
    assert_eq!(
        db.render(&v).unwrap(),
        r#"{"2024-01-15 17:30:00+05:30",infinity}"#
    );
    // The canonical render stays zone-free whatever the session zone.
    assert_eq!(v.render(), r#"{"2024-01-15 12:00:00+00",infinity}"#);
    // A value with no timestamptz renders the same either way.
    assert_eq!(db.render(&Value::Int(7)).unwrap(), "7");
    assert_eq!(db.render(&Value::Null).unwrap(), "NULL");
}

#[test]
fn transaction_render_uses_the_owning_session_zone() {
    let mut db = mem();
    db.set_time_zone("+05:00").unwrap();
    let text = db
        .update(|tx| {
            let v = tx
                .query("SELECT timestamptz '2024-01-15 12:00:00+00'", &[])?
                .next()
                .unwrap()[0]
                .clone();
            tx.render(&v)
        })
        .unwrap();
    assert_eq!(text, "2024-01-15 07:00:00-05");
}

#[test]
fn session_render_at_the_i64_extreme_is_22008() {
    // i64::MAX µs is 294247-01-10 04:00:54.775807 UTC — far outside PostgreSQL's range. A UTC-5
    // reading of a finite instant within five hours of it overflows (timezones.md §6).
    let mut db = mem();
    let v = one(&mut db, "SELECT timestamptz '294247-01-10 04:00:00+00'");
    assert_eq!(v.render(), "294247-01-10 04:00:00+00");
    db.set_time_zone("-05:00").unwrap();
    assert_eq!(db.render(&v).unwrap_err().code(), "22008");
    // Nested in a container, the same instant fails the whole render.
    let arr = one(
        &mut db,
        "SELECT ARRAY[timestamptz '294247-01-10 04:00:00+00']",
    );
    assert_eq!(db.render(&arr).unwrap_err().code(), "22008");
}

#[test]
fn explain_keeps_the_canonical_utc_constant() {
    // A documented divergence: PostgreSQL deparses the constant in the session zone; jed's plan text
    // is context-free, so the constant prints in its canonical UTC spelling under any session zone.
    let mut db = mem();
    db.query_outcome("CREATE TABLE ev (id i32 PRIMARY KEY, at timestamptz)", &[])
        .unwrap();
    db.set_time_zone("+05:00").unwrap();
    let rows = match db
        .query_outcome(
            "EXPLAIN (VERBOSE) SELECT id FROM ev WHERE at > timestamptz '2024-01-15 12:00:00+00'",
            &[],
        )
        .unwrap()
    {
        Outcome::Query { rows, .. } => rows,
        other => panic!("expected query, got {other:?}"),
    };
    let text = rows
        .iter()
        .flat_map(|r| r.iter())
        .filter_map(|v| match v {
            Value::Text(s) => Some(s.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        text.contains("'2024-01-15 12:00:00+00'"),
        "EXPLAIN should print the canonical UTC constant:\n{text}"
    );
}
