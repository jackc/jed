//! The `f32` / `f64` IEEE 754 types, end to end through `execute`
//! (spec/design/float.md). The cross-core contract is asserted on the RENDERED output (the `R`
//! tag tolerates layout, but these finite values render identically), the total order, the trap
//! model, strict-island coercion, the casts, the canonical-order-fold SUM/AVG, and a transcendental.

use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

fn db_with(stmts: &[&str]) -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for s in stmts {
        db.execute(s, &[])
            .unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn rendered(db: &mut Session, sql: &str) -> Vec<Vec<String>> {
    match db
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
    {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn one(db: &mut Session, sql: &str) -> String {
    let rows = rendered(db, sql);
    assert_eq!(rows.len(), 1, "{sql:?} should return one row");
    assert_eq!(rows[0].len(), 1, "{sql:?} should return one column");
    rows[0][0].clone()
}

fn col_types(db: &mut Session, sql: &str) -> Vec<String> {
    match db
        .execute(sql, &[])
        .unwrap_or_else(|e| panic!("{sql:?}: {}", e.message))
    {
        Outcome::Query { column_types, .. } => column_types,
        Outcome::Statement { .. } => panic!("expected a query for {sql:?}"),
    }
}

fn err_code(db: &mut Session, sql: &str) -> String {
    db.execute(sql, &[])
        .err()
        .unwrap_or_else(|| panic!("{sql:?} should have failed"))
        .code()
        .to_string()
}

// ---------------------------------------------------------------------------------------------
// Names / aliases / the promotion tower
// ---------------------------------------------------------------------------------------------

#[test]
fn aliases_resolve_and_rejected_spellings_fail() {
    // `real` → f32; `float` → f64 (the single-word aliases the parser accepts; the
    // two-word `double precision` is a from_name alias but, like `timestamp without time zone`,
    // not produced by this slice's single-identifier type parser — a documented narrowing).
    // PG's byte-shorthand float8 → f64 / float4 → f32 IS accepted (the f prefix keeps jed's
    // bit-namespace disjoint from PG's byte-namespace — CLAUDE.md §1/§4); the `float(p)`
    // precision typmod is still NOT accepted.
    let mut db = db_with(&[
        "CREATE TABLE t (a real, b float)",
        "INSERT INTO t VALUES (1.5, 2.5)",
    ]);
    assert_eq!(col_types(&mut db, "SELECT a, b FROM t"), vec!["f32", "f64"]);
    // The canonical ids resolve too.
    let mut db2 = db_with(&["CREATE TABLE t (a f32, b f64)"]);
    assert_eq!(col_types(&mut db2, "SELECT * FROM t"), vec!["f32", "f64"]);
    // PG byte-shorthand resolves: float4 → f32, float8 → f64.
    let mut db3 = db_with(&["CREATE TABLE t (a float4, b float8)"]);
    assert_eq!(col_types(&mut db3, "SELECT * FROM t"), vec!["f32", "f64"]);
    // The `float(p)` precision typmod is still rejected.
    assert!(db.execute("CREATE TABLE u (x float(10))", &[]).is_err());
}

#[test]
fn mixed_width_arithmetic_promotes_to_float64() {
    let mut db = db_with(&[
        "CREATE TABLE t (f f64, g f32)",
        "INSERT INTO t VALUES (1.5, 2.5)",
    ]);
    // f32 + f64 → f64 (the tower); f32 + f32 stays f32.
    assert_eq!(col_types(&mut db, "SELECT f + g FROM t"), vec!["f64"]);
    assert_eq!(col_types(&mut db, "SELECT g + g FROM t"), vec!["f32"]);
    assert_eq!(one(&mut db, "SELECT f + g FROM t"), "4");
}

// ---------------------------------------------------------------------------------------------
// The TOTAL order: -0 = +0, NaN = NaN (TRUE), NaN largest — DISTINCT / GROUP BY collapse
// ---------------------------------------------------------------------------------------------

#[test]
fn distinct_and_group_by_collapse_neg_zero_and_nan() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, f f64)",
        "INSERT INTO t VALUES (1, 0.0), (2, 0.0), (3, 0.0), (4, 0.0), (5, 1.5)",
        "UPDATE t SET f = -CAST(0.0 AS f64) WHERE id = 2", // -0.0
        "UPDATE t SET f = float 'NaN' WHERE id = 3",
        "UPDATE t SET f = float 'NaN' WHERE id = 4", // a second NaN
    ]);
    // DISTINCT: {+0 (collapses -0), NaN (both collapse), 1.5} = 3 groups.
    let distinct = rendered(&mut db, "SELECT DISTINCT f FROM t ORDER BY f");
    assert_eq!(
        distinct.len(),
        3,
        "distinct collapses -0/+0 and the two NaNs"
    );
    // GROUP BY: the zero group has 2 rows (id 1,2), the NaN group has 2 (id 3,4), 1.5 has 1.
    let groups = rendered(&mut db, "SELECT f, count(*) FROM t GROUP BY f ORDER BY f");
    let counts: Vec<String> = groups.iter().map(|r| r[1].clone()).collect();
    assert_eq!(counts, vec!["2", "1", "2"]); // 0(x2) < 1.5(x1) < NaN(x2)
}

// ---------------------------------------------------------------------------------------------
// Rendering of special values
// ---------------------------------------------------------------------------------------------

#[test]
fn rendering_of_special_values() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(one(&mut db, "SELECT float 'Infinity'"), "Infinity");
    assert_eq!(one(&mut db, "SELECT float '-Infinity'"), "-Infinity");
    assert_eq!(one(&mut db, "SELECT float 'NaN'"), "NaN");
    // -0 renders -0 (a genuine float negative zero, via negation of +0).
    assert_eq!(one(&mut db, "SELECT -CAST(0.0 AS f64)"), "-0");
    // The case-insensitive special spellings all parse.
    assert_eq!(one(&mut db, "SELECT float 'inf'"), "Infinity");
    assert_eq!(one(&mut db, "SELECT float '-inf'"), "-Infinity");
    assert_eq!(one(&mut db, "SELECT float 'nan'"), "NaN");
    // Malformed → 22P02.
    assert_eq!(err_code(&mut db, "SELECT float 'not a float'"), "22P02");
}
