//! Phase 7: the formal host API (spec/design/api.md) — open/create/commit/close a database
//! file, prepare/execute/query, the `Rows` cursor, and the structured-error surface. Files are
//! written under Cargo's per-test temp dir (`CARGO_TARGET_TMPDIR`), never the repo tree.

use std::path::PathBuf;

use jed::value::Value;
use jed::{Database, DatabaseOptions, Outcome, execute};

fn tmp(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

#[test]
fn create_commit_reopen_round_trips() {
    let path = tmp("round_trip.jed");
    let _ = std::fs::remove_file(&path);

    let mut db = Database::create(&path, DatabaseOptions::default()).unwrap();
    assert_eq!(db.txid(), 1); // the initial empty image is committed at create
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, v int32)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1, 10), (2, 20)").unwrap();
    db.commit().unwrap();
    let after_commit = db.txid();
    db.close().unwrap();

    let mut db = Database::open(&path).unwrap();
    assert_eq!(db.txid(), after_commit);
    match execute(&mut db, "SELECT id, v FROM t").unwrap() {
        Outcome::Query { rows, .. } => assert_eq!(
            rows,
            vec![
                vec![Value::Int(1), Value::Int(10)],
                vec![Value::Int(2), Value::Int(20)],
            ]
        ),
        _ => panic!("expected a query"),
    }
}

#[test]
fn open_missing_file_is_58p01() {
    let path = tmp("does_not_exist.jed");
    let _ = std::fs::remove_file(&path);
    assert_eq!(Database::open(&path).err().unwrap().code(), "58P01");
}

#[test]
fn create_over_existing_file_is_58p02() {
    let path = tmp("already_here.jed");
    let _ = std::fs::remove_file(&path);
    Database::create(&path, DatabaseOptions::default()).unwrap();
    assert_eq!(
        Database::create(&path, DatabaseOptions::default())
            .err()
            .unwrap()
            .code(),
        "58P02"
    );
}

#[test]
fn create_with_custom_page_size_round_trips() {
    let path = tmp("page256.jed");
    let _ = std::fs::remove_file(&path);
    let db = Database::create(&path, DatabaseOptions { page_size: 256 }).unwrap();
    assert_eq!(db.page_size(), 256);
    db.close().unwrap();

    // The file's recorded page size survives reopen, and the on-disk u32 at offset 8 is 256.
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 256);
    let db = Database::open(&path).unwrap();
    assert_eq!(db.page_size(), 256);
}

#[test]
fn autocommit_persists_each_write_across_close() {
    // jed autocommits (spec/design/transactions.md §4.1): a write is durable as soon as it
    // succeeds, so it survives a `close` with no explicit `commit` — the opposite of the
    // original "no autocommit" model this test used to assert.
    let path = tmp("autocommit.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(&path, DatabaseOptions::default()).unwrap();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1)").unwrap(); // autocommitted, no explicit commit
    db.close().unwrap();

    let mut db = Database::open(&path).unwrap();
    match execute(&mut db, "SELECT id FROM t").unwrap() {
        Outcome::Query { rows, .. } => {
            assert_eq!(
                rows,
                vec![vec![Value::Int(1)]],
                "autocommitted insert must persist"
            )
        }
        _ => panic!("expected a query"),
    }
}

#[test]
fn commit_and_rollback_are_noops_under_autocommit() {
    // With no explicit transaction open, both are lenient no-op successes (transactions.md §4.2).
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1)").unwrap();
    db.commit().unwrap();
    db.rollback().unwrap(); // does NOT undo the autocommitted insert
    match db.query("SELECT id FROM t", &[]).unwrap().next() {
        Some(row) => assert_eq!(row, vec![Value::Int(1)]),
        None => panic!("autocommitted row must remain"),
    }
}

#[test]
fn prepare_execute_and_query_with_params() {
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, v int32)").unwrap();
    let insert = db.prepare("INSERT INTO t VALUES ($1, $2)").unwrap();
    insert
        .execute(&mut db, &[Value::Int(1), Value::Int(100)])
        .unwrap();
    insert
        .execute(&mut db, &[Value::Int(2), Value::Int(200)])
        .unwrap();

    let select = db.prepare("SELECT id, v FROM t WHERE v = $1").unwrap();
    let mut rows = select.query(&mut db, &[Value::Int(200)]).unwrap();
    assert_eq!(rows.column_names(), &["id".to_string(), "v".to_string()]);
    let collected: Vec<Vec<Value>> = rows.by_ref().collect();
    assert_eq!(collected, vec![vec![Value::Int(2), Value::Int(200)]]);
    assert!(rows.cost() >= 0);
}

#[test]
fn one_shot_query_iterates_rows() {
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (1), (2), (3)").unwrap();
    let ids: Vec<Value> = db
        .query("SELECT id FROM t", &[])
        .unwrap()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(ids, vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
}

#[test]
fn query_on_non_query_statement_errors() {
    let mut db = Database::new();
    assert!(
        db.query("CREATE TABLE t (id int32 PRIMARY KEY)", &[])
            .is_err()
    );
}

#[test]
fn errors_surface_with_sqlstate() {
    let db = Database::new();
    assert_eq!(db.prepare("SELCT 1").err().unwrap().code(), "42601");
}

#[test]
fn commit_on_in_memory_is_noop_success() {
    let mut db = Database::new();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY)").unwrap();
    db.commit().unwrap(); // no path -> no-op, not an error
    assert_eq!(db.txid(), 0);
    assert!(db.path().is_none());
}

#[test]
fn incremental_commit_round_trips_to_canonical_image() {
    // A commit is now an *incremental* dirty-page write (P6.1 part B), so the on-disk file is no
    // longer byte-identical to a from-scratch `to_image` (it carries leaked pages and only the
    // alternate meta slot is bumped). The contract instead: reopening the incrementally-written
    // file yields the identical *logical* tree — its canonical re-serialization matches the live
    // db's, byte-for-byte (spec/fileformat/format.md, *Allocation & incremental commit*).
    let path = tmp("incremental_canonical.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(&path, DatabaseOptions::default()).unwrap();
    execute(&mut db, "CREATE TABLE t (id int32 PRIMARY KEY, v int32)").unwrap();
    execute(&mut db, "INSERT INTO t VALUES (5, 50)").unwrap();
    db.commit().unwrap();
    let canonical = db.to_image(db.page_size(), db.txid()).unwrap();
    db.close().unwrap();

    let reopened = Database::open(&path).unwrap();
    assert_eq!(
        reopened
            .to_image(reopened.page_size(), reopened.txid())
            .unwrap(),
        canonical,
        "the incremental file must decode to the identical canonical image"
    );
}

#[test]
fn table_names_lists_tables_sorted_excluding_indexes() {
    // The catalog-read surface (api.md §6): canonical names, sorted ascending by
    // lowercased name; secondary indexes are relations but not tables.
    let mut db = Database::new();
    assert_eq!(db.table_names(), Vec::<String>::new());
    execute(&mut db, "CREATE TABLE Zed (id int32 PRIMARY KEY, v int32)").unwrap();
    execute(&mut db, "CREATE TABLE apple (id int32 PRIMARY KEY)").unwrap();
    execute(&mut db, "CREATE INDEX zed_v_idx ON Zed (v)").unwrap();
    // Sorted by LOWERCASED name (apple < zed), returning the canonical spelling (`Zed`).
    assert_eq!(
        db.table_names(),
        vec!["apple".to_string(), "Zed".to_string()]
    );
    // The visible snapshot includes an open transaction's working set.
    execute(&mut db, "BEGIN").unwrap();
    execute(&mut db, "CREATE TABLE mid (id int32 PRIMARY KEY)").unwrap();
    assert_eq!(db.table_names(), vec!["apple", "mid", "Zed"]);
    execute(&mut db, "ROLLBACK").unwrap();
    assert_eq!(db.table_names(), vec!["apple", "Zed"]);
}
