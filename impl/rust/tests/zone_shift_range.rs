//! Zone-shift range guard (timezones.md §6): every path that moves a finite timestamp across a UT
//! offset — the cross-family casts, `AT TIME ZONE`, `date_trunc`/`EXTRACT`/`date_part` in a zone,
//! `make_timestamptz`, and the `timestamptz → text` render — traps `22008` when the shifted reading
//! leaves jed's i64-µs range or lands exactly on a `±infinity` sentinel (`i64::MIN`/`MAX`). jed's
//! timestamp range (≈ 290309 BC … 294247 AD) is far wider than PostgreSQL's, so none of these
//! extremes are oracle-checkable; this file pins them per core. Mirrors
//! impl/go/zone_shift_range_test.go and impl/ts/tests/zone_shift_range.test.ts.

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

// i64::MAX µs is 294247-01-10 04:00:54.775807 UTC; i64::MIN µs is 290309-12-21 19:59:05.224192 BC.
// Each seeded value sits within a few hours of an end. Session zones use the POSIX fixed-offset
// spelling, where a positive offset is WEST of UTC ('+05:00' = UTC-5, '-05:00' = UTC+5).
const TS_HI: &str = "294247-01-10 04:00:00"; // shifts past i64::MAX in a UTC-5 reading
const TS_LO: &str = "290309-12-22 00:00:00 BC"; // shifts past i64::MIN in a UTC+5 reading
const TS_LO_EDGE: &str = "290309-12-22 00:59:05.224192 BC"; // i64::MIN + 5h exactly
const TZ_HI: &str = "294247-01-10 04:00:00+00"; // + 5h overflows
const TZ_HI_EDGE: &str = "294247-01-09 23:00:54.775807+00"; // i64::MAX − 5h exactly
const TZ_LO: &str = "290309-12-22 00:00:00+00 BC"; // − 5h overflows

/// `t(id i32 pk, ts timestamp, tz timestamptz, d date)` with the near-range rows, session zone UTC.
fn seeded() -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for sql in [
        "CREATE TABLE t (id i32 PRIMARY KEY, ts timestamp, tz timestamptz, d date)".to_string(),
        format!("INSERT INTO t VALUES (1, '{TS_HI}', '{TZ_HI}', '294247-01-10')"),
        format!("INSERT INTO t VALUES (2, '{TS_LO}', '{TZ_LO}', '290309-12-22 BC')"),
        format!("INSERT INTO t VALUES (3, '{TS_LO_EDGE}', '{TZ_HI_EDGE}', '300000-01-01')"),
    ] {
        db.query_outcome(&sql, &[]).unwrap();
    }
    db
}

fn at(db: &mut Session, expr: &str, id: i32) -> Result<Value, String> {
    match db.query_outcome(&format!("SELECT {expr} FROM t WHERE id = {id}"), &[]) {
        Ok(Outcome::Query { rows, .. }) => Ok(rows[0][0].clone()),
        Ok(other) => panic!("expected query, got {other:?}"),
        Err(e) => Err(e.code().to_string()),
    }
}

fn traps(db: &mut Session, zone: &str, expr: &str, id: i32) {
    db.set_time_zone(zone).unwrap();
    match at(db, expr, id) {
        Err(code) => assert_eq!(code, "22008", "{zone} {expr} (row {id})"),
        Ok(v) => panic!("{zone} {expr} (row {id}): expected 22008, got {v:?}"),
    }
}

#[test]
fn shift_past_i64_max_traps_22008() {
    let mut db = seeded();
    // A wall clock read in a UTC-5 zone moves +5h.
    for expr in [
        "ts :: timestamptz",
        "ts AT TIME ZONE '+05:00'",
        "make_timestamptz(294247, 1, 10, 4, 0, 0)",
        "d :: timestamptz", // midnight, read in the session zone
    ] {
        traps(&mut db, "+05:00", expr, 1);
    }
    traps(
        &mut db,
        "UTC",
        "make_timestamptz(294247, 1, 10, 4, 0, 0, '+05:00')",
        1,
    );
    // An instant rendered in a UTC+5 zone moves +5h.
    for expr in [
        "tz :: timestamp",
        "tz :: date",
        "tz :: text",
        "tz AT TIME ZONE '-05:00'",
        "date_trunc('day', tz)",
        "EXTRACT(hour FROM tz)",
        "date_part('hour', tz)",
    ] {
        traps(&mut db, "-05:00", expr, 1);
    }
    traps(&mut db, "UTC", "date_trunc('day', tz, '-05:00')", 1);
}

#[test]
fn shift_past_i64_min_traps_22008() {
    let mut db = seeded();
    // A wall clock read in a UTC+5 zone moves −5h.
    for expr in [
        "ts :: timestamptz",
        "ts AT TIME ZONE '-05:00'",
        "d :: timestamptz",
    ] {
        traps(&mut db, "-05:00", expr, 2);
    }
    // An instant rendered in a UTC-5 zone moves −5h.
    for expr in [
        "tz :: timestamp",
        "tz :: date",
        "tz :: text",
        "tz AT TIME ZONE '+05:00'",
        "date_trunc('day', tz)",
        "EXTRACT(hour FROM tz)",
        "date_part('hour', tz)",
    ] {
        traps(&mut db, "+05:00", expr, 2);
    }
}

#[test]
fn shift_onto_an_infinity_sentinel_traps_22008() {
    let mut db = seeded();
    // Landing EXACTLY on i64::MIN / i64::MAX would silently become ±infinity.
    traps(&mut db, "-05:00", "ts :: timestamptz", 3);
    traps(&mut db, "UTC", "ts AT TIME ZONE '-05:00'", 3);
    traps(&mut db, "-05:00", "tz :: timestamp", 3);
    traps(&mut db, "-05:00", "tz :: text", 3);
    traps(&mut db, "UTC", "tz AT TIME ZONE '-05:00'", 3);
    // One microsecond inside the range is representable.
    db.set_time_zone("-05:00").unwrap();
    let v = at(&mut db, "(tz - INTERVAL '1 microsecond') :: timestamp", 3).unwrap();
    assert_eq!(v, Value::Timestamp(i64::MAX - 1));
}

#[test]
fn date_widening_past_the_timestamp_range_traps_22008() {
    // A date is wider than the timestamp range (date.md §1): its midnight widening must trap, not
    // wrap the day × µs/day product.
    let mut db = seeded();
    traps(&mut db, "UTC", "d :: timestamp", 3);
    traps(&mut db, "UTC", "d :: timestamptz", 3);
}

#[test]
fn the_same_values_shift_inward_without_error() {
    // The guard rejects only an out-of-range result: the opposite-signed zone moves each value
    // toward the epoch and succeeds.
    let mut db = seeded();
    db.set_time_zone("-05:00").unwrap();
    let v = at(&mut db, "ts :: timestamptz", 1).unwrap();
    assert!(matches!(v, Value::Timestamptz(_)), "{v:?}");
    let v = at(&mut db, "tz :: text", 2).unwrap();
    assert!(matches!(v, Value::Text(_)), "{v:?}");
    db.set_time_zone("+05:00").unwrap();
    let v = at(&mut db, "tz :: timestamp", 1).unwrap();
    assert!(matches!(v, Value::Timestamp(_)), "{v:?}");
    let v = at(&mut db, "ts :: timestamptz", 2).unwrap();
    assert!(matches!(v, Value::Timestamptz(_)), "{v:?}");
}
