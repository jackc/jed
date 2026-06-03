//! Phase 3: the exact `decimal` / `numeric` type, end to end through `execute`
//! (spec/design/decimal.md). Assertions are on the **rendered** output — the cross-core
//! contract — since decimal value-equality (1.5 == 1.50) is intentionally scale-insensitive.

use jed::{Database, Outcome, execute};

fn db_with(stmts: &[&str]) -> Database {
    let mut db = Database::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

/// Run a query and render every cell to its canonical string (row-major).
fn rendered(db: &mut Database, sql: &str) -> Vec<Vec<String>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows
            .iter()
            .map(|r| r.iter().map(|v| v.render()).collect())
            .collect(),
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

/// A single-cell query result, rendered.
fn one(db: &mut Database, sql: &str) -> String {
    let rows = rendered(db, sql);
    assert_eq!(rows.len(), 1, "{sql:?} should return one row");
    assert_eq!(rows[0].len(), 1, "{sql:?} should return one column");
    rows[0][0].clone()
}

/// The SQLSTATE of a statement expected to error.
fn err_code(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
        .err()
        .unwrap_or_else(|| panic!("{sql:?} should have failed"))
        .code()
        .to_string()
}

#[test]
fn storage_preserves_display_scale() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, v numeric)",
        "INSERT INTO t VALUES (1, 1.50), (2, 1.5), (3, 0.00), (4, -0.013), (5, 123), (6, NULL)",
    ]);
    assert_eq!(one(&mut db, "SELECT v FROM t WHERE id = 1"), "1.50");
    assert_eq!(one(&mut db, "SELECT v FROM t WHERE id = 2"), "1.5");
    assert_eq!(one(&mut db, "SELECT v FROM t WHERE id = 3"), "0.00");
    assert_eq!(one(&mut db, "SELECT v FROM t WHERE id = 4"), "-0.013");
    assert_eq!(one(&mut db, "SELECT v FROM t WHERE id = 5"), "123");
    assert_eq!(one(&mut db, "SELECT v FROM t WHERE id = 6"), "NULL");
}

#[test]
fn numeric_p_s_rounds_and_pads_on_store() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, money numeric(10,2))",
        "INSERT INTO t VALUES (1, 1.5), (2, 1.555), (3, 1.554), (4, 5), (5, -2.5)",
    ]);
    assert_eq!(one(&mut db, "SELECT money FROM t WHERE id = 1"), "1.50");
    assert_eq!(one(&mut db, "SELECT money FROM t WHERE id = 2"), "1.56"); // half away
    assert_eq!(one(&mut db, "SELECT money FROM t WHERE id = 3"), "1.55");
    assert_eq!(one(&mut db, "SELECT money FROM t WHERE id = 4"), "5.00"); // int → decimal
    assert_eq!(one(&mut db, "SELECT money FROM t WHERE id = 5"), "-2.50");
}

#[test]
fn precision_overflow_on_store_traps_22003() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, v numeric(3,2))"]);
    // integer part may have at most p - s = 1 digit; 12.34 has 2 → 22003.
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (1, 12.34)"),
        "22003"
    );
    // a value that rounds up into an extra integer digit also overflows.
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (2, 9.999)"),
        "22003"
    );
}

#[test]
fn comparison_by_value_ignores_scale() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, v numeric)",
        "INSERT INTO t VALUES (1, 1.50), (2, 2.5), (3, 10.0)",
    ]);
    // 1.50 = 1.5 by value
    assert_eq!(rendered(&mut db, "SELECT id FROM t WHERE v = 1.5"), [["1"]]);
    assert_eq!(
        rendered(&mut db, "SELECT id FROM t WHERE v = 1.500"),
        [["1"]]
    );
    // integer ↔ decimal comparison (cross-family promotion)
    assert_eq!(rendered(&mut db, "SELECT id FROM t WHERE v = 10"), [["3"]]);
    assert_eq!(
        rendered(&mut db, "SELECT id FROM t WHERE v > 2 ORDER BY id"),
        [["2"], ["3"]]
    );
}

#[test]
fn order_by_decimal_is_numeric_with_nulls_last() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, v numeric)",
        "INSERT INTO t VALUES (1, 10), (2, -1), (3, 0.5), (4, -10.5), (5, NULL), (6, 1.50)",
    ]);
    // -10.5 < -1 < 0.5 < 1.50 < 10, NULL last
    assert_eq!(
        rendered(&mut db, "SELECT id FROM t ORDER BY v"),
        [["4"], ["2"], ["3"], ["6"], ["1"], ["5"]]
    );
}

#[test]
fn arithmetic_scale_rules() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, a numeric, b numeric)",
        "INSERT INTO t VALUES (1, 1.50, 1.5), (2, 2.0, 3.000), (3, 1.234, 1.2)",
    ]);
    assert_eq!(one(&mut db, "SELECT a + b FROM t WHERE id = 1"), "3.00"); // max(s1,s2)
    assert_eq!(one(&mut db, "SELECT a - b FROM t WHERE id = 3"), "0.034");
    assert_eq!(one(&mut db, "SELECT a * b FROM t WHERE id = 1"), "2.250"); // s1+s2
    assert_eq!(one(&mut db, "SELECT a * b FROM t WHERE id = 2"), "6.0000");
    // division: PG select_div_scale + half-away rounding
    assert_eq!(
        one(&mut db, "SELECT a / b FROM t WHERE id = 2"),
        "0.66666666666666666667"
    );
}

#[test]
fn division_and_modulo() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, a numeric, b numeric)",
        "INSERT INTO t VALUES (1, 1, 3), (2, 10.0, 4.0), (3, -5.5, 2)",
    ]);
    assert_eq!(
        one(&mut db, "SELECT a / b FROM t WHERE id = 1"),
        "0.33333333333333333333"
    );
    assert_eq!(
        one(&mut db, "SELECT a / b FROM t WHERE id = 2"),
        "2.5000000000000000"
    );
    assert_eq!(one(&mut db, "SELECT a % b FROM t WHERE id = 3"), "-1.5");
    assert_eq!(
        err_code(&mut db, "SELECT a / 0 FROM t WHERE id = 1"),
        "22012"
    );
    assert_eq!(
        err_code(&mut db, "SELECT a % 0 FROM t WHERE id = 1"),
        "22012"
    );
}

#[test]
fn mixed_integer_decimal_arithmetic() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, i int32, d numeric)",
        "INSERT INTO t VALUES (1, 3, 1.5)",
    ]);
    assert_eq!(one(&mut db, "SELECT i + d FROM t WHERE id = 1"), "4.5");
    assert_eq!(one(&mut db, "SELECT d * i FROM t WHERE id = 1"), "4.5");
    assert_eq!(one(&mut db, "SELECT i - d FROM t WHERE id = 1"), "1.5");
}

#[test]
fn casts_int_decimal_both_ways() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, i int32, d numeric)",
        "INSERT INTO t VALUES (1, 7, 2.5)",
    ]);
    assert_eq!(
        one(&mut db, "SELECT CAST(i AS numeric) FROM t WHERE id = 1"),
        "7"
    );
    assert_eq!(
        one(
            &mut db,
            "SELECT CAST(i AS numeric(10,2)) FROM t WHERE id = 1"
        ),
        "7.00"
    );
    // decimal → int rounds half away from zero (2.5 → 3)
    assert_eq!(
        one(&mut db, "SELECT CAST(d AS int32) FROM t WHERE id = 1"),
        "3"
    );
    // a plain decimal literal projects unconstrained
    assert_eq!(
        one(&mut db, "SELECT CAST(-2.5 AS int32) FROM t WHERE id = 1"),
        "-3"
    );
}

#[test]
fn typmod_validation_traps_22023() {
    let mut db = Database::new();
    assert_eq!(err_code(&mut db, "CREATE TABLE a (x numeric(0))"), "22023");
    assert_eq!(
        err_code(&mut db, "CREATE TABLE b (x numeric(1001))"),
        "22023"
    );
    assert_eq!(
        err_code(&mut db, "CREATE TABLE c (x numeric(5,7))"),
        "22023"
    );
    // a typmod on a non-decimal type is unsupported (0A000), not a decimal error.
    assert_eq!(err_code(&mut db, "CREATE TABLE d (x int32(5))"), "0A000");
}

#[test]
fn decimal_primary_key_is_rejected_0a000() {
    let mut db = Database::new();
    assert_eq!(
        err_code(&mut db, "CREATE TABLE t (k numeric PRIMARY KEY)"),
        "0A000"
    );
}

#[test]
fn cross_family_and_assignment_type_errors() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, n numeric, i int32, s text)",
        "INSERT INTO t VALUES (1, 1.5, 2, 'x')",
    ]);
    // decimal vs text is not comparable
    assert_eq!(err_code(&mut db, "SELECT id FROM t WHERE n = 'x'"), "42804");
    // a decimal literal into an integer column is a type error (decimal→int is explicit only)
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (2, 1.0, 1.5, 'y')"),
        "42804"
    );
    // a decimal value into a text column is a type error
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (3, 1.0, 1, 9.9)"),
        "42804"
    );
}

#[test]
fn unconstrained_cap_overflow_traps_22003() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, v numeric)"]);
    // a literal with scale > 1000 overflows the cap.
    let lit = format!("0.{}", "0".repeat(1001));
    assert_eq!(
        err_code(&mut db, &format!("INSERT INTO t VALUES (1, {lit})")),
        "22003"
    );
}

#[test]
fn update_coerces_to_typmod() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, money numeric(10,2))",
        "INSERT INTO t VALUES (1, 0)",
        "UPDATE t SET money = 3.14159 WHERE id = 1",
    ]);
    assert_eq!(one(&mut db, "SELECT money FROM t WHERE id = 1"), "3.14");
}

#[test]
fn on_disk_round_trip_preserves_decimals_and_typmod() {
    let db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, money numeric(10,2), free numeric)",
        "INSERT INTO t VALUES (1, 1.5, -12345.6789), (2, 0, 0.00), (3, 100, NULL)",
    ]);
    let image = db.to_image(8192, 1).unwrap();
    let mut loaded = Database::from_image(&image).unwrap();
    // values survive byte-for-byte (re-serialization is identical)
    assert_eq!(loaded.to_image(8192, 1).unwrap(), image);
    // and the reloaded numeric(10,2) typmod still coerces a new insert
    assert_eq!(one(&mut loaded, "SELECT money FROM t WHERE id = 1"), "1.50");
    assert_eq!(
        one(&mut loaded, "SELECT free FROM t WHERE id = 1"),
        "-12345.6789"
    );
    execute(&mut loaded, "INSERT INTO t VALUES (4, 9.999, 9.999)").unwrap();
    assert_eq!(
        one(&mut loaded, "SELECT money FROM t WHERE id = 4"),
        "10.00"
    ); // typmod persisted
    assert_eq!(one(&mut loaded, "SELECT free FROM t WHERE id = 4"), "9.999"); // unconstrained
}

#[test]
fn distinct_collapses_equal_values_across_scale() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, v numeric)",
        "INSERT INTO t VALUES (1, 1.5), (2, 1.50), (3, 1.500), (4, 2.0)",
    ]);
    // 1.5, 1.50, 1.500 are one value; first occurrence (1.5) is kept.
    assert_eq!(
        rendered(&mut db, "SELECT DISTINCT v FROM t ORDER BY v"),
        [["1.5"], ["2.0"]]
    );
}
