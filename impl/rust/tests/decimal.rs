//! Phase 3: the exact `decimal` / `numeric` type, end to end through `execute`
//! (spec/design/decimal.md). Assertions are on the **rendered** output — the cross-core
//! contract — since decimal value-equality (1.5 == 1.50) is intentionally scale-insensitive.

use jed::{Database, Outcome, Session, SessionOptions};

fn db_with(stmts: &[&str]) -> Session {
    let mut db = Database::new_in_memory().session(SessionOptions::default());
    for s in stmts {
        db.execute(s, &[])
            .unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

/// Run a query and render every cell to its canonical string (row-major).
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

/// A single-cell query result, rendered.
fn one(db: &mut Session, sql: &str) -> String {
    let rows = rendered(db, sql);
    assert_eq!(rows.len(), 1, "{sql:?} should return one row");
    assert_eq!(rows[0].len(), 1, "{sql:?} should return one column");
    rows[0][0].clone()
}

#[test]
fn update_coerces_to_typmod() {
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, money numeric(10,2))",
        "INSERT INTO t VALUES (1, 0)",
        "UPDATE t SET money = 3.14159 WHERE id = 1",
    ]);
    assert_eq!(one(&mut db, "SELECT money FROM t WHERE id = 1"), "3.14");
}

#[test]
fn on_disk_round_trip_preserves_decimals_and_typmod() {
    let db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY, money numeric(10,2), free numeric)",
        "INSERT INTO t VALUES (1, 1.5, -12345.6789), (2, 0, 0.00), (3, 100, NULL)",
    ]);
    let image = db.to_image(8192, 1).unwrap();
    let mut loaded = Database::from_image(&image)
        .unwrap()
        .session(SessionOptions::default());
    // values survive byte-for-byte (re-serialization is identical)
    assert_eq!(loaded.to_image(8192, 1).unwrap(), image);
    // and the reloaded numeric(10,2) typmod still coerces a new insert
    assert_eq!(one(&mut loaded, "SELECT money FROM t WHERE id = 1"), "1.50");
    assert_eq!(
        one(&mut loaded, "SELECT free FROM t WHERE id = 1"),
        "-12345.6789"
    );
    loaded
        .execute("INSERT INTO t VALUES (4, 9.999, 9.999)", &[])
        .unwrap();
    assert_eq!(
        one(&mut loaded, "SELECT money FROM t WHERE id = 4"),
        "10.00"
    ); // typmod persisted
    assert_eq!(one(&mut loaded, "SELECT free FROM t WHERE id = 4"), "9.999"); // unconstrained
}

#[test]
fn mul_result_scale_rounds_at_max_scale() {
    // PG numeric_mul: an exact product whose scale exceeds max_scale (16383) ROUNDS to it,
    // half away from zero, instead of trapping (spec/design/decimal.md §2). Mirrors
    // impl/go/decimal_test.go and impl/ts/tests/decimal.test.ts.
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1)",
    ]);
    let tiny1 = format!("0.{}1", "0".repeat(8191)); // 1e-8192 (scale 8192)
    let tiny5 = format!("0.{}5", "0".repeat(8191)); // 5e-8192
    // 1e-8192 * 1e-8192 = 1e-16384: the dropped digit is 1 -> rounds DOWN to 0 at scale 16383.
    assert_eq!(
        one(&mut db, &format!("SELECT {tiny1} * {tiny1} = 0 FROM t")),
        "true"
    );
    // 5e-8192 * 1e-8192 = 5e-16384: the dropped digit is 5 -> rounds UP to 1e-16383, nonzero.
    assert_eq!(
        one(&mut db, &format!("SELECT {tiny5} * {tiny1} = 0 FROM t")),
        "false"
    );
}

#[test]
fn cost_ceiling_aborts_ahead_of_a_big_multiply() {
    // decimal_work is charged and GUARDED before the limb work runs (spec/design/cost.md §3/§6),
    // so a ceiling aborts a pathological multiply up front (CLAUDE.md §13). ~20000 digits is
    // ~5000 groups; the mul W is ~25,000,000 — far over the tiny ceiling.
    let mut db = db_with(&[
        "CREATE TABLE t (id i32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1)",
    ]);
    let big = format!("{}.5", "9".repeat(20000));
    db.set_max_cost(1000);
    match db.execute(&format!("SELECT {big} * {big} FROM t"), &[]) {
        Err(e) => assert_eq!(e.code(), "54P01", "want the cost-limit abort"),
        Ok(_) => panic!("expected the cost ceiling to abort the multiply"),
    }
}
