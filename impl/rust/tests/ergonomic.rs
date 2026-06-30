//! The rusqlite-style ergonomic layer (spec/design/api.md §11): native-typed bind parameters
//! ([`ToValue`]/[`Params`]) and typed row scanning ([`FromValue`]/[`Row`]). Per-core unit tests, NOT
//! the shared corpus: this is a host-API surface (api.md §1), and these pin the conversions +
//! error codes the corpus cannot express (catalog-free, host-side). The underlying SQL behavior is
//! the corpus's job — every method funnels through the same parser + executor the raw path uses.

use jed::{Database, SessionOptions, Value};

fn seeded() -> Database {
    let mut db = Database::new_in_memory();
    db.run(
        "CREATE TABLE t (id i32 PRIMARY KEY, name text, score f64)",
        (),
    )
    .unwrap();
    db.run(
        "INSERT INTO t (id, name, score) VALUES ($1, $2, $3)",
        (1_i32, "ada", 9.5_f64),
    )
    .unwrap();
    db.run(
        "INSERT INTO t (id, name, score) VALUES ($1, $2, $3)",
        (2_i32, "bob", 7.25_f64),
    )
    .unwrap();
    db
}

/// `run` binds a heterogeneous tuple and returns the affected-row count; DDL carries no count (0).
#[test]
fn run_binds_tuple_and_returns_affected() {
    let mut db = Database::new_in_memory();
    assert_eq!(
        db.run("CREATE TABLE t (id i32 PRIMARY KEY, name text)", ())
            .unwrap(),
        0,
        "DDL has no affected-row count"
    );
    assert_eq!(
        db.run("INSERT INTO t VALUES ($1, $2)", (1_i32, "ada"))
            .unwrap(),
        1
    );
    assert_eq!(
        db.run(
            "INSERT INTO t VALUES ($1, $2), ($3, $4)",
            (2_i32, "bob", 3_i32, "cy")
        )
        .unwrap(),
        2,
        "multi-row insert affects 2"
    );
}

/// `query_rows` yields typed [`Row`]s; `get::<T>` converts a column via [`FromValue`], and the
/// narrowing/float/text targets all read correctly.
#[test]
fn query_rows_typed_get() {
    let mut db = seeded();
    let rows = db
        .query_rows("SELECT id, name, score FROM t ORDER BY id", ())
        .unwrap();
    assert_eq!(rows.len(), 2);

    let id: i32 = rows[0].get(0).unwrap();
    let name: String = rows[0].get(1).unwrap();
    let score: f64 = rows[0].get(2).unwrap();
    assert_eq!((id, name.as_str(), score), (1, "ada", 9.5));

    // The same column read at several integer widths (all widen from the engine's i64).
    let id64: i64 = rows[1].get(0).unwrap();
    let id16: i16 = rows[1].get(0).unwrap();
    assert_eq!((id64, id16), (2, 2));

    // By name, and the raw-Value escape hatch.
    let by_name: String = rows[1].get_by_name("name").unwrap();
    assert_eq!(by_name, "bob");
    assert!(matches!(rows[1].value(0).unwrap(), Value::Int(2)));
    assert_eq!(rows[0].len(), 3);
    assert_eq!(rows[0].column_names(), &["id", "name", "score"]);
}

/// `query_map` maps each row into a native tuple; `query_row` returns the first row as `Option`.
#[test]
fn query_map_and_query_row() {
    let mut db = seeded();
    let pairs: Vec<(i32, String)> = db
        .query_map("SELECT id, name FROM t ORDER BY id", (), |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(pairs, vec![(1, "ada".to_string()), (2, "bob".to_string())]);

    // query_row: present → Some(mapped); absent → None (the idiomatic "maybe a row").
    let count: Option<i64> = db
        .query_row("SELECT count(*) FROM t", (), |r| r.get(0))
        .unwrap();
    assert_eq!(count, Some(2));

    let missing: Option<i32> = db
        .query_row("SELECT id FROM t WHERE id = $1", (999_i32,), |r| r.get(0))
        .unwrap();
    assert_eq!(missing, None);
}

/// `Option<T>` is the nullable scan target: NULL reads as `None`, a present value as `Some`. A bare
/// scalar target rejects NULL with `22004`.
#[test]
fn null_scanning() {
    let mut db = Database::new_in_memory();
    db.run("CREATE TABLE t (id i32 PRIMARY KEY, name text)", ())
        .unwrap();
    db.run(
        "INSERT INTO t (id, name) VALUES ($1, $2)",
        (1_i32, None::<&str>),
    )
    .unwrap();

    let rows = db.query_rows("SELECT id, name FROM t", ()).unwrap();
    let name: Option<String> = rows[0].get(1).unwrap();
    assert_eq!(name, None, "NULL reads into Option as None");

    // A bare String target rejects the NULL with 22004.
    let err = rows[0].get::<String>(1).unwrap_err();
    assert_eq!(err.code(), "22004");
}

/// The conversion error codes: family mismatch `42804`, narrowing overflow `22003`, bad column
/// index/name `42703`.
#[test]
fn scan_error_codes() {
    let mut db = Database::new_in_memory();
    db.run("CREATE TABLE t (big i64 PRIMARY KEY, label text)", ())
        .unwrap();
    db.run("INSERT INTO t VALUES ($1, $2)", (5_000_000_000_i64, "x"))
        .unwrap();
    let rows = db.query_rows("SELECT big, label FROM t", ()).unwrap();

    // text column read as an integer → 42804 datatype mismatch.
    assert_eq!(rows[0].get::<i64>(1).unwrap_err().code(), "42804");
    // a value past i32 read as i32 → 22003 numeric value out of range.
    assert_eq!(rows[0].get::<i32>(0).unwrap_err().code(), "22003");
    // out-of-range index and unknown name → 42703 undefined column.
    assert_eq!(rows[0].value(9).unwrap_err().code(), "42703");
    assert_eq!(
        rows[0].get_by_name::<i64>("nope").unwrap_err().code(),
        "42703"
    );
}

/// The `Params` impls beyond tuples: `()` (empty), a homogeneous `[T; N]` array, a `Vec<T>`, and a
/// raw `[Value]` (via `Value: ToValue`) — all reach the same `$N` binder.
#[test]
fn params_shapes() {
    let mut db = seeded();

    // array params
    let two: Vec<i32> = db
        .query_map(
            "SELECT id FROM t WHERE id = $1 OR id = $2 ORDER BY id",
            [1_i32, 2_i32],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(two, vec![1, 2]);

    // Vec params
    let one: Option<String> = db
        .query_row("SELECT name FROM t WHERE id = $1", vec![2_i32], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(one.as_deref(), Some("bob"));

    // a raw [Value] array (the low-level path still flows through Params)
    let raw: Option<i32> = db
        .query_row("SELECT id FROM t WHERE id = $1", [Value::Int(1)], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(raw, Some(1));
}

/// The ergonomic methods are on every handle: a `Session`, and a `Transaction` (where `run` rolls
/// back with the block on a thrown error).
#[test]
fn on_session_and_transaction() {
    let db = Database::new_in_memory();
    let mut s = db.session(SessionOptions::default());
    s.run("CREATE TABLE t (id i32 PRIMARY KEY, name text)", ())
        .unwrap();

    // Inside an explicit transaction.
    s.update(|tx| {
        tx.run("INSERT INTO t VALUES ($1, $2)", (1_i32, "ada"))?;
        let n: Option<i64> = tx.query_row("SELECT count(*) FROM t", (), |r| r.get(0))?;
        assert_eq!(n, Some(1));
        Ok(())
    })
    .unwrap();

    let total: Option<i64> = s
        .query_row("SELECT count(*) FROM t", (), |r| r.get(0))
        .unwrap();
    assert_eq!(total, Some(1), "committed through the session");
    s.close();
}
