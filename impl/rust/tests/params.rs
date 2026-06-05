//! Phase 7: parameterized queries (`$N` bind parameters) — spec/design/api.md §5.
//! Parameters are a host-API surface (not the shared corpus): their type is inferred from
//! context and supplied values are coerced two-phase before any row is touched.

use jed::value::Value;
use jed::{Database, Outcome, execute, execute_params};

fn db_with(sql: &[&str]) -> Database {
    let mut db = Database::new();
    for s in sql {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn rows(db: &mut Database, sql: &str, params: &[Value]) -> Vec<Vec<Value>> {
    match execute_params(db, sql, params).unwrap_or_else(|e| panic!("{sql:?}: {}", e.message)) {
        Outcome::Query { rows, .. } => rows,
        _ => panic!("expected a query result"),
    }
}

fn err_code(db: &mut Database, sql: &str, params: &[Value]) -> String {
    execute_params(db, sql, params)
        .err()
        .unwrap_or_else(|| panic!("{sql:?}: expected an error"))
        .code()
        .to_string()
}

#[test]
fn where_pk_eq_param_point_lookup() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, v int32)",
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
    ]);
    let got = rows(&mut db, "SELECT v FROM t WHERE id = $1", &[Value::Int(2)]);
    assert_eq!(got, vec![vec![Value::Int(20)]]);
}

#[test]
fn param_adopts_narrow_column_type_and_traps_overflow() {
    // `$1` compared against an int16 column is typed int16; a value out of int16 range traps
    // 22003 at bind, before any scan.
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, s int16)"]);
    execute(&mut db, "INSERT INTO t VALUES (1, 100)").unwrap();
    assert_eq!(
        err_code(
            &mut db,
            "SELECT id FROM t WHERE s = $1",
            &[Value::Int(100000)]
        ),
        "22003"
    );
    // In range: it just matches (or not) normally.
    let got = rows(&mut db, "SELECT id FROM t WHERE s = $1", &[Value::Int(100)]);
    assert_eq!(got, vec![vec![Value::Int(1)]]);
}

#[test]
fn insert_values_params_round_trip() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, name text)"]);
    execute_params(
        &mut db,
        "INSERT INTO t VALUES ($1, $2)",
        &[Value::Int(7), Value::Text("alice".into())],
    )
    .unwrap();
    let got = rows(
        &mut db,
        "SELECT id, name FROM t WHERE id = $1",
        &[Value::Int(7)],
    );
    assert_eq!(got, vec![vec![Value::Int(7), Value::Text("alice".into())]]);
}

#[test]
fn insert_param_null_into_not_null_traps_23502() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, name text NOT NULL)"]);
    assert_eq!(
        err_code(
            &mut db,
            "INSERT INTO t VALUES ($1, $2)",
            &[Value::Int(1), Value::Null],
        ),
        "23502"
    );
}

#[test]
fn insert_param_wrong_family_traps_42804() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, n int32)"]);
    // `$2` is typed int32 (its column); binding text is a family mismatch.
    assert_eq!(
        err_code(
            &mut db,
            "INSERT INTO t VALUES ($1, $2)",
            &[Value::Int(1), Value::Text("x".into())],
        ),
        "42804"
    );
}

#[test]
fn update_set_and_where_params() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, v int32)",
        "INSERT INTO t VALUES (1, 10), (2, 20)",
    ]);
    execute_params(
        &mut db,
        "UPDATE t SET v = $1 WHERE id = $2",
        &[Value::Int(99), Value::Int(2)],
    )
    .unwrap();
    let got = rows(&mut db, "SELECT v FROM t WHERE id = $1", &[Value::Int(2)]);
    assert_eq!(got, vec![vec![Value::Int(99)]]);
}

#[test]
fn delete_where_param() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1), (2), (3)",
    ]);
    execute_params(&mut db, "DELETE FROM t WHERE id = $1", &[Value::Int(2)]).unwrap();
    let got = rows(&mut db, "SELECT id FROM t", &[]);
    assert_eq!(got, vec![vec![Value::Int(1)], vec![Value::Int(3)]]);
}

#[test]
fn text_param_inference() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, name text)",
        "INSERT INTO t VALUES (1, 'alice'), (2, 'bob')",
    ]);
    let got = rows(
        &mut db,
        "SELECT id FROM t WHERE name = $1",
        &[Value::Text("bob".into())],
    );
    assert_eq!(got, vec![vec![Value::Int(2)]]);
}

#[test]
fn bare_select_param_is_indeterminate_42p18() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY)"]);
    assert_eq!(
        err_code(&mut db, "SELECT $1 FROM t", &[Value::Int(1)]),
        "42P18"
    );
}

#[test]
fn gap_in_param_indices_is_42p18() {
    // `$1` and `$3` referenced, `$2` never — the missing slot is indeterminate.
    let mut db = db_with(&["CREATE TABLE t (a int32 PRIMARY KEY, b int32)"]);
    assert_eq!(
        err_code(
            &mut db,
            "SELECT a FROM t WHERE a = $1 OR b = $3",
            &[Value::Int(1), Value::Int(2), Value::Int(3)],
        ),
        "42P18"
    );
}

#[test]
fn conflicting_inference_is_42804() {
    let mut db = db_with(&["CREATE TABLE t (a int32 PRIMARY KEY, name text)"]);
    assert_eq!(
        err_code(
            &mut db,
            "SELECT a FROM t WHERE a = $1 OR name = $1",
            &[Value::Int(1)],
        ),
        "42804"
    );
}

#[test]
fn count_mismatch_is_42601() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1)",
    ]);
    assert_eq!(
        err_code(&mut db, "SELECT id FROM t WHERE id = $1", &[]),
        "42601"
    );
    assert_eq!(
        err_code(
            &mut db,
            "SELECT id FROM t WHERE id = $1",
            &[Value::Int(1), Value::Int(2)],
        ),
        "42601"
    );
}

#[test]
fn null_param_three_valued() {
    // `col = $1` with a NULL bound yields UNKNOWN, so no rows; IS NOT DISTINCT FROM matches NULL.
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY, v int32)",
        "INSERT INTO t VALUES (1, 10)",
    ]);
    let got = rows(&mut db, "SELECT id FROM t WHERE v = $1", &[Value::Null]);
    assert!(got.is_empty());
}

#[test]
fn param_in_in_list() {
    let mut db = db_with(&[
        "CREATE TABLE t (id int32 PRIMARY KEY)",
        "INSERT INTO t VALUES (1), (2), (3)",
    ]);
    let got = rows(
        &mut db,
        "SELECT id FROM t WHERE id IN ($1, $2)",
        &[Value::Int(1), Value::Int(3)],
    );
    assert_eq!(got, vec![vec![Value::Int(1)], vec![Value::Int(3)]]);
}

#[test]
fn ddl_with_params_traps_42601() {
    let mut db = Database::new();
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE t (id int32 PRIMARY KEY)",
            &[Value::Int(1)]
        ),
        "42601"
    );
}

#[test]
fn lexer_rejects_bad_param_tokens() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY)"]);
    for sql in [
        "SELECT id FROM t WHERE id = $0",
        "SELECT id FROM t WHERE id = $",
        "SELECT id FROM t WHERE id = $01",
    ] {
        assert_eq!(
            execute(&mut db, sql).err().map(|e| e.code().to_string()),
            Some("42601".to_string()),
            "{sql:?} should be 42601"
        );
    }
}
