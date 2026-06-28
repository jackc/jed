//! Runtime text → numeric/boolean casts — the parts the PG-clean oracle corpus cannot express
//! (the runtime-text-cast slice; spec/design/grammar.md §36, spec/design/types.md §5,
//! spec/types/casts.toml). The accepted-grammar int/decimal/boolean cases AGREE with PostgreSQL and
//! are oracle-checked in `suites/cast/text_to_scalar.test` (run on every core); this file covers
//! only what that corpus cannot: (a) the jed-stricter grammar DIVERGENCES — hex / digit-underscore /
//! NaN trap 22P02 where PG accepts them, so they cannot live in the PG-clean corpus — and (b) the
//! runtime text → f32/f64 cast, kept out of the corpus because the float renderer is in the
//! determinism-exception ledger. Every cast below is on a NON-LITERAL text expression (a text
//! COLUMN), so it exercises the per-row `evalCast` path, not the resolve-time literal fold.

use jed::value::Value;
use jed::{Engine, Outcome, execute};

/// Build a one-column text table `t(id i32 pk, s text)` seeded with `rows` (id = 1.., s = each str).
fn seeded(rows: &[&str]) -> Engine {
    let mut db = Engine::new();
    execute(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, s text)").unwrap();
    for (i, s) in rows.iter().enumerate() {
        execute(
            &mut db,
            &format!("INSERT INTO t VALUES ({}, '{}')", i + 1, s),
        )
        .unwrap();
    }
    db
}

/// The scalar value of `SELECT <expr> FROM t WHERE id = <id>`.
fn at(db: &mut Engine, expr: &str, id: usize) -> Value {
    match execute(db, &format!("SELECT {expr} FROM t WHERE id = {id}")).unwrap() {
        Outcome::Query { rows, .. } => rows[0][0].clone(),
        other => panic!("expected query, got {other:?}"),
    }
}

/// The SQLSTATE of a query expected to error per row.
fn err_at(db: &mut Engine, expr: &str, id: usize) -> String {
    match execute(db, &format!("SELECT {expr} FROM t WHERE id = {id}")) {
        Err(e) => e.code().to_string(),
        Ok(_) => panic!("expected error for {expr} (id {id})"),
    }
}

// --- (a) jed-stricter grammar divergences on the RUNTIME path -------------------------------------
// These trap 22P02 in jed but PostgreSQL ACCEPTS them, so they cannot be oracle-checked. The point
// is to prove the runtime `evalCast` coercion uses jed's OWN literal grammar (identical to the
// resolve-time literal form), not PG's input-function extras.

#[test]
fn hex_integer_string_traps_22p02_at_runtime() {
    // PG: '0x10'::int4 → 16. jed: decimal digits only → 22P02.
    let mut db = seeded(&["0x10"]);
    assert_eq!(err_at(&mut db, "s :: int", 1), "22P02");
}

#[test]
fn digit_underscore_integer_string_traps_22p02_at_runtime() {
    // PG: '1_000'::int4 → 1000. jed: no underscores → 22P02.
    let mut db = seeded(&["1_000"]);
    assert_eq!(err_at(&mut db, "s :: int", 1), "22P02");
}

#[test]
fn nan_numeric_string_traps_22p02_at_runtime() {
    // PG: 'NaN'::numeric → NaN. jed's decimal is always finite → 22P02 (decimal.md §2).
    let mut db = seeded(&["NaN"]);
    assert_eq!(err_at(&mut db, "s :: numeric", 1), "22P02");
}

// --- (b) runtime text → f32/f64 (out of the corpus: float render is determinism-exempt) ----------

#[test]
fn text_to_f64_finite() {
    let mut db = seeded(&["1.5", "-0.25", "100", "1e3"]);
    assert_eq!(at(&mut db, "s :: float8", 1), Value::Float64(1.5));
    assert_eq!(at(&mut db, "s :: float8", 2), Value::Float64(-0.25));
    assert_eq!(at(&mut db, "s :: float8", 3), Value::Float64(100.0));
    // scientific notation reaches the float grammar (1e3 → 1000.0)
    assert_eq!(at(&mut db, "s :: float8", 4), Value::Float64(1000.0));
}

#[test]
fn text_to_f32_frounds() {
    let mut db = seeded(&["0.5", "3.14"]);
    assert_eq!(at(&mut db, "s :: float4", 1), Value::Float32(0.5));
    // 3.14 rounds to the nearest binary32
    assert_eq!(at(&mut db, "s :: float4", 2), Value::Float32(3.14_f32));
}

#[test]
fn text_to_float_special_words() {
    let mut db = seeded(&["NaN", "Infinity", "-inf"]);
    // float (unlike decimal) accepts the IEEE special words — they are first-class values.
    match at(&mut db, "s :: float8", 1) {
        Value::Float64(f) => assert!(f.is_nan()),
        other => panic!("expected NaN float, got {other:?}"),
    }
    assert_eq!(at(&mut db, "s :: float8", 2), Value::Float64(f64::INFINITY));
    assert_eq!(
        at(&mut db, "s :: float8", 3),
        Value::Float64(f64::NEG_INFINITY)
    );
}

#[test]
fn text_to_float_overflow_and_malformed() {
    let mut db = seeded(&["1e400", "abc"]);
    // a FINITE literal beyond the binary64 range traps 22003 (not ±Inf — the finite-overflow rule)
    assert_eq!(err_at(&mut db, "s :: float8", 1), "22003");
    // junk traps 22P02
    assert_eq!(err_at(&mut db, "s :: float8", 2), "22P02");
}

#[test]
fn text_to_float_null_propagates() {
    let mut db = Engine::new();
    execute(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, s text)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1, NULL)").unwrap();
    assert_eq!(at(&mut db, "s :: float8", 1), Value::Null);
}
