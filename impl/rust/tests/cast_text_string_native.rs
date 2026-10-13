//! Runtime text → timestamp / timestamptz / interval / bytea casts — the parts the PG-clean oracle
//! corpus cannot express (spec/types/casts.toml, spec/design/types.md §5). The accepted grammars
//! AGREE with PostgreSQL and are oracle-checked in `suites/cast/text_to_string_native.test` (run on
//! every core); this file covers only the DIVERGENCES: the jed-stricter input grammars (identical to
//! the literal path), the SQLSTATE subcodes that differ from PG, the forbidden-pair convention
//! (42804 where PG reports 42846), the i64 guard on the session-zone shift, and `$1::interval`.
//! Every cast is on a NON-LITERAL text column, so it exercises the per-row eval path, not the
//! resolve-time literal fold. Mirrors impl/go/cast_text_string_native_test.go.

use jed::interval::Interval;
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

fn query(db: &mut Session, sql: &str, params: &[Value]) -> Value {
    match db.query_outcome(sql, params).unwrap() {
        Outcome::Query { rows, .. } => rows[0][0].clone(),
        other => panic!("expected query, got {other:?}"),
    }
}

fn at(db: &mut Session, expr: &str, id: usize) -> Value {
    query(db, &format!("SELECT {expr} FROM t WHERE id = {id}"), &[])
}

fn err_code(db: &mut Session, sql: &str) -> String {
    match db.query_outcome(sql, &[]) {
        Err(e) => e.code().to_string(),
        Ok(_) => panic!("expected error for {sql}"),
    }
}

fn err_at(db: &mut Session, expr: &str, id: usize) -> String {
    err_code(db, &format!("SELECT {expr} FROM t WHERE id = {id}"))
}

#[test]
fn grammar_and_sqlstate_divergences() {
    let cases = [
        ("now", "s :: timestamp", "22007"), // PG: the statement time; jed: no clock specials
        ("today", "s :: timestamptz", "22007"), // PG: midnight today; jed: no clock specials
        ("epoch", "s :: timestamp", "22007"), // PG: 1970-01-01 00:00:00; jed: not accepted
        ("01/15/2024 12:00", "s :: timestamp", "22007"), // PG (DateStyle MDY) accepts; jed: ISO only
        ("2024-01-01 12:00:00+16", "s :: timestamptz", "22008"), // PG: 22009 displacement
        ("2147483648 days", "s :: interval", "22008"),   // PG: 22015 interval field overflow
        (r"\x0", "s :: bytea", "22P02"),                 // PG: 22023 odd number of hex digits
        (r"\xzz", "s :: bytea", "22P02"),                // PG: 22023 invalid hexadecimal digit
    ];
    for (s, expr, want) in cases {
        let mut db = seeded(&[s]);
        assert_eq!(err_at(&mut db, expr, 1), want, "{s:?} {expr}");
    }
}

#[test]
fn forbidden_source_to_string_native_is_42804() {
    // No integer → interval / timestamp / bytea cast exists: jed's strict-matrix 42804 (PG 42846).
    let mut db = seeded(&[]);
    for sql in [
        "SELECT 1::interval",
        "SELECT CAST(1 + 1 AS timestamp)",
        "SELECT 1::timestamptz",
        "SELECT 1::bytea",
        "SELECT (DATE '2024-01-01')::interval",
    ] {
        assert_eq!(err_code(&mut db, sql), "42804", "{sql}");
    }
}

#[test]
fn timestamptz_zone_shift_overflow_traps() {
    // A wall clock just below jed's i64 ceiling, read in a UTC-5 session (the POSIX fixed-offset
    // spelling '+05:00' — positive is WEST), shifts past i64: 22008, never a wrapped instant.
    let mut db = seeded(&["294247-01-10 04:00:00"]);
    db.set_time_zone("+05:00").unwrap();
    assert_eq!(err_at(&mut db, "s :: timestamptz", 1), "22008");
    // The same wall clock in UTC is representable.
    db.set_time_zone("UTC").unwrap();
    assert!(matches!(
        at(&mut db, "s :: timestamptz", 1),
        Value::Timestamptz(_)
    ));
}

#[test]
fn timestamptz_reads_fixed_offset_session_zone() {
    // A fixed-offset session zone needs no tz data: '-02:00' is UTC+2, so 12:00 local is 10:00 UTC.
    let mut db = seeded(&["2024-07-15 12:00:00", "2024-07-15 12:00:00+00"]);
    db.set_time_zone("-02:00").unwrap();
    let utc = "(s :: timestamptz) AT TIME ZONE 'UTC'";
    assert_eq!(at(&mut db, utc, 1).render(), "2024-07-15 10:00:00");
    assert_eq!(at(&mut db, utc, 2).render(), "2024-07-15 12:00:00");
}

#[test]
fn param_cast_to_interval_types_the_parameter() {
    let mut db = seeded(&[]);
    let iv = Value::Interval(Interval {
        months: 0,
        days: 3,
        micros: 0,
    });
    assert_eq!(
        query(&mut db, "SELECT $1::interval", &[iv]).render(),
        "3 days"
    );
}

#[test]
fn bytea_hex_error_messages_are_postgresqls() {
    // PostgreSQL's hex_decode_safe messages, shared by bytea hex input on the literal and per-row
    // paths (jed's code 22P02, PG's 22023) and decode(s, 'hex') (22023, as PG). The corpus checks
    // only the SQLSTATE, so the message — part of the cross-core contract — is pinned here.
    let cases = [
        (r"\x0 1", r#"invalid hexadecimal digit: " ""#),
        (r"\x 0", "invalid hexadecimal data: odd number of digits"),
        (r"\xzz1", r#"invalid hexadecimal digit: "z""#),
        (r"\x0é", r#"invalid hexadecimal digit: "é""#),
    ];
    for (s, want) in cases {
        let mut db = seeded(&[s]);
        for (sql, code) in [
            (format!("SELECT '{s}' :: bytea"), "22P02"),
            ("SELECT s :: bytea FROM t".to_string(), "22P02"),
            (format!("SELECT decode('{}', 'hex')", &s[2..]), "22023"),
        ] {
            match db.query_outcome(&sql, &[]) {
                Err(e) => {
                    assert_eq!(e.code(), code, "{sql}");
                    assert_eq!(e.message, want, "{sql}");
                }
                Ok(_) => panic!("expected error for {sql}"),
            }
        }
    }
}
