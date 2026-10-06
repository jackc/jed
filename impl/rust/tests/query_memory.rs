//! Host-API surface of the live query-memory budget (spec/design/memory.md §2/§5): the setting's
//! default and normalization, cursor lifetime, and the engine-owned `query_rows` collector. Abort
//! thresholds themselves are pinned in spec/conformance/suites/resource/query_memory.test.

use jed::{CreateOptions, Database, SessionOptions};

#[test]
fn query_memory_setting_defaults_to_unlimited() {
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut s = db.session(SessionOptions::default());
    assert_eq!(s.max_query_memory_bytes(), 0);
    s.set_max_query_memory_bytes(4096);
    assert_eq!(s.max_query_memory_bytes(), 4096);
    s.set_max_query_memory_bytes(-1);
    assert_eq!(s.max_query_memory_bytes(), 0);
    // Unlimited never aborts, even for a large materialized result.
    let n = s
        .query(
            "SELECT repeat('x', 1000) FROM generate_series(1, 2000)",
            &[],
        )
        .unwrap()
        .count();
    assert_eq!(n, 2000);
}

#[test]
fn query_rows_collector_is_admitted() {
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut s = db.session(SessionOptions::default());
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    s.execute("INSERT INTO t SELECT g FROM generate_series(1, 100) g", &[])
        .unwrap();
    s.set_max_query_memory_bytes(64 * 100);
    // A streaming scan holds no row buffer; draining it by hand is the host's own memory.
    assert_eq!(s.query("SELECT id FROM t", &[]).unwrap().count(), 100);
    // The engine-owned collector charges each collected row (32 + 32 bytes): 100 rows need 6400.
    assert_eq!(s.query_rows("SELECT id FROM t", ()).unwrap().len(), 100);
    s.set_max_query_memory_bytes(64 * 100 - 1);
    let err = s.query_rows("SELECT id FROM t", ()).err().expect("54P05");
    assert_eq!(err.code(), "54P05");
}

#[test]
fn query_memory_budget_stays_with_cursor() {
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut s = db.session(SessionOptions {
        max_query_memory_bytes: 1 << 20,
        ..Default::default()
    });
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    s.execute("INSERT INTO t VALUES (1), (2)", &[]).unwrap();
    // A buffered (sorted) result opened under 1 MiB keeps that budget after the setting changes.
    let mut old = s.query("SELECT id FROM t ORDER BY id DESC", &[]).unwrap();
    s.set_max_query_memory_bytes(1);
    let err = s
        .query_rows("SELECT id FROM t ORDER BY id DESC", ())
        .err()
        .expect("54P05");
    assert_eq!(err.code(), "54P05");
    assert!(old.next().is_some());
    assert!(old.next().is_some());
    assert!(old.next().is_none());
    old.error().unwrap();
}
