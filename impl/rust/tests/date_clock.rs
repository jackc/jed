//! Clock-relative date literals — the parts the corpus cannot express (spec/design/date.md §6).
//! The literal surface (all five specials, the STABLE statement clock, INSERT/UPDATE/DEFAULT, the
//! 42P17 index rejections, and the strict INSERT…SELECT divergence) is corpus-tested with
//! injected clocks (suites/types/date_clock.test, run on every core); this file covers only what
//! that cannot reach: the SESSION-ZONE interaction (the corpus has no zone-setting directive —
//! the session TimeZone is host-API-only, session.md §6.2) and the never-folded property observed
//! across clock changes on ONE handle. Mirrors impl/go/date_clock_test.go.

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions, fixed_clock};

/// 2024-07-15 23:30:00 UTC — half an hour before a UTC midnight, so a UTC+2 session zone is
/// already on 2024-07-16 while UTC is still on 2024-07-15.
const NEAR_MIDNIGHT_UTC: i64 = 1_721_086_200_000_000;

fn clock_db(micros: i64, stmts: &[&str]) -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for s in stmts {
        db.query_outcome(s, &[]).unwrap();
    }
    db.set_clock_source(fixed_clock(micros));
    db
}

/// Whether `expr` evaluates to 2024-07-16 (vs 2024-07-15).
fn is_next_day(db: &mut Session, expr: &str) -> bool {
    match db
        .query_outcome(&format!("SELECT ({expr}) = '2024-07-16'::date"), &[])
        .unwrap()
    {
        Outcome::Query { rows, .. } => matches!(rows[0][0], Value::Bool(true)),
        other => panic!("expected query, got {other:?}"),
    }
}

#[test]
fn date_clock_uses_session_zone() {
    let mut db = clock_db(NEAR_MIDNIGHT_UTC, &[]);
    // Default UTC session: still 2024-07-15.
    assert!(!is_next_day(&mut db, "'today'::date"));
    // A UTC+2 session zone (the POSIX fixed-offset spelling '-02:00' — positive is WEST,
    // timezones.md §6): local wall clock is 01:30 on 2024-07-16.
    db.set_time_zone("-02:00").unwrap();
    assert!(is_next_day(&mut db, "'today'::date"));
    // The runtime text→date cast consults the same zone.
    db.query_outcome("CREATE TABLE s (id i32 PRIMARY KEY, w text)", &[])
        .unwrap();
    db.query_outcome("INSERT INTO s VALUES (1, 'today')", &[])
        .unwrap();
    assert!(is_next_day(
        &mut db,
        "(SELECT w FROM s WHERE id = 1) :: date"
    ));
}

#[test]
fn date_clock_default_never_folds() {
    // The DEFAULT is created under clock A; an INSERT under clock B (a day later) must see B's
    // day — a CREATE-TABLE-folded constant (PostgreSQL's behavior) could not. Same handle.
    let mut db = clock_db(
        NEAR_MIDNIGHT_UTC,
        &["CREATE TABLE d (id i32 PRIMARY KEY, dt date DEFAULT 'today')"],
    );
    db.query_outcome("INSERT INTO d (id) VALUES (1)", &[])
        .unwrap();
    db.set_clock_source(fixed_clock(NEAR_MIDNIGHT_UTC + 86_400_000_000));
    db.query_outcome("INSERT INTO d (id) VALUES (2)", &[])
        .unwrap();
    match db
        .query_outcome(
            "SELECT (SELECT dt FROM d WHERE id = 2) = (SELECT dt FROM d WHERE id = 1) + 1",
            &[],
        )
        .unwrap()
    {
        Outcome::Query { rows, .. } => {
            assert!(
                matches!(rows[0][0], Value::Bool(true)),
                "DEFAULT 'today' folded: day 2 is not day 1 + 1"
            );
        }
        other => panic!("expected query, got {other:?}"),
    }
}
