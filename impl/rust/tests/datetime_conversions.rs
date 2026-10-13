//! date_trunc / EXTRACT / cross-family datetime casts — the deliberate PostgreSQL divergences
//! (spec/design/timezones.md §9). The agreeing behavior (every value, the 0A000/22023 field-validity
//! errors, the session-zone computation) is oracle-checked in `suites/expr/{date_trunc,extract,
//! datetime_cast}.test` and runs on every core; these per-core tests cover only what the oracle
//! corpus CANNOT express (CLAUDE.md §10) — the cases where jed deliberately differs from PG:
//!
//!   * `EXTRACT(field FROM ±infinity)` (timestamp, timestamptz, or date) — jed's decimal is
//!     finite-only, so it traps `22003`; PG returns numeric `±Infinity` (or NULL for an oscillating
//!     field). `date_part` follows EXTRACT (`22003` where PG returns float `±Infinity`)
//!     (timezones.md §9.2).
//!   * a non-datetime, non-text source to a datetime target — jed `42804` (PG `42846`)
//!     (timezones.md §9.3, casts.toml).

use jed::{CreateOptions, Database, Session, SessionOptions};

fn err_code(db: &mut Session, sql: &str) -> String {
    match db.query_outcome(sql, &[]) {
        Err(e) => e.code().to_string(),
        Ok(_) => panic!("expected error for {sql}"),
    }
}

/// EXTRACT (and date_part) over an infinite timestamp/timestamptz/date traps 22003 (jed's decimal
/// is finite-only); PG returns ±Infinity (or NULL for an oscillating field).
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
    for sql in [
        "SELECT EXTRACT(julian FROM timestamp 'infinity')",
        "SELECT EXTRACT(julian FROM date '-infinity')",
        "SELECT EXTRACT(year FROM date 'infinity')",
        "SELECT EXTRACT(day FROM date '-infinity')",
        "SELECT date_part('julian', timestamp '-infinity')",
        "SELECT date_part('julian', timestamptz 'infinity')",
        "SELECT date_part('julian', date 'infinity')",
    ] {
        assert_eq!(err_code(&mut db, sql), "22003", "{sql}");
    }
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
