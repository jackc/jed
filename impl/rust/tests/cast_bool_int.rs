//! boolean ⇄ i32 casts — the deliberate PostgreSQL divergences (spec/types/casts.toml,
//! spec/design/types.md §9). The agreeing behavior (bool→i32, i32→bool, NULL, chains, the
//! literal-adapts-to-i32 rule) is oracle-checked in `suites/cast/bool_int.test` and runs on every
//! core; these per-core tests cover only what the oracle corpus CANNOT express (CLAUDE.md §10):
//!
//!   * the FORBIDDEN width pairs — PG ties the boolean↔integer cast to int4 ONLY, so bool⇄i16 and
//!     bool⇄i64 are not casts. jed reports `42804` (datatype_mismatch — its standing convention for
//!     a forbidden cast pair) where PG reports `42846` (cannot_coerce); an oracle-clean corpus could
//!     not assert the jed code.
//!   * the literal-beyond-i32 corner — `CAST(5000000000 AS boolean)` traps `22003` in jed (the
//!     literal adapts to the i32 the bool cast needs and overflows it) where PG says `42846`.

use jed::value::Value;
use jed::{Database, Outcome, Session, SessionOptions};

fn err_code(db: &mut Session, sql: &str) -> String {
    match db.execute(sql, &[]) {
        Err(e) => e.code().to_string(),
        Ok(_) => panic!("expected error for {sql}"),
    }
}

fn one(db: &mut Session, sql: &str) -> Value {
    match db.execute(sql, &[]).unwrap() {
        Outcome::Query { rows, .. } => rows[0][0].clone(),
        other => panic!("expected query, got {other:?}"),
    }
}

/// bool → i16 and bool → i64 are forbidden (PG has only bool → int4): jed 42804, PG 42846.
#[test]
fn bool_to_non_i32_is_forbidden() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(err_code(&mut db, "SELECT CAST(TRUE AS i16)"), "42804");
    assert_eq!(err_code(&mut db, "SELECT CAST(TRUE AS i64)"), "42804");
    // the PG byte-shorthand spellings resolve to the same widths
    assert_eq!(err_code(&mut db, "SELECT CAST(FALSE AS smallint)"), "42804");
    assert_eq!(err_code(&mut db, "SELECT TRUE::bigint"), "42804");
}

/// i16 → boolean and i64 → boolean are forbidden (PG has only int4 → bool): jed 42804, PG 42846.
/// A column carries the width unambiguously (a bare literal would adapt to i32).
#[test]
fn non_i32_to_bool_is_forbidden() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY, s i16, b i64)", &[])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 5, 9)", &[]).unwrap();
    assert_eq!(
        err_code(&mut db, "SELECT CAST(s AS boolean) FROM t WHERE id = 1"),
        "42804"
    );
    assert_eq!(
        err_code(&mut db, "SELECT b::boolean FROM t WHERE id = 1"),
        "42804"
    );
}

/// An integer literal operand of a boolean target adapts to i32, so a magnitude beyond i32 range
/// traps 22003 (PG reports 42846 — it types the literal as int8 first). A documented divergence.
#[test]
fn literal_beyond_i32_to_bool_overflows() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(
        err_code(&mut db, "SELECT CAST(5000000000 AS boolean)"),
        "22003"
    );
    assert_eq!(err_code(&mut db, "SELECT 5000000000::boolean"), "22003");
}

/// The headline directions still work here (a quick per-core smoke check alongside the divergences;
/// the exhaustive behavior is in the corpus). true→1, false→0, 0→false, nonzero→true, NULL→NULL.
#[test]
fn bool_i32_round_trip_smoke() {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    assert_eq!(one(&mut db, "SELECT CAST(TRUE AS i32)"), Value::Int(1));
    assert_eq!(one(&mut db, "SELECT FALSE::int"), Value::Int(0));
    assert_eq!(
        one(&mut db, "SELECT CAST(0 AS boolean)"),
        Value::Bool(false)
    );
    assert_eq!(one(&mut db, "SELECT (-7)::boolean"), Value::Bool(true));
    assert_eq!(one(&mut db, "SELECT CAST(NULL AS boolean)"), Value::Null);
    // chains compose the two directions
    assert_eq!(one(&mut db, "SELECT 7::boolean::int"), Value::Int(1));
}
