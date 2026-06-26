//! The three array-involving casts — the parts the PG-clean oracle corpus cannot express (the array
//! cast follow-ons; spec/design/array.md §7, spec/types/casts.toml). The numeric/text element pairs
//! AGREE with PostgreSQL and are oracle-checked in `suites/cast/array_casts.test` (run on every
//! core); this file covers only what that corpus cannot:
//!   (a) array → text is EXPLICIT-only — an assignment/implicit context stays 42804 (stricter than
//!       PG's assignment-cast-to-text, the same convention as uuid/json → text);
//!   (b) the jed-only element casts uuid⇄bytea, which SUCCEED where PG errors (42846);
//!   (c) the forbidden scalar element pair → 42804 (jed's strict-matrix convention; PG reports
//!       42846) and a composite-element array cast → 0A000;
//!   (d) runtime text → f32[]/f64[] element casts, kept out of the corpus because the float renderer
//!       is in the determinism-exception ledger.

use jed::value::Value;
use jed::{Database, Outcome, execute};

/// The rendered scalar of `SELECT <expr>` (single row, single column).
fn scalar(db: &mut Database, expr: &str) -> String {
    match execute(db, &format!("SELECT {expr}")).unwrap() {
        Outcome::Query { rows, .. } => rows[0][0].render(),
        other => panic!("expected query, got {other:?}"),
    }
}

/// The SQLSTATE of a statement expected to error.
fn err(db: &mut Database, sql: &str) -> String {
    match execute(db, sql) {
        Err(e) => e.code().to_string(),
        Ok(o) => panic!("expected error for {sql}, got {o:?}"),
    }
}

// --- (a) array → text is EXPLICIT-only -----------------------------------------------------------
// The explicit CAST/`::` spelling works (oracle-checked in the corpus); an ASSIGNMENT context
// (INSERT into a text column) or an IMPLICIT one (text comparison) does NOT silently coerce — it
// stays 42804, stricter than PG. This is the strict-matrix convention (uuid/json → text are the
// precedent), and it cannot live in the PG-clean corpus (PG accepts the assignment).

#[test]
fn array_to_text_is_explicit_only() {
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, label text)").unwrap();
    // Assignment context: an array value into a text column is a datatype mismatch, NOT a silent
    // array_out (PG would assignment-cast it).
    assert_eq!(
        err(&mut db, "INSERT INTO t VALUES (1, ARRAY[1,2,3])"),
        "42804"
    );
    // Implicit context: comparing a text column to an array value is a mismatch.
    execute(&mut db, "INSERT INTO t VALUES (1, '{1,2,3}')").unwrap();
    assert_eq!(
        err(&mut db, "SELECT id FROM t WHERE label = ARRAY[1,2,3]"),
        "42804"
    );
    // The explicit cast, by contrast, succeeds.
    assert_eq!(scalar(&mut db, "(ARRAY[1,2,3])::text"), "{1,2,3}");
}

// --- (b) the jed-only element casts uuid ⇄ bytea (succeed where PG errors) ------------------------

#[test]
fn uuid_array_to_bytea_array_and_back() {
    let mut db = Database::new();
    // uuid[] → bytea[]: each element is its 16 raw bytes; bytea[] → uuid[] reverses it. PG has no
    // bytea⇄uuid cast at all (42846), so this whole round-trip is jed-only.
    let round = scalar(
        &mut db,
        "((ARRAY['00000000-0000-0000-0000-000000000001']::uuid[])::bytea[])::uuid[] = \
          ARRAY['00000000-0000-0000-0000-000000000001']::uuid[]",
    );
    assert_eq!(round, "true");
    // A bytea[] element of the wrong width on bytea[] → uuid[] traps 22P02 per element (a uuid is
    // exactly 16 bytes). The bytea[] source is built from a cast bytea element literal (text →
    // bytea is not in the matrix, so a bare `'\x00'` string array would itself be 42804).
    assert_eq!(
        err(&mut db, "SELECT (ARRAY['\\x00'::bytea])::uuid[]"),
        "22P02",
    );
}

// --- (c) forbidden scalar element pair (42804) + composite-element array cast (0A000) -------------

#[test]
fn forbidden_element_pairs() {
    let mut db = Database::new();
    // A scalar element pair with no cast between the element types → 42804 (jed's strict-matrix
    // convention; PG reports 42846). i32 → timestamp has no cast.
    assert_eq!(
        err(&mut db, "SELECT (ARRAY[1,2,3]::i32[])::timestamp[]"),
        "42804",
    );
    // A composite-element array cast is the deferred composite cast surface → 0A000.
    execute(&mut db, "CREATE TYPE addr AS (street text, zip i32)").unwrap();
    assert_eq!(
        err(
            &mut db,
            "SELECT (ARRAY[ROW('Main',90210)::addr]::addr[])::text[]"
        ),
        "0A000",
    );
    // A bind parameter into an array type stays the container-param narrowing (0A000).
    assert_eq!(err(&mut db, "SELECT $1::i32[]"), "0A000");
}

// --- (d) runtime text → f32[] / f64[] element casts (float renderer is determinism-exempt) -------

#[test]
fn runtime_text_to_float_arrays() {
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE t (id i32 PRIMARY KEY, s text)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1, '{0.5,0.25,-1.5}')").unwrap();
    // text → f64[] (binary64-exact values render exactly).
    let got = match execute(&mut db, "SELECT (s::float8[])::text FROM t WHERE id = 1").unwrap() {
        Outcome::Query { rows, .. } => rows[0][0].render(),
        other => panic!("{other:?}"),
    };
    assert_eq!(got, "{0.5,0.25,-1.5}");
    // text → f32[] then widen to f64[] (0.5/0.25 are exact in binary32).
    assert_eq!(
        scalar(
            &mut db,
            "(((ARRAY['0.5','0.25']::text[])::float4[])::float8[])::text"
        ),
        "{0.5,0.25}",
    );
    // i32[] → f64[] element-wise (numeric → float).
    assert_eq!(
        scalar(&mut db, "((ARRAY[1,2,3]::i32[])::float8[])::text"),
        "{1,2,3}"
    );
}
