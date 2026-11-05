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

/// These boundaries use jed's full i64 range and integer-family constructor overloads,
/// both deliberate PostgreSQL differences, so they cannot use the oracle corpus.
#[test]
fn datetime_boundary_regressions() {
    use jed::datetime_fn::{date_part_julian, date_trunc_micros};
    use jed::timestamp::{make_timestamp, parse_timestamp};

    let minimum = i64::MIN + 1;
    assert_eq!(
        parse_timestamp("290309-12-21 19:59:05.224193 BC").unwrap(),
        minimum
    );
    assert_eq!(
        make_timestamp(-290309, 12, 21, 19, 59, 5.224193).unwrap(),
        minimum
    );
    for text in [
        "290309-12-21 19:59:05.224192 BC",
        "290309-12-21 19:59:05.224191 BC",
    ] {
        assert_eq!(parse_timestamp(text).unwrap_err().code(), "22008");
    }
    let near_minimum = parse_timestamp("290309-12-21 19:59:06 BC").unwrap();
    for unit in [
        "minute",
        "hour",
        "day",
        "week",
        "month",
        "quarter",
        "year",
        "decade",
        "century",
        "millennium",
    ] {
        assert_eq!(
            date_trunc_micros(unit, near_minimum, false)
                .unwrap_err()
                .code(),
            "22008",
            "{unit}"
        );
    }
    for unit in ["microseconds", "milliseconds", "second"] {
        assert_eq!(
            date_trunc_micros(unit, near_minimum, false).unwrap(),
            near_minimum
        );
    }
    assert_eq!(
        date_trunc_micros("milliseconds", minimum, false)
            .unwrap_err()
            .code(),
        "22008"
    );
    assert_eq!(
        date_part_julian(near_minimum),
        -104311404.0 + 71946.0 / 86400.0
    );

    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for sql in [
        "SELECT make_date('-9223372036854775808'::bigint, 1, 1)",
        "SELECT make_timestamp('-9223372036854775808'::bigint, 1, 1, 0, 0, 0.0::f64)",
        "SELECT make_timestamptz('-9223372036854775808'::bigint, 1, 1, 0, 0, 0.0::f64)",
        "SELECT make_timestamptz('-9223372036854775808'::i64, 1, 1, 0, 0, 0, 'UTC')",
    ] {
        assert_eq!(err_code(&mut db, sql), "22008", "{sql}");
    }
    db.query_outcome(
        "SELECT EXTRACT(julian FROM timestamp '290309-12-21 19:59:06 BC')",
        &[],
    )
    .unwrap();
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

// Calendar arithmetic must preserve the earliest finite second just as input
// construction does. These BC dates are outside PostgreSQL's supported range.
#[test]
fn timestamp_month_shift_preserves_earliest_finite_second() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for ty in ["timestamp", "timestamptz"] {
        for shift in ["- interval '1 month'", "+ interval '-1 month'"] {
            let sql = format!(
                "SELECT ({ty} '290308-01-21 19:59:05.224193 BC' {shift}) = {ty} '290309-12-21 19:59:05.224193 BC'"
            );
            match db.query_outcome(&sql, &[]).unwrap() {
                jed::Outcome::Query { rows, .. } => {
                    assert_eq!(rows, vec![vec![jed::Value::Bool(true)]], "{sql}");
                }
                other => panic!("expected query, got {other:?}"),
            }
        }
        let sql = format!("SELECT {ty} '290308-01-21 19:59:05.224192 BC' - interval '1 month'");
        assert_eq!(err_code(&mut db, &sql), "22008", "{sql}");
    }
}

// PostgreSQL checks each completed calendar step before applying later fields.
// These tests use jed's own finite boundaries, which differ from PostgreSQL's.
#[test]
fn timestamp_calendar_steps_reject_nonfinite_intermediates() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for ty in ["timestamp", "timestamptz"] {
        for (input, forward, reverse) in [
            (
                "294246-12-10 04:00:54.775807",
                "1 month -1 day",
                "-1 month 1 day",
            ),
            (
                "294246-12-10 04:00:54.775808",
                "1 month -1 day",
                "-1 month 1 day",
            ),
            (
                "290308-01-21 19:59:05.224192 BC",
                "-1 month 1 day",
                "1 month -1 day",
            ),
            (
                "290308-01-21 19:59:05.224191 BC",
                "-1 month 1 day",
                "1 month -1 day",
            ),
            (
                "294247-01-09 04:00:54.775807",
                "1 day -1 second",
                "-1 day 1 second",
            ),
            (
                "294247-01-09 04:00:54.775808",
                "1 day -1 second",
                "-1 day 1 second",
            ),
            (
                "290309-12-22 19:59:05.224192 BC",
                "-1 day 1 second",
                "1 day -1 second",
            ),
            (
                "290309-12-22 19:59:05.224191 BC",
                "-1 day 1 second",
                "1 day -1 second",
            ),
        ] {
            for (op, interval) in [("+", forward), ("-", reverse)] {
                let sql = format!("SELECT {ty} '{input}' {op} interval '{interval}'");
                assert_eq!(err_code(&mut db, &sql), "22008", "{sql}");
            }
        }
    }
}
