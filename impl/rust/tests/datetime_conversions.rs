//! date_trunc / EXTRACT / cross-family datetime casts — the deliberate PostgreSQL divergences
//! (spec/design/timezones.md §9). The agreeing behavior (every value, the 0A000/22023 field-validity
//! errors, the session-zone computation) is oracle-checked in `suites/expr/{date_trunc,extract,
//! datetime_cast}.test` and runs on every core; these per-core tests cover only what the oracle
//! corpus CANNOT express (CLAUDE.md §10) — the cases where jed deliberately differs from PG:
//!
//!   * `EXTRACT(field FROM ±infinity)` of a monotonic field (`year`, `decade`, `century`,
//!     `millennium`, `julian`, `isoyear`, `epoch`) over a timestamp, timestamptz, or date — jed's
//!     decimal is finite-only, so it traps `22003`; PG returns numeric `±Infinity`. (The oscillating
//!     fields are NULL, as in PG, and `date_part`'s float8 carries the `±Infinity` — both
//!     oracle-checked in `suites/expr/extract_infinity.test`.) (timezones.md §9.2)
//!   * `EXTRACT(timezone* FROM ±infinity timestamp)` — jed validates EXTRACT's field at resolve, so
//!     it is `0A000` as for a finite timestamp; PG decides per value and returns NULL.
//!   * a non-datetime, non-text source to a datetime target — jed `42804` (PG `42846`)
//!     (timezones.md §9.3, casts.toml).

use jed::{CreateOptions, Database, Session, SessionOptions};

fn err_code(db: &mut Session, sql: &str) -> String {
    match db.query_outcome(sql, &[]) {
        Err(e) => e.code().to_string(),
        Ok(_) => panic!("expected error for {sql}"),
    }
}

/// EXTRACT of a monotonic field over an infinite timestamp/timestamptz/date traps 22003 (jed's
/// decimal is finite-only); PG returns numeric ±Infinity.
#[test]
fn extract_monotonic_field_from_infinity_traps() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for field in [
        "year",
        "decade",
        "century",
        "millennium",
        "julian",
        "isoyear",
        "epoch",
    ] {
        for ty in ["timestamp", "timestamptz", "date"] {
            for inf in ["infinity", "-infinity"] {
                let sql = format!("SELECT EXTRACT({field} FROM {ty} '{inf}')");
                assert_eq!(err_code(&mut db, &sql), "22003", "{sql}");
            }
        }
    }
}

/// EXTRACT(timezone* FROM an infinite timestamp) is 0A000 at resolve, like a finite timestamp's;
/// PG returns NULL (its NonFiniteTimestampTzPart accepts every unit).
#[test]
fn extract_timezone_from_infinite_timestamp_is_unsupported() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for field in ["timezone", "timezone_hour", "timezone_minute"] {
        let sql = format!("SELECT EXTRACT({field} FROM timestamp 'infinity')");
        assert_eq!(err_code(&mut db, &sql), "0A000", "{sql}");
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
