//! Composite PRIMARY KEY — the table-level `PRIMARY KEY (a, b, …)` constraint
//! (spec/design/constraints.md §3, grammar.md §28). Covers what the corpus suite
//! (`ddl/composite_pk.test`) cannot: catalog flag introspection, the stored key order
//! (the concatenated encoding of encoding.md §2.3), and the on-disk round-trip (a
//! composite-PK table reloads as a KEYED table, not a rowid table). Mirrored in
//! impl/go/composite_pk_test.go and impl/ts/tests/composite_pk.test.ts.

use jed::value::Value;
use jed::{CreateOptions, Database, Session, SessionOptions};

fn db_with(sql: &[&str]) -> Session {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    for s in sql {
        db.query_outcome(s, &[])
            .unwrap_or_else(|e| panic!("setup {s:?}: {}", e.message));
    }
    db
}

fn err_code(db: &mut Session, sql: &str) -> String {
    db.query_outcome(sql, &[])
        .expect_err(&format!("expected an error from {sql:?}"))
        .code()
        .to_string()
}

/// The constraint flags every member primary_key + NOT NULL, and the stored order is the
/// tuple's lexicographic order (the concatenated key — first component, then the second
/// breaking its ties), independent of insertion order.
#[test]
fn composite_key_orders_by_tuple() {
    let mut db = db_with(&["CREATE TABLE t (a i32, b i32, v i16, PRIMARY KEY (a, b))"]);
    let t = db.table("t").unwrap();
    assert_eq!(t.pk_indices(), vec![0, 1]);
    assert!(t.columns[0].primary_key && t.columns[0].not_null);
    assert!(t.columns[1].primary_key && t.columns[1].not_null);
    assert!(!t.columns[2].primary_key);
    // The single-member-only helper must NOT see a composite key.
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
        db.query_outcome(stmt, &[]).unwrap();
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
        "CREATE TABLE t (a i32, b i32, PRIMARY KEY (a, b))",
        "INSERT INTO t VALUES (1, 1)",
    ]);
    db.query_outcome("INSERT INTO t VALUES (1, 2)", &[])
        .unwrap(); // shared prefix: distinct row
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        err_code(&mut db, "CREATE TABLE t (a i32, PRIMARY KEY (a, nosuch))"),
        "42703"
    );
    assert_eq!(
        err_code(&mut db, "CREATE TABLE t (a i32, b i32, PRIMARY KEY (a, a))"),
        "42701"
    );
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE t (a i32 PRIMARY KEY, b i32, PRIMARY KEY (b))"
        ),
        "42P16"
    );
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE t (a i32, b i32, PRIMARY KEY (a), PRIMARY KEY (b))"
        ),
        "42P16"
    );
    // 42P16 fires BEFORE the second constraint's members resolve (PostgreSQL's order).
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE t (a i32 PRIMARY KEY, PRIMARY KEY (nosuch))"
        ),
        "42P16"
    );
    // The list order is the KEY order — it may differ from declaration order (the original
    // 0A000 narrowing was lifted by the v5 catalog reshape, constraints.md §3): the table
    // keys by (b, a), so the stored scan order is b-major.
    db.query_outcome("CREATE TABLE t (a i32, b i32, PRIMARY KEY (b, a))", &[])
        .unwrap();
    assert_eq!(db.table("t").unwrap().pk_indices(), vec![1, 0]);
    db.query_outcome("INSERT INTO t VALUES (1, 20), (2, 10), (3, 15)", &[])
        .unwrap();
    let rows = db.rows_in_key_order("t").unwrap();
    let bs: Vec<i64> = rows
        .iter()
        .map(|r| match r[1] {
            Value::Int(n) => n,
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(
        bs,
        vec![10, 15, 20],
        "stored order is the (b, a) tuple order"
    );
    db.query_outcome("DROP TABLE t", &[]).unwrap();
    // f64 IS now a key-encodable PK member (the float-order-preserving key, encoding.md §2.8 — every
    // scalar is keyable): a composite PK with a float member succeeds.
    db.query_outcome("CREATE TABLE fpk (a i32, s f64, PRIMARY KEY (a, s))", &[])
        .unwrap();
    // The `composite` container is now keyable too (the third container key, `composite-field-slots`
    // encoding.md §2.15 / composite.md §6): a composite-typed PK member of an all-keyable-field type
    // succeeds. Only a composite that transitively contains an array-of-composite field stays 0A000.
    db.query_outcome("CREATE TYPE addr AS (street text, zip i32)", &[])
        .unwrap();
    db.query_outcome("CREATE TABLE cpk (a i32, s addr, PRIMARY KEY (a, s))", &[])
        .unwrap();
    assert_eq!(db.table("cpk").unwrap().pk_indices(), vec![0, 1]);
    db.query_outcome("CREATE TYPE poly AS (name text, pts addr[])", &[])
        .unwrap();
    assert_eq!(
        err_code(
            &mut db,
            "CREATE TABLE t (a i32, p poly, PRIMARY KEY (a, p))"
        ),
        "0A000"
    );
    // A single-column table constraint is the column-level form's equivalent.
    db.query_outcome("CREATE TABLE ok (a i32, PRIMARY KEY (a))", &[])
        .unwrap();
    let t = db.table("ok").unwrap();
    assert_eq!(t.primary_key_index(), Some(0));
    assert!(t.columns[0].not_null);
}

/// Every member is a key column: NULL into any member traps 23502. Assigning a member now
/// re-keys the row (§11 step 6 — the narrowing is lifted) instead of trapping 0A000; a
/// non-member updates in place.
#[test]
fn members_are_not_null_and_rekey() {
    let mut db = db_with(&[
        "CREATE TABLE t (a i32, b i32, v i16, PRIMARY KEY (a, b))",
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
    // Assigning a key member re-keys the row: (1,1) → (9,1) → (9,9); a non-member is in place.
    db.query_outcome("UPDATE t SET a = 9", &[]).unwrap();
    db.query_outcome("UPDATE t SET b = 9", &[]).unwrap();
    db.query_outcome("UPDATE t SET v = 11", &[]).unwrap();
    let rows = db.rows_in_key_order("t").unwrap();
    assert_eq!(rows.len(), 1);
    assert!(matches!(
        (&rows[0][0], &rows[0][1], &rows[0][2]),
        (Value::Int(9), Value::Int(9), Value::Int(11))
    ));
}

/// Mixed fixed-width components (uuid first, i32 second) concatenate per encoding.md
/// §2.3 and iterate in tuple order — uuid bytes compare first, the int breaks ties.
#[test]
fn mixed_uuid_int_components_order_correctly() {
    let mut db = db_with(&["CREATE TABLE t (u uuid, n i32, PRIMARY KEY (u, n))"]);
    for stmt in [
        "INSERT INTO t VALUES ('ffffffff-ffff-ffff-ffff-ffffffffffff', -5)",
        "INSERT INTO t VALUES ('00000000-0000-0000-0000-000000000001', 7)",
        "INSERT INTO t VALUES ('00000000-0000-0000-0000-000000000001', -2)",
    ] {
        db.query_outcome(stmt, &[]).unwrap();
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
        "CREATE TABLE t (a i32, b i32, v i16, PRIMARY KEY (a, b))",
        "INSERT INTO t VALUES (2, 1, 40), (1, 2, 20), (1, 1, 10)",
    ]);
    let image = db.to_image(256, 1).unwrap();
    let mut loaded = Database::from_image(&image)
        .unwrap()
        .session(SessionOptions::default());

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
    loaded
        .query_outcome("INSERT INTO t VALUES (2, 2, 50)", &[])
        .unwrap();
    assert_eq!(loaded.rows_in_key_order("t").unwrap().len(), 4);
}
