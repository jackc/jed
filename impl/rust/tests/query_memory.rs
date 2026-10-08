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
    // The columnar projection lane gathers the id column (100 values × 32 = 3200 bytes of lane state,
    // memory.md §6.5), held until its emission completes.
    s.set_max_query_memory_bytes(3200);
    // Draining by hand: the lane is the engine's, the rows are the host's own memory.
    assert_eq!(s.query("SELECT id FROM t", &[]).unwrap().count(), 100);
    // The engine-owned collector also charges each collected row (32 + 32 bytes): 3200 + 6400.
    s.set_max_query_memory_bytes(3200 + 64 * 100);
    assert_eq!(s.query_rows("SELECT id FROM t", ()).unwrap().len(), 100);
    s.set_max_query_memory_bytes(3200 + 64 * 100 - 1);
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

#[test]
fn rejected_collector_admission_reports_a_reached_cost_ceiling() {
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut s = db.session(SessionOptions::default());
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY)", &[])
        .unwrap();
    s.execute("INSERT INTO t SELECT g FROM generate_series(1, 100) g", &[])
        .unwrap();
    // A streaming scan holds no row buffer; only the collector charges (64 bytes per row).
    let sql = "SELECT id FROM t LIMIT 1000";
    let mut drained = s.query(sql, &[]).unwrap();
    while drained.next().is_some() {}
    let cost = drained.cost();
    drop(drained);
    // At the exact ceiling the scan's next guard aborts once every row is out — with or without an
    // ample budget, which adds no guard point of its own.
    s.set_max_cost(cost);
    assert_eq!(s.query_rows(sql, ()).err().expect("54P01").code(), "54P01");
    s.set_max_query_memory_bytes(64 * 100);
    assert_eq!(s.query_rows(sql, ()).err().expect("54P01").code(), "54P01");
    // When the last row's admission is rejected — right after its charge reached the ceiling and
    // before the scan's next guard — the reached ceiling wins.
    s.set_max_query_memory_bytes(64 * 100 - 1);
    assert_eq!(s.query_rows(sql, ()).err().expect("54P01").code(), "54P01");
    // Below the ceiling the memory error stands.
    s.set_max_cost(cost + 1);
    assert_eq!(s.query_rows(sql, ()).err().expect("54P05").code(), "54P05");
}

/// Spill-capable operator state charges only its resident portion (memory.md §6.6): on a file-backed
/// database the bounded-spill lane's structures release their charge as they spill, so a budget that
/// a fully resident DISTINCT exceeds is enough once `work_mem` makes it spill — and the account itself
/// never forces the spill. Disk-only behavior, so it lives here rather than in the corpus.
#[test]
fn spilling_operator_state_releases_its_charge() {
    let path = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("query_memory_spill.jed");
    let _ = std::fs::remove_file(&path);
    let db = Database::create(CreateOptions {
        path: Some(path.clone()),
        skip_fsync: true,
        ..Default::default()
    })
    .unwrap();
    let mut s = db.session(SessionOptions::default());
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY, s text)", &[])
        .unwrap();
    s.execute(
        "INSERT INTO t SELECT g, repeat('x', g) FROM generate_series(1, 300) g",
        &[],
    )
    .unwrap();
    let sql = "SELECT DISTINCT s FROM t";
    let peak = |s: &mut jed::Session, work_mem: usize| -> i64 {
        s.set_work_mem(work_mem);
        s.set_max_query_memory_bytes(1 << 40);
        jed::tooling::QUERY_MEMORY_PEAK.store(0, std::sync::atomic::Ordering::Relaxed);
        s.query_outcome(sql, &[]).unwrap();
        jed::tooling::QUERY_MEMORY_PEAK.load(std::sync::atomic::Ordering::Relaxed)
    };
    let resident = peak(&mut s, 1 << 30);
    let spilling = peak(&mut s, 4096);
    assert!(
        spilling < resident / 4,
        "spilling peak {spilling} vs resident {resident}"
    );
    // The spilling run fits a budget the resident run exceeds.
    s.set_max_query_memory_bytes(spilling);
    s.set_work_mem(4096);
    match s.query_outcome(sql, &[]).unwrap() {
        jed::Outcome::Query { rows, .. } => assert_eq!(rows.len(), 300),
        other => panic!("expected rows, got {other:?}"),
    }
    s.set_work_mem(1 << 30);
    let err = s.query_outcome(sql, &[]).err().expect("54P05");
    assert_eq!(err.code(), "54P05");
    drop(s);
    drop(db);
    let _ = std::fs::remove_file(&path);
}
