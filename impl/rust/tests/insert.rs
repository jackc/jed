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

// --- multi-row INSERT (spec/design/grammar.md §12) --------------------------------

#[test]
fn multi_row_insert_stores_all_rows_in_key_order() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, v int16)"]);
    // One statement, rows out of key order; storage must yield them in PK order.
    execute(&mut db, "INSERT INTO t VALUES (3, 30), (1, 10), (2, 20)").unwrap();
    let rows = db.rows_in_key_order("t").unwrap();
    let ids: Vec<Value> = rows.iter().map(|r| r[0]).collect();
    assert_eq!(ids, vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
}

#[test]
fn multi_row_insert_is_all_or_nothing_on_overflow() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, s int16)"]);
    // The second row overflows int16 — the WHOLE statement fails and stores nothing,
    // even though the first row is valid (two-phase / all-or-nothing).
    let err = execute(&mut db, "INSERT INTO t VALUES (1, 10), (2, 99999)").unwrap_err();
    assert_eq!(err.code(), "22003");
    assert_eq!(db.rows_in_key_order("t").unwrap().len(), 0);
}

#[test]
fn multi_row_insert_duplicate_within_batch_traps_and_stores_nothing() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY)"]);
    let err = execute(&mut db, "INSERT INTO t VALUES (1), (1)").unwrap_err();
    assert_eq!(err.code(), "23505");
    assert_eq!(db.rows_in_key_order("t").unwrap().len(), 0);
}

#[test]
fn multi_row_insert_duplicate_against_stored_traps_and_stores_nothing() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY)"]);
    execute(&mut db, "INSERT INTO t VALUES (1)").unwrap();
    // The second row of the batch collides with the already-stored row 1; the new
    // row 2 must NOT be left behind.
    let err = execute(&mut db, "INSERT INTO t VALUES (2), (1)").unwrap_err();
    assert_eq!(err.code(), "23505");
    let ids: Vec<Value> = db
        .rows_in_key_order("t")
        .unwrap()
        .iter()
        .map(|r| r[0])
        .collect();
    assert_eq!(ids, vec![Value::Int(1)]);
}

#[test]
fn multi_row_insert_wrong_arity_in_one_row_is_rejected() {
    let mut db = db_with(&["CREATE TABLE t (id int32 PRIMARY KEY, v int16)"]);
    let err = execute(&mut db, "INSERT INTO t VALUES (1, 10), (2)").unwrap_err();
    assert_eq!(err.code(), "42601");
    assert_eq!(db.rows_in_key_order("t").unwrap().len(), 0);
}

#[test]
fn no_pk_multi_row_insert_keeps_insertion_order() {
    let mut db = db_with(&["CREATE TABLE log (a int32)"]);
    // No PK ⇒ monotonic synthetic rowids, allocated left-to-right; key order = insertion order.
    execute(&mut db, "INSERT INTO log VALUES (30), (10), (20)").unwrap();
    let vals: Vec<Value> = db
        .rows_in_key_order("log")
        .unwrap()
        .iter()
        .map(|r| r[0])
        .collect();
    assert_eq!(vals, vec![Value::Int(30), Value::Int(10), Value::Int(20)]);
}

#[test]
fn no_pk_multi_row_insert_is_all_or_nothing() {
    let mut db = db_with(&["CREATE TABLE log (a int16)"]);
    execute(&mut db, "INSERT INTO log VALUES (1)").unwrap();
    // The batch fails validation (second row overflows int16), so its first row (2) must
    // not be stored either — even though a no-PK row can never collide on its rowid.
    let err = execute(&mut db, "INSERT INTO log VALUES (2), (99999)").unwrap_err();
    assert_eq!(err.code(), "22003");
    execute(&mut db, "INSERT INTO log VALUES (3), (4)").unwrap();
    let vals: Vec<Value> = db
        .rows_in_key_order("log")
        .unwrap()
        .iter()
        .map(|r| r[0])
        .collect();
    // Only 1, 3, 4 landed; the failed batch's 2 is absent.
    assert_eq!(vals, vec![Value::Int(1), Value::Int(3), Value::Int(4)]);
}
