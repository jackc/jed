//! Phase D/E: SELECT — projection, WHERE (=, ordering ops, IS [NOT] NULL),
//! three-valued logic, ORDER BY (NULLs first), and CAST. These complement the
//! conformance corpus with finer-grained per-feature assertions.

use abide::value::Value;
use abide::{Database, Outcome, execute};

fn db_with(stmts: &[&str]) -> Database {
    let mut db = Database::new();
    for s in stmts {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

/// Run a query and return its rows as nested Value vectors.
fn query(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match execute(db, sql).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows,
        Outcome::Statement { .. } => panic!("expected a query result for {sql:?}"),
    }
}

fn setup() -> Database {
    db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, v int16)",
        "INSERT INTO t VALUES (1, 10)",
        "INSERT INTO t VALUES (2, 20)",
        "INSERT INTO t VALUES (3, NULL)",
    ])
}

#[test]
fn point_lookup_by_primary_key() {
    let mut db = setup();
    assert_eq!(
        query(&mut db, "SELECT v FROM t WHERE id = 2"),
        vec![vec![Value::Int(20)]]
    );
}

#[test]
fn select_star_projects_all_columns_in_order() {
    let mut db = setup();
    let rows = query(&mut db, "SELECT * FROM t WHERE id = 1");
    assert_eq!(rows, vec![vec![Value::Int(1), Value::Int(10)]]);
}

#[test]
fn full_scan_is_in_primary_key_order() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY)",
        "INSERT INTO t VALUES (3)",
        "INSERT INTO t VALUES (1)",
        "INSERT INTO t VALUES (2)",
    ]);
    let rows = query(&mut db, "SELECT id FROM t ORDER BY id");
    let ids: Vec<Value> = rows.into_iter().map(|r| r[0]).collect();
    assert_eq!(ids, vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
}

#[test]
fn is_null_and_is_not_null() {
    let mut db = setup();
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE v IS NULL"),
        vec![vec![Value::Int(3)]]
    );
    let not_null = query(&mut db, "SELECT id FROM t WHERE v IS NOT NULL ORDER BY id");
    let ids: Vec<Value> = not_null.into_iter().map(|r| r[0]).collect();
    assert_eq!(ids, vec![Value::Int(1), Value::Int(2)]);
}

#[test]
fn equality_with_null_is_unknown_so_no_rows() {
    // `v = NULL` is UNKNOWN for every row, including the NULL row (CLAUDE.md §4).
    let mut db = setup();
    assert!(query(&mut db, "SELECT id FROM t WHERE v = NULL").is_empty());
}

#[test]
fn comparison_against_null_column_excludes_null_rows() {
    // The NULL row never satisfies an ordering comparison.
    let mut db = setup();
    let rows = query(&mut db, "SELECT id FROM t WHERE v > 5 ORDER BY id");
    let ids: Vec<Value> = rows.into_iter().map(|r| r[0]).collect();
    assert_eq!(ids, vec![Value::Int(1), Value::Int(2)]);
}

#[test]
fn order_by_sorts_nulls_first_then_descending_last() {
    let mut db = setup();
    // Ascending: NULL row (id 3) sorts first by v.
    let asc = query(&mut db, "SELECT id FROM t ORDER BY v");
    let ids: Vec<Value> = asc.into_iter().map(|r| r[0]).collect();
    assert_eq!(ids, vec![Value::Int(3), Value::Int(1), Value::Int(2)]);
    // Descending: NULL row sorts last.
    let desc = query(&mut db, "SELECT id FROM t ORDER BY v DESC");
    let ids: Vec<Value> = desc.into_iter().map(|r| r[0]).collect();
    assert_eq!(ids, vec![Value::Int(2), Value::Int(1), Value::Int(3)]);
}

fn ids(rows: Vec<Vec<Value>>) -> Vec<Value> {
    rows.into_iter().map(|r| r[0]).collect()
}

#[test]
fn limit_caps_and_offset_skips() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, v int32)",
        "INSERT INTO t VALUES (1, 10)",
        "INSERT INTO t VALUES (2, 20)",
        "INSERT INTO t VALUES (3, 30)",
        "INSERT INTO t VALUES (4, 40)",
        "INSERT INTO t VALUES (5, 50)",
    ]);
    // LIMIT takes the first n; OFFSET skips; the two clauses commute.
    assert_eq!(
        ids(query(&mut db, "SELECT id FROM t ORDER BY id LIMIT 2")),
        vec![Value::Int(1), Value::Int(2)]
    );
    assert_eq!(
        ids(query(
            &mut db,
            "SELECT id FROM t ORDER BY id LIMIT 2 OFFSET 1"
        )),
        vec![Value::Int(2), Value::Int(3)]
    );
    assert_eq!(
        ids(query(
            &mut db,
            "SELECT id FROM t ORDER BY id OFFSET 1 LIMIT 2"
        )),
        vec![Value::Int(2), Value::Int(3)]
    );
    assert_eq!(
        ids(query(&mut db, "SELECT id FROM t ORDER BY id OFFSET 3")),
        vec![Value::Int(4), Value::Int(5)]
    );
    // LIMIT 0 and an OFFSET past the end are empty (not errors); a huge LIMIT clamps.
    assert!(query(&mut db, "SELECT id FROM t ORDER BY id LIMIT 0").is_empty());
    assert!(query(&mut db, "SELECT id FROM t ORDER BY id OFFSET 10").is_empty());
    assert_eq!(
        query(&mut db, "SELECT id FROM t ORDER BY id LIMIT 100").len(),
        5
    );
}

#[test]
fn limit_offset_window_reduces_produced_cost() {
    // The slice runs before projection, so only windowed rows charge row_produced:
    // 5 scanned + 2 produced = 7 (spec/design/cost.md §3).
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1)",
        "INSERT INTO t VALUES (2)",
        "INSERT INTO t VALUES (3)",
        "INSERT INTO t VALUES (4)",
        "INSERT INTO t VALUES (5)",
    ]);
    let cost = execute(&mut db, "SELECT id FROM t ORDER BY id LIMIT 2")
        .unwrap()
        .cost();
    assert_eq!(cost, 7);
}

#[test]
fn negative_limit_and_offset_trap_distinctly() {
    let mut db = setup();
    assert_eq!(
        execute(&mut db, "SELECT id FROM t LIMIT -1")
            .unwrap_err()
            .code(),
        "2201W"
    );
    assert_eq!(
        execute(&mut db, "SELECT id FROM t OFFSET -1")
            .unwrap_err()
            .code(),
        "2201X"
    );
}

#[test]
fn duplicate_limit_or_offset_is_a_syntax_error() {
    let mut db = setup();
    assert_eq!(
        execute(&mut db, "SELECT id FROM t LIMIT 1 LIMIT 2")
            .unwrap_err()
            .code(),
        "42601"
    );
    assert_eq!(
        execute(&mut db, "SELECT id FROM t OFFSET 1 OFFSET 2")
            .unwrap_err()
            .code(),
        "42601"
    );
}

#[test]
fn cross_type_comparison_promotes() {
    let mut db = db_with(&[
        "CREATE TABLE p (id int32 PRIMARY KEY, a int16, c int64)",
        "INSERT INTO p VALUES (1, 100, 100)",
        "INSERT INTO p VALUES (2, 100, 300)",
    ]);
    // int16 column compared to an int64 column promotes losslessly.
    let rows = query(&mut db, "SELECT id FROM p WHERE a = c ORDER BY id");
    assert_eq!(rows, vec![vec![Value::Int(1)]]);
}

#[test]
fn cast_narrowing_fits_and_traps() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, b int64)",
        "INSERT INTO t VALUES (1, 1000)",
        "INSERT INTO t VALUES (2, 5000000000)",
    ]);
    assert_eq!(
        query(&mut db, "SELECT CAST(b AS int16) FROM t WHERE id = 1"),
        vec![vec![Value::Int(1000)]]
    );
    let err = execute(&mut db, "SELECT CAST(b AS int16) FROM t WHERE id = 2").unwrap_err();
    assert_eq!(err.code(), "22003");
}

#[test]
fn unknown_column_traps() {
    let mut db = setup();
    assert_eq!(
        execute(&mut db, "SELECT nope FROM t").unwrap_err().code(),
        "42703"
    );
    assert_eq!(
        execute(&mut db, "SELECT id FROM t WHERE nope = 1")
            .unwrap_err()
            .code(),
        "42703"
    );
}

#[test]
fn select_from_missing_table_traps() {
    let mut db = Database::new();
    assert_eq!(
        execute(&mut db, "SELECT x FROM nope").unwrap_err().code(),
        "42P01"
    );
}

#[test]
fn out_of_range_literal_in_comparison_traps() {
    // Context-adaptive literal typing (spec/design/types.md §6): a literal that cannot be
    // represented in the compared column's type is a type error (22003), not a silent
    // non-match — for every operator. An in-range literal compares normally.
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, small int16)",
        "INSERT INTO t VALUES (1, 30000)",
    ]);
    assert_eq!(
        query(&mut db, "SELECT id FROM t WHERE small = 30000"),
        vec![vec![Value::Int(1)]]
    );
    for sql in [
        "SELECT id FROM t WHERE small = 100000",
        "SELECT id FROM t WHERE small < 100000",
        "SELECT id FROM t WHERE small > 100000",
    ] {
        assert_eq!(execute(&mut db, sql).unwrap_err().code(), "22003", "{sql}");
    }
    // The context is the compared column: 5e9 fits int64 but not int32 (the id column).
    assert_eq!(
        execute(&mut db, "SELECT id FROM t WHERE id = 5000000000")
            .unwrap_err()
            .code(),
        "22003"
    );
}
