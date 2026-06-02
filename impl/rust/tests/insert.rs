//! Phase C: INSERT ... VALUES — positional type-checking, overflow trap (22003),
//! NOT NULL (23502) and unique-PK (23505) enforcement, storage in PK order.

use abide::value::Value;
use abide::{Database, Outcome, execute};

fn db_with(sql: &[&str]) -> Database {
    let mut db = Database::new();
    for s in sql {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

#[test]
fn inserts_rows_in_primary_key_order() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, v int16)"]);
    // Insert out of key order; storage must yield them in PK order.
    execute(&mut db, "INSERT INTO t VALUES (3, 30)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1, 10)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (2, 20)").unwrap();

    let rows = db.rows_in_key_order("t").unwrap();
    let ids: Vec<Value> = rows.iter().map(|r| r[0]).collect();
    assert_eq!(ids, vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
}

#[test]
fn negative_keys_sort_before_positive() {
    // Exercises the sign-flip in the order-preserving key encoding.
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY)"]);
    for v in [
        "INSERT INTO t VALUES (1)",
        "INSERT INTO t VALUES (-1)",
        "INSERT INTO t VALUES (0)",
    ] {
        execute(&mut db, v).unwrap();
    }
    let rows = db.rows_in_key_order("t").unwrap();
    let ids: Vec<Value> = rows.iter().map(|r| r[0]).collect();
    assert_eq!(ids, vec![Value::Int(-1), Value::Int(0), Value::Int(1)]);
}

#[test]
fn boundary_values_round_trip() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, s int16, b int64)"]);
    execute(
        &mut db,
        "INSERT INTO t VALUES (1, 32767, 9223372036854775807)",
    )
    .unwrap();
    execute(
        &mut db,
        "INSERT INTO t VALUES (2, -32768, -9223372036854775808)",
    )
    .unwrap();
    let rows = db.rows_in_key_order("t").unwrap();
    assert_eq!(
        rows[0],
        vec![
            Value::Int(1),
            Value::Int(32767),
            Value::Int(9223372036854775807)
        ]
    );
    assert_eq!(
        rows[1],
        vec![
            Value::Int(2),
            Value::Int(-32768),
            Value::Int(-9223372036854775808)
        ]
    );
}

#[test]
fn overflow_traps_and_row_is_not_stored() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, s int16)"]);
    execute(&mut db, "INSERT INTO t VALUES (1, 32767)").unwrap();

    for (sql, _why) in [
        ("INSERT INTO t VALUES (2, 32768)", "int16 max + 1"),
        ("INSERT INTO t VALUES (3, -32769)", "int16 min - 1"),
    ] {
        let err = execute(&mut db, sql).unwrap_err();
        assert_eq!(err.code(), "22003", "{sql}");
    }
    // Only the in-range row landed.
    assert_eq!(db.rows_in_key_order("t").unwrap().len(), 1);
}

#[test]
fn int32_and_int64_overflow_boundaries() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, n int32)"]);
    assert_eq!(
        execute(&mut db, "INSERT INTO t VALUES (1, 2147483648)")
            .unwrap_err()
            .code(),
        "22003"
    );
    // int32 max fits.
    assert_eq!(
        execute(&mut db, "INSERT INTO t VALUES (2, 2147483647)").unwrap(),
        Outcome::Statement { cost: 0 }
    );
}

#[test]
fn null_into_nullable_column_is_stored() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, v int16)"]);
    execute(&mut db, "INSERT INTO t VALUES (1, NULL)").unwrap();
    let rows = db.rows_in_key_order("t").unwrap();
    assert_eq!(rows[0], vec![Value::Int(1), Value::Null]);
}

#[test]
fn null_into_primary_key_traps() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, v int16)"]);
    let err = execute(&mut db, "INSERT INTO t VALUES (NULL, 1)").unwrap_err();
    assert_eq!(err.code(), "23502");
}

#[test]
fn duplicate_primary_key_traps() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY)"]);
    execute(&mut db, "INSERT INTO t VALUES (1)").unwrap();
    let err = execute(&mut db, "INSERT INTO t VALUES (1)").unwrap_err();
    assert_eq!(err.code(), "23505");
}

#[test]
fn wrong_value_count_is_rejected() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, v int16)"]);
    assert_eq!(
        execute(&mut db, "INSERT INTO t VALUES (1)")
            .unwrap_err()
            .code(),
        "42601"
    );
    assert_eq!(
        execute(&mut db, "INSERT INTO t VALUES (1, 2, 3)")
            .unwrap_err()
            .code(),
        "42601"
    );
}

#[test]
fn insert_into_missing_table_traps() {
    let mut db = Database::new();
    let err = execute(&mut db, "INSERT INTO nope VALUES (1)").unwrap_err();
    assert_eq!(err.code(), "42P01");
}
