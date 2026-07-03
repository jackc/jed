//! Phase 7: the formal host API (spec/design/api.md) — open/create/commit/close a database
//! file, prepare/execute/query, the `Rows` cursor, and the structured-error surface. Files are
//! written under Cargo's per-test temp dir (`CARGO_TARGET_TMPDIR`), never the repo tree.

use std::path::PathBuf;

use jed::value::Value;
use jed::{CreateOptions, Database, OpenOptions, Outcome, SessionOptions};

fn tmp(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name)
}

#[test]
fn create_commit_reopen_round_trips() {
    let path = tmp("round_trip.jed");
    let _ = std::fs::remove_file(&path);

    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    assert_eq!(db.txid(), 1); // the initial empty image is committed at create
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1, 10), (2, 20)", &[])
        .unwrap();
    db.commit().unwrap();
    let after_commit = db.txid();
    drop(db);

    let mut db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(db.txid(), after_commit);
    match db.execute("SELECT id, v FROM t", &[]).unwrap() {
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
    Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    assert_eq!(
        Database::create(CreateOptions {
            path: Some(std::path::PathBuf::from(&path)),
            ..Default::default()
        })
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
    let db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        page_size: 256,
    })
    .unwrap()
    .session(SessionOptions::default());
    assert_eq!(db.page_size(), 256);
    drop(db);

    // The file's recorded page size survives reopen, and the on-disk u32 at offset 8 is 256.
    let bytes = std::fs::read(&path).unwrap();
    assert_eq!(u32::from_be_bytes(bytes[8..12].try_into().unwrap()), 256);
    let db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(db.page_size(), 256);
}

#[test]
fn autocommit_persists_each_write_across_close() {
    // jed autocommits (spec/design/transactions.md §4.1): a write is durable as soon as it
    // succeeds, so it survives a `close` with no explicit `commit` — the opposite of the
    // original "no autocommit" model this test used to assert.
    let path = tmp("autocommit.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1)", &[]).unwrap(); // autocommitted, no explicit commit
    drop(db);

    let mut db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    match db.execute("SELECT id FROM t", &[]).unwrap() {
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
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
    db.commit().unwrap();
    db.rollback().unwrap(); // does NOT undo the autocommitted insert
    match db.query("SELECT id FROM t", &[]).unwrap().next() {
        Some(row) => assert_eq!(row, vec![Value::Int(1)]),
        None => panic!("autocommitted row must remain"),
    }
}

#[test]
fn prepare_execute_and_query_with_params() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    let insert = db.prepare("INSERT INTO t VALUES ($1, $2)").unwrap();
    db.execute_prepared(&insert, &[Value::Int(1), Value::Int(100)])
        .unwrap();
    db.execute_prepared(&insert, &[Value::Int(2), Value::Int(200)])
        .unwrap();

    let select = db.prepare("SELECT id, v FROM t WHERE v = $1").unwrap();
    let mut rows = db.query_prepared(&select, &[Value::Int(200)]).unwrap();
    assert_eq!(rows.column_names(), &["id".to_string(), "v".to_string()]);
    let collected: Vec<Vec<Value>> = rows.by_ref().collect();
    assert_eq!(collected, vec![vec![Value::Int(2), Value::Int(200)]]);
    assert!(rows.cost() >= 0);
}

#[test]
fn one_shot_query_iterates_rows() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1), (2), (3)", &[])
        .unwrap();
    let ids: Vec<Value> = db
        .query("SELECT id FROM t", &[])
        .unwrap()
        .map(|r| r[0].clone())
        .collect();
    assert_eq!(ids, vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
}

#[test]
fn query_on_non_query_statement_errors() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert!(
        db.query("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
            .is_err()
    );
}

#[test]
fn errors_surface_with_sqlstate() {
    let db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(db.prepare("SELCT 1").err().unwrap().code(), "42601");
}

#[test]
fn commit_on_in_memory_is_noop_success() {
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    let before = db.txid();
    db.commit().unwrap(); // no open block -> a no-op success, not an error
    assert_eq!(db.txid(), before, "a no-op commit advances nothing");
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
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    db.execute("INSERT INTO t VALUES (5, 50)", &[]).unwrap();
    db.commit().unwrap();
    let canonical = db.to_image(db.page_size(), db.txid()).unwrap();
    drop(db);

    let reopened = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(
        reopened
            .to_image(reopened.page_size(), reopened.txid())
            .unwrap(),
        canonical,
        "the incremental file must decode to the identical canonical image"
    );
}

#[test]
fn open_read_only_blocks_writes_and_never_touches_the_file() {
    // Read-only open (api.md §2.1): the handle behaves like PostgreSQL hot standby — every
    // transaction defaults to READ ONLY, an explicit READ WRITE request and any write are
    // 25006, and the file bytes are never touched.
    let path = tmp("readonly.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        ..Default::default()
    })
    .unwrap()
    .session(SessionOptions::default());
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    db.execute("INSERT INTO t VALUES (1)", &[]).unwrap();
    drop(db);
    let before = std::fs::read(&path).unwrap();

    let mut db = Database::open_with_options(
        &path,
        OpenOptions {
            read_only: true,
            ..OpenOptions::default()
        },
    )
    .unwrap()
    .session(SessionOptions::default());
    assert!(db.read_only());

    // Reads work — bare and inside an explicit block (plain BEGIN defaults to READ ONLY here).
    match db.execute("SELECT id FROM t", &[]).unwrap() {
        Outcome::Query { rows, .. } => assert_eq!(rows, vec![vec![Value::Int(1)]]),
        _ => panic!("expected a query"),
    }
    db.execute("BEGIN", &[]).unwrap();
    db.execute("SELECT id FROM t", &[]).unwrap();
    db.execute("COMMIT", &[]).unwrap();

    // Autocommit writes are 25006 (the implicit transaction is read-only)...
    assert_eq!(
        db.execute("INSERT INTO t VALUES (2)", &[])
            .unwrap_err()
            .code(),
        "25006"
    );
    // ...as are writes inside a block (which then poisons, like any in-block error)...
    db.execute("BEGIN", &[]).unwrap();
    assert_eq!(
        db.execute("DELETE FROM t", &[]).unwrap_err().code(),
        "25006"
    );
    assert_eq!(
        db.execute("SELECT id FROM t", &[]).unwrap_err().code(),
        "25P02"
    );
    db.execute("ROLLBACK", &[]).unwrap();
    // ...and an explicit READ WRITE request, via SQL or the host API.
    assert_eq!(
        db.execute("BEGIN READ WRITE", &[]).unwrap_err().code(),
        "25006"
    );
    assert_eq!(db.begin(true).err().unwrap().code(), "25006");
    db.view(|tx| tx.query("SELECT id FROM t", &[]).map(|_| ()))
        .unwrap();
    assert_eq!(
        db.update(|tx| tx.execute("DELETE FROM t", &[]).map(|_| ()))
            .err()
            .unwrap()
            .code(),
        "25006"
    );
    drop(db);

    // The file is byte-identical after the whole read-only session.
    assert_eq!(std::fs::read(&path).unwrap(), before);

    // A normal reopen is writable again.
    let mut db = Database::open(&path)
        .unwrap()
        .session(SessionOptions::default());
    assert!(!db.read_only());
    db.execute("INSERT INTO t VALUES (2)", &[]).unwrap();
}

#[test]
fn rows_affected_reports_dml_counts() {
    // The affected-row count (api.md §4): INSERT/UPDATE/DELETE without RETURNING report
    // how many rows they touched (PostgreSQL's command-tag count); a DML statement that
    // matched nothing reports Some(0); DDL and transaction control report None; DML with
    // RETURNING is a query outcome (its row count is the result's length).
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    let affected = |out: Outcome| match out {
        Outcome::Statement { rows_affected, .. } => rows_affected,
        Outcome::Query { .. } => panic!("expected a statement outcome"),
    };

    let ddl = db
        .execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    assert_eq!(affected(ddl), None);
    let ins = db
        .execute("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)", &[])
        .unwrap();
    assert_eq!(affected(ins), Some(3));
    let upd = db
        .execute("UPDATE t SET v = v + 1 WHERE id <= 2", &[])
        .unwrap();
    assert_eq!(affected(upd), Some(2));
    let del = db.execute("DELETE FROM t WHERE id = 3", &[]).unwrap();
    assert_eq!(affected(del), Some(1));
    let none = db.execute("DELETE FROM t WHERE id = 99", &[]).unwrap();
    assert_eq!(affected(none), Some(0));
    let begin = db.execute("BEGIN", &[]).unwrap();
    assert_eq!(affected(begin), None);
    let commit = db.execute("COMMIT", &[]).unwrap();
    assert_eq!(affected(commit), None);

    // INSERT ... SELECT counts the inserted rows; DML with RETURNING is a Query.
    db.execute("CREATE TABLE dst (id i32 PRIMARY KEY)", &[])
        .unwrap();
    let ins_sel = db.execute("INSERT INTO dst SELECT id FROM t", &[]).unwrap();
    assert_eq!(affected(ins_sel), Some(2));
    match db.execute("DELETE FROM dst RETURNING id", &[]).unwrap() {
        Outcome::Query { rows, .. } => assert_eq!(rows.len(), 2),
        _ => panic!("RETURNING must yield a query outcome"),
    }
}

#[test]
fn table_names_lists_tables_sorted_excluding_indexes() {
    // The catalog-read surface (api.md §6): canonical names, sorted ascending by
    // lowercased name; secondary indexes are relations but not tables.
    let mut db = Database::create(CreateOptions::default())
        .unwrap()
        .session(SessionOptions::default());
    assert_eq!(db.table_names(), Vec::<String>::new());
    db.execute("CREATE TABLE Zed (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    db.execute("CREATE TABLE apple (id i32 PRIMARY KEY)", &[])
        .unwrap();
    db.execute("CREATE INDEX zed_v_idx ON Zed (v)", &[])
        .unwrap();
    // Sorted by LOWERCASED name (apple < zed), returning the canonical spelling (`Zed`).
    assert_eq!(
        db.table_names(),
        vec!["apple".to_string(), "Zed".to_string()]
    );
    // The visible snapshot includes an open transaction's working set.
    db.execute("BEGIN", &[]).unwrap();
    db.execute("CREATE TABLE mid (id i32 PRIMARY KEY)", &[])
        .unwrap();
    assert_eq!(db.table_names(), vec!["apple", "mid", "Zed"]);
    db.execute("ROLLBACK", &[]).unwrap();
    assert_eq!(db.table_names(), vec!["apple", "Zed"]);
}
