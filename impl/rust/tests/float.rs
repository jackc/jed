//! The `float32` / `float64` IEEE 754 types, end to end through `execute`
//! (spec/design/float.md). The cross-core contract is asserted on the RENDERED output (the `R`
//! tag tolerates layout, but these finite values render identically), the total order, the trap
//! model, strict-island coercion, the casts, the canonical-order-fold SUM/AVG, and a transcendental.

use jed::{Database, Outcome, execute};

fn db_with(stmts: &[&str]) -> Database {
    let mut db = Database::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn rendered(db: &mut Database, sql: &str) -> Vec<Vec<String>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn one(db: &mut Database, sql: &str) -> String {
    let rows = rendered(db, sql);
    assert_eq!(rows.len(), 1, "{sql:?} should return one row");
    assert_eq!(rows[0].len(), 1, "{sql:?} should return one column");
    rows[0][0].clone()
}

fn col_types(db: &mut Database, sql: &str) -> Vec<String> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { column_types, .. } => column_types,
        Outcome::Statement { .. } => panic!("expected a query for {sql:?}"),
    }
}

fn err_code(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
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
    // `real` → float32; `float` → float64 (the single-word aliases the parser accepts; the
    // two-word `double precision` is a from_name alias but, like `timestamp without time zone`,
    // not produced by this slice's single-identifier type parser — a documented narrowing).
    // PG's float8/float4/float(p) are NOT accepted (we own our surface).
    let mut db = db_with(&[
        "CREATE TABLE t (a real, b float)",
        "INSERT INTO t VALUES (1.5, 2.5)",
    ]);
    assert_eq!(
        col_types(&mut db, "SELECT a, b FROM t"),
        vec!["float32", "float64"]
    );
    // The canonical ids resolve too.
    let mut db2 = db_with(&["CREATE TABLE t (a float32, b float64)"]);
    assert_eq!(
        col_types(&mut db2, "SELECT * FROM t"),
        vec!["float32", "float64"]
    );
    assert!(execute(&mut db, "CREATE TABLE u (x float8)").is_err());
    assert!(execute(&mut db, "CREATE TABLE u (x float4)").is_err());
    assert!(execute(&mut db, "CREATE TABLE u (x float(10))").is_err());
}

#[test]
fn mixed_width_arithmetic_promotes_to_float64() {
    let mut db = db_with(&[
        "CREATE TABLE t (f float64, g float32)",
        "INSERT INTO t VALUES (1.5, 2.5)",
    ]);
    // float32 + float64 → float64 (the tower); float32 + float32 stays float32.
    assert_eq!(col_types(&mut db, "SELECT f + g FROM t"), vec!["float64"]);
    assert_eq!(col_types(&mut db, "SELECT g + g FROM t"), vec!["float32"]);
    assert_eq!(one(&mut db, "SELECT f + g FROM t"), "4");
}

// ---------------------------------------------------------------------------------------------
// The TOTAL order: -0 = +0, NaN = NaN (TRUE), NaN largest — DISTINCT / GROUP BY collapse
// ---------------------------------------------------------------------------------------------

#[test]
fn distinct_and_group_by_collapse_neg_zero_and_nan() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, f float64)",
        "INSERT INTO t VALUES (1, 0.0), (2, 0.0), (3, 0.0), (4, 0.0), (5, 1.5)",
        "UPDATE t SET f = -CAST(0.0 AS float64) WHERE id = 2", // -0.0
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
    let mut db = Database::new();
    assert_eq!(one(&mut db, "SELECT float 'Infinity'"), "Infinity");
    assert_eq!(one(&mut db, "SELECT float '-Infinity'"), "-Infinity");
    assert_eq!(one(&mut db, "SELECT float 'NaN'"), "NaN");
    // -0 renders -0 (a genuine float negative zero, via negation of +0).
    assert_eq!(one(&mut db, "SELECT -CAST(0.0 AS float64)"), "-0");
    // The case-insensitive special spellings all parse.
    assert_eq!(one(&mut db, "SELECT float 'inf'"), "Infinity");
    assert_eq!(one(&mut db, "SELECT float '-inf'"), "-Infinity");
    assert_eq!(one(&mut db, "SELECT float 'nan'"), "NaN");
    // Malformed → 22P02.
    assert_eq!(err_code(&mut db, "SELECT float 'not a float'"), "22P02");
}
