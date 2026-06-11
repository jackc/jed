//! Composite PRIMARY KEY — the table-level `PRIMARY KEY (a, b, …)` constraint
//! (spec/design/constraints.md §3, grammar.md §28). Covers what the corpus suite
//! (`ddl/composite_pk.test`) cannot: catalog flag introspection, the stored key order
//! (the concatenated encoding of encoding.md §2.3), and the on-disk round-trip (a
//! composite-PK table reloads as a KEYED table, not a rowid table). Mirrored in
//! impl/go/composite_pk_test.go and impl/ts/tests/composite_pk.test.ts.

use jed::value::Value;
use jed::{Database, execute};

fn db_with(sql: &[&str]) -> Database {
    let mut db = Database::new();
    for s in sql {
        execute(&mut db, s).unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn err_code(db: &mut Database, sql: &str) -> String {
    execute(db, sql)
        .expect_err(&format!("expected an error from {sql:?}"))
        .code()
        .to_string()
}

/// The constraint flags every member primary_key + NOT NULL, and the stored order is the
/// tuple's lexicographic order (the concatenated key — first component, then the second
/// breaking its ties), independent of insertion order.
#[test]
fn composite_key_orders_by_tuple() {
    let mut db = db_with(&["CREATE TABLE t (a int32, b int32, v int16, PRIMARY KEY (a, b))"]);
    let t = db.table("t").unwrap();
    assert_eq!(t.pk_indices(), vec![0, 1]);
    assert!(t.columns[0].primary_key && t.columns[0].not_null);
    assert!(t.columns[1].primary_key && t.columns[1].not_null);
    assert!(!t.columns[2].primary_key);
    // Single-column pushdown accessor must NOT see a composite key.
    assert_eq!(t.primary_key_index(), None);

    // Insert out of tuple order; include a negative first component (sign-flip) and ties
    // on the first component broken by the second.
    for stmt in [
        "INSERT INTO t VALUES (2, 1, 50)",
        "INSERT INTO t VALUES (1, 2, 30)",
        "INSERT INTO t VALUES (-1, 9, 10)",
        "INSERT INTO t VALUES (1, 1, 20)",
        "INSERT INTO t VALUES (2, 0, 40)",
    ] {
        execute(&mut db, stmt).unwrap();
    }
    let rows = db.rows_in_key_order("t").unwrap();
    let tuples: Vec<(i64, i64)> = rows
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::Int(a), Value::Int(b)) => (*a, *b),
            other => panic!("expected int pair, got {other:?}"),
        })
        .collect();
    assert_eq!(tuples, vec![(-1, 9), (1, 1), (1, 2), (2, 0), (2, 1)]);
}

/// Uniqueness is over the WHOLE tuple: a shared prefix is fine, a duplicate tuple traps
/// 23505 — both against the store and within one INSERT's batch (two-phase, nothing stored).
#[test]
fn uniqueness_is_the_whole_tuple() {
    let mut db = db_with(&[
        "CREATE TABLE t (a int32, b int32, PRIMARY KEY (a, b))",
        "INSERT INTO t VALUES (1, 1)",
    ]);
    execute(&mut db, "INSERT INTO t VALUES (1, 2)").unwrap(); // shared prefix: distinct row
    assert_eq!(err_code(&mut db, "INSERT INTO t VALUES (1, 1)"), "23505");
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (5, 5), (5, 5)"),
        "23505"
    );
    // The failed batch stored nothing (all-or-nothing).
    assert_eq!(db.rows_in_key_order("t").unwrap().len(), 2);
}

/// DDL errors mirror PostgreSQL (oracle-probed): unknown member 42703, repeated member
/// 42701, more than one primary key across both forms 42P16 — plus the jed narrowings
/// (0A000): out-of-declaration-order list, non-keyable member type.
#[test]
fn ddl_errors_match_postgres_and_narrowings() {
    let mut db = Database::new();
    assert_eq!(
        err_code(&mut db, "CREATE TABLE t (a int32, PRIMARY KEY (a, nosuch))"),
        "42703"
    );
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE t (a int32, b int32, PRIMARY KEY (a, a))"
        ),
        "42701"
    );
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE t (a int32 PRIMARY KEY, b int32, PRIMARY KEY (b))"
        ),
        "42P16"
    );
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE t (a int32, b int32, PRIMARY KEY (a), PRIMARY KEY (b))"
        ),
        "42P16"
    );
    // 42P16 fires BEFORE the second constraint's members resolve (PostgreSQL's order).
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE t (a int32 PRIMARY KEY, PRIMARY KEY (nosuch))"
        ),
        "42P16"
    );
    // Narrowing: the list must name columns in declaration order.
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE t (a int32, b int32, PRIMARY KEY (b, a))"
        ),
        "0A000"
    );
    // Narrowing: every member must be key-encodable (text is not, types.md §11).
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE t (a int32, s text, PRIMARY KEY (a, s))"
        ),
        "0A000"
    );
    // A single-column table constraint is the column-level form's equivalent.
    execute(&mut db, "CREATE TABLE ok (a int32, PRIMARY KEY (a))").unwrap();
    let t = db.table("ok").unwrap();
    assert_eq!(t.primary_key_index(), Some(0));
    assert!(t.columns[0].not_null);
}

/// Every member is a key column: NULL into any member traps 23502, and UPDATE may assign
/// no member (0A000 — the storage key never changes), while non-member columns update fine.
#[test]
fn members_are_not_null_and_update_guarded() {
    let mut db = db_with(&[
        "CREATE TABLE t (a int32, b int32, v int16, PRIMARY KEY (a, b))",
        "INSERT INTO t VALUES (1, 1, 10)",
    ]);
    assert_eq!(
        err_code(&mut db, "INSERT INTO t VALUES (1, NULL, 5)"),
        "23502"
    );
    assert_eq!(
        err_code(&mut db, "INSERT INTO t (a, v) VALUES (2, 5)"),
        "23502"
    );
    assert_eq!(err_code(&mut db, "UPDATE t SET a = 9"), "0A000");
    assert_eq!(err_code(&mut db, "UPDATE t SET b = 9"), "0A000");
    execute(&mut db, "UPDATE t SET v = 11").unwrap();
}

/// Mixed fixed-width components (uuid first, int32 second) concatenate per encoding.md
/// §2.3 and iterate in tuple order — uuid bytes compare first, the int breaks ties.
#[test]
fn mixed_uuid_int_components_order_correctly() {
    let mut db = db_with(&["CREATE TABLE t (u uuid, n int32, PRIMARY KEY (u, n))"]);
    for stmt in [
        "INSERT INTO t VALUES ('ffffffff-ffff-ffff-ffff-ffffffffffff', -5)",
        "INSERT INTO t VALUES ('00000000-0000-0000-0000-000000000001', 7)",
        "INSERT INTO t VALUES ('00000000-0000-0000-0000-000000000001', -2)",
    ] {
        execute(&mut db, stmt).unwrap();
    }
    let rows = db.rows_in_key_order("t").unwrap();
    let ns: Vec<i64> = rows
        .iter()
        .map(|r| match &r[1] {
            Value::Int(n) => *n,
            v => panic!("expected int, got {v:?}"),
        })
        .collect();
    assert_eq!(ns, vec![-2, 7, -5]);
}

/// The on-disk round-trip: a composite-PK table reloads as a KEYED table (both flag bits
/// survive in the catalog), key order is preserved, and a duplicate tuple still traps
/// 23505 after the reload. Guards the format.rs has_pk seam — a composite-PK table must
/// not be mistaken for a rowid table on load.
#[test]
fn round_trips_through_the_on_disk_image() {
    let db = db_with(&[
        "CREATE TABLE t (a int32, b int32, v int16, PRIMARY KEY (a, b))",
        "INSERT INTO t VALUES (2, 1, 40), (1, 2, 20), (1, 1, 10)",
    ]);
    let image = db.to_image(256, 1).unwrap();
    let mut loaded = Database::from_image(&image).unwrap();

    let t = loaded.table("t").unwrap();
    assert_eq!(t.pk_indices(), vec![0, 1]);
    assert!(t.columns[0].not_null && t.columns[1].not_null);

    let tuples: Vec<(i64, i64)> = loaded
        .rows_in_key_order("t")
        .unwrap()
        .iter()
        .map(|r| match (&r[0], &r[1]) {
            (Value::Int(a), Value::Int(b)) => (*a, *b),
            other => panic!("expected int pair, got {other:?}"),
        })
        .collect();
    assert_eq!(tuples, vec![(1, 1), (1, 2), (2, 1)]);

    assert_eq!(
        err_code(&mut loaded, "INSERT INTO t VALUES (1, 2, 99)"),
        "23505"
    );
    execute(&mut loaded, "INSERT INTO t VALUES (2, 2, 50)").unwrap();
    assert_eq!(loaded.rows_in_key_order("t").unwrap().len(), 4);
}
