//! date_trunc / EXTRACT / cross-family datetime casts — the deliberate PostgreSQL divergences
//! (spec/design/timezones.md §9). The agreeing behavior (every value, the 0A000/22023 field-validity
//! errors, the session-zone computation) is oracle-checked in `suites/expr/{date_trunc,extract,
//! datetime_cast}.test` and runs on every core; these per-core tests cover only what the oracle
//! corpus CANNOT express (CLAUDE.md §10) — the cases where jed deliberately differs from PG:
//!
//!   * `EXTRACT(julian …)` — jed defers the field (`0A000`); PG returns a value (timezones.md §9.2).
//!   * `date_part('field', …)` — jed has no such function (`42883`); PG returns `double precision`,
//!     and jed has no `float` type, so the function is deferred (timezones.md §9.2).
//!   * `EXTRACT(field FROM ±infinity)` — jed's decimal is finite-only, so it traps `22003`; PG
//!     returns numeric `±Infinity` (timezones.md §9.2).
//!   * a non-datetime, non-text source to a datetime target — jed `42804` (PG `42846`)
//!     (timezones.md §9.3, casts.toml).

use jed::{CreateOptions, Database, Session, SessionOptions};

fn err_code(db: &mut Session, sql: &str) -> String {
    match db.query_outcome(sql, &[]) {
        Err(e) => e.code().to_string(),
        Ok(_) => panic!("expected error for {sql}"),
    }
}

/// EXTRACT(julian …) is a deferred field on every type: jed 0A000, PG returns a value.
#[test]
fn extract_julian_is_deferred() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        err_code(
            &mut db,
            "SELECT EXTRACT(julian FROM timestamp '2024-03-15 00:00:00')"
        ),
        "0A000"
    );
    assert_eq!(
        err_code(&mut db, "SELECT EXTRACT(julian FROM date '2024-03-15')"),
        "0A000"
    );
}

/// date_part has LANDED (suites/expr/date_part.test); `julian` stays EXTRACT's deferred field on
/// it too — 0A000 where PG computes a value (the documented divergence the oracle_overrides
/// ledger records; the timestamp overload cannot live in the PG-clean corpus).
#[test]
fn date_part_julian_is_deferred() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        err_code(
            &mut db,
            "SELECT date_part('julian', timestamp '2024-03-15 13:00:00')"
        ),
        "0A000"
    );
}

/// EXTRACT over an infinite timestamp traps 22003 (jed's decimal is finite-only); PG returns
/// numeric ±Infinity.
#[test]
fn extract_from_infinity_traps() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        err_code(&mut db, "SELECT EXTRACT(year FROM timestamp 'infinity')"),
        "22003"
    );
    assert_eq!(
        err_code(
            &mut db,
            "SELECT EXTRACT(epoch FROM timestamptz '-infinity')"
        ),
        "22003"
    );
}

/// A non-datetime source to a datetime target is a forbidden pair in jed (42804; PG 42846). The
/// runtime text → datetime casts have landed (tests/cast_text_string_native.rs).
#[test]
fn non_datetime_source_to_datetime_is_forbidden() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    // int → timestamp: jed 42804, PG 42846.
    assert_eq!(
        err_code(&mut db, "SELECT CAST(1 + 1 AS timestamp)"),
        "42804"
    );
}
