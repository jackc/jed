//! S3/S4: the lazy result cursor (spec/design/streaming.md §3/§4/§5/§6). The conformance corpus
//! drives the materialized `execute()` path, so the lazy cursor — which only affects `query()` →
//! `Rows` — is internal machinery the corpus cannot reach (CLAUDE.md §10). These per-core tests pin
//! the contract: a fully-drained query yields the IDENTICAL rows + total cost as the eager path (§6);
//! a caller that stops early reads (and charges) less (the early-exit win, §6); the cursor pins its
//! snapshot for its life (§5); and a mid-drain error surfaces (§6).
//!
//! The first group covers the **S3 `Streaming`** cursor — the single-table no-blocking-operator scan
//! (PK-ordered / LIMIT short-circuit). The second group (suffixed `buffered_`) covers the **S4
//! `Buffered`** cursor — a blocking plan (non-PK `ORDER BY`, `DISTINCT`, aggregate, window, join)
//! whose input buffers but whose OUTPUT is yielded one row at a time (the `SortedRows::next()` pattern
//! generalized): same rows + total cost under full drain, but a caller's early exit skips the
//! projection of the rows it never pulls (the top-N-over-the-buffer win, §4).

use jed::value::Value;
use jed::{CreateOptions, Database, Outcome, Session, SessionOptions};

/// Seed an in-memory shared db with `t(id i32 PK, v i32)` holding `1..=n` (v = id * 10).
fn seeded(n: i64) -> Database {
    let db = Database::create(CreateOptions::default()).unwrap();
    let mut w = db.write_session();
    w.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    for i in 1..=n {
        w.query_outcome(&format!("INSERT INTO t VALUES ({i}, {})", i * 10), &[])
            .unwrap();
    }
    w.commit().unwrap();
    db
}

/// The materialized (`execute()`) result: rows + total cost — the oracle the streaming cursor must
/// match under full drain (§6).
fn eager(sess: &mut Session, sql: &str) -> (Vec<Vec<Value>>, i64) {
    match sess.query_outcome(sql, &[]).unwrap() {
        Outcome::Query { rows, cost, .. } => (rows, cost),
        _ => panic!("expected a query"),
    }
}

/// The streaming (`query()`) result, fully drained: rows + final cost.
fn streamed(sess: &mut Session, sql: &str) -> (Vec<Vec<Value>>, i64) {
    let mut rows = sess.query(sql, &[]).unwrap();
    let mut out = Vec::new();
    for r in &mut rows {
        out.push(r);
    }
    rows.error().unwrap();
    (out, rows.cost())
}

/// Every streamable shape: `query()` (lazy) must equal `execute()` (eager) on rows AND total cost.
#[test]
fn streaming_matches_eager_rows_and_cost() {
    let db = seeded(100);
    let mut s = db.session(SessionOptions::default());
    for sql in [
        // No ORDER BY + LIMIT: the LIMIT short-circuit.
        "SELECT id, v FROM t LIMIT 5",
        "SELECT id, v FROM t LIMIT 5 OFFSET 10",
        // ORDER BY satisfied by the PK scan (forward + reverse), with and without LIMIT.
        "SELECT id, v FROM t ORDER BY id",
        "SELECT id, v FROM t ORDER BY id LIMIT 7",
        "SELECT id, v FROM t ORDER BY id DESC LIMIT 7",
        // A WHERE filter (residual), PK-bounded and unbounded.
        "SELECT id, v FROM t WHERE v > 500 ORDER BY id",
        "SELECT id FROM t WHERE id >= 90 ORDER BY id",
        // DISTINCT in PK-scan order.
        "SELECT v FROM t ORDER BY id LIMIT 3",
        // A projection expression (operator_eval per row).
        "SELECT id, v + 1 FROM t ORDER BY id LIMIT 4",
        // Empty result (a PK point that misses) still matches.
        "SELECT id FROM t WHERE id = 9999",
    ] {
        let (er, ec) = eager(&mut s, sql);
        let (sr, sc) = streamed(&mut s, sql);
        assert_eq!(sr, er, "rows mismatch: {sql}");
        assert_eq!(sc, ec, "cost mismatch: {sql}");
    }
}

/// A non-streamable shape (aggregate / no-PK-ordered DISTINCT / join) still works through `query()`
/// — it falls back to the buffered cursor and matches `execute()`.
#[test]
fn non_streamable_falls_back_and_matches() {
    let db = seeded(20);
    let mut s = db.session(SessionOptions::default());
    for sql in [
        "SELECT count(*) FROM t",                 // aggregate → Buffered
        "SELECT v FROM t ORDER BY v",             // ORDER BY not satisfied by PK scan → Buffered
        "SELECT DISTINCT v FROM t",               // no-PK-ordered DISTINCT → Buffered
        "SELECT id FROM t a JOIN t b USING (id)", // join → Buffered
    ] {
        let (er, ec) = eager(&mut s, sql);
        let (sr, sc) = streamed(&mut s, sql);
        // Both unordered shapes are deterministic here (single relation, PK order), so compare directly.
        assert_eq!(sr, er, "rows mismatch: {sql}");
        assert_eq!(sc, ec, "cost mismatch: {sql}");
    }
}

/// Early exit (§6): pulling only a prefix does LESS work than draining — fewer `storage_row_read`
/// charges — and yields exactly the prefix. The streaming win the materialized path cannot offer.
#[test]
fn early_exit_reads_and_charges_less() {
    let db = seeded(1000);
    let mut s = db.session(SessionOptions::default());

    // Drain the whole table: cost includes a storage_row_read per row.
    let (full_rows, full_cost) = streamed(&mut s, "SELECT id FROM t ORDER BY id");
    assert_eq!(full_rows.len(), 1000);

    // Pull only the first 3 rows, then drop the cursor — far fewer rows scanned, so far less cost.
    let mut rows = s.query("SELECT id FROM t ORDER BY id", &[]).unwrap();
    let prefix: Vec<Value> = (&mut rows).take(3).map(|r| r[0].clone()).collect();
    let partial_cost = rows.cost();
    drop(rows);

    assert_eq!(
        prefix,
        vec![Value::Int(1), Value::Int(2), Value::Int(3)],
        "early pull yields the prefix"
    );
    assert!(
        partial_cost < full_cost,
        "early exit must charge less than a full drain (partial={partial_cost}, full={full_cost})"
    );
}

/// Snapshot pinning (§5): a streaming cursor reads the snapshot it opened on, even as a concurrent
/// writer commits new rows — and the watermark holds at the cursor's version until it is dropped.
#[test]
fn streaming_cursor_pins_its_snapshot_and_watermark() {
    let db = seeded(3); // version 1, ids 1..=3
    assert_eq!(db.version(), 1);
    assert_eq!(db.oldest_live_txid(), 1);

    let mut reader = db.session(SessionOptions::default());
    // Open a streaming cursor over the v1 snapshot but pull only ONE row (cursor stays live).
    let mut rows = reader.query("SELECT id FROM t ORDER BY id", &[]).unwrap();
    let first = (&mut rows).next().unwrap();
    assert_eq!(first, vec![Value::Int(1)]);
    // The live cursor pins v1 in the watermark.
    assert_eq!(db.oldest_live_txid(), 1, "open cursor pins its version");

    // A concurrent writer commits two more rows (version 2) while the cursor is open.
    {
        let mut w = db.write_session();
        w.query_outcome("INSERT INTO t VALUES (4, 40), (5, 50)", &[])
            .unwrap();
        w.commit().unwrap();
    }
    assert_eq!(db.version(), 2);
    assert_eq!(
        db.oldest_live_txid(),
        1,
        "watermark held at the cursor's pin"
    );

    // Draining the rest of the cursor sees ONLY the v1 snapshot (ids 2, 3) — not the writer's rows.
    let rest: Vec<Value> = (&mut rows).map(|r| r[0].clone()).collect();
    assert_eq!(
        rest,
        vec![Value::Int(2), Value::Int(3)],
        "frozen at open-time root"
    );
    rows.error().unwrap();

    // Closing the cursor releases the pin; the watermark advances.
    rows.close();
    drop(rows);
    assert_eq!(db.oldest_live_txid(), 2, "closed cursor releases its pin");

    // A fresh streaming read sees the writer's rows.
    let (fresh, _) = streamed(&mut reader, "SELECT id FROM t ORDER BY id");
    assert_eq!(
        fresh,
        vec![
            vec![Value::Int(1)],
            vec![Value::Int(2)],
            vec![Value::Int(3)],
            vec![Value::Int(4)],
            vec![Value::Int(5)],
        ]
    );
}

/// A mid-drain cost-ceiling abort (§6): the `54P01` surfaces during iteration (the cursor stops and
/// `error()` returns it), not at `query()` time.
#[test]
fn mid_drain_cost_abort_surfaces() {
    let db = seeded(1000);
    let mut s = db.session(SessionOptions::default());
    // A tiny ceiling: building the cursor is fine, but draining trips the per-row meter guard.
    s.set_max_cost(50);
    let mut rows = s.query("SELECT id FROM t ORDER BY id", &[]).unwrap();
    // Iterate until the cursor stops (the meter guard aborts mid-drain).
    let mut n = 0;
    for _ in &mut rows {
        n += 1;
        if n > 10_000 {
            panic!("the cost ceiling should have aborted the drain");
        }
    }
    let err = rows
        .error()
        .expect_err("a mid-drain cost abort must surface");
    assert_eq!(err.code(), "54P01", "the abort is a cost-limit error");
}

/// The bare `Database::query` convenience streams too: the transient mint-a-session does not strand
/// the cursor (it owns its snapshot), and the result matches a fresh session's eager path.
#[test]
fn database_query_convenience_streams() {
    let mut db = seeded(50);
    let mut rows = db
        .query("SELECT id, v FROM t ORDER BY id LIMIT 4", &[])
        .unwrap();
    let out: Vec<Vec<Value>> = (&mut rows).collect();
    rows.error().unwrap();
    assert_eq!(
        out,
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(20)],
            vec![Value::Int(3), Value::Int(30)],
            vec![Value::Int(4), Value::Int(40)],
        ]
    );
}

/// The bare-handle `Database::query` path pins the reader-liveness watermark exactly like a `Session`
/// query (streaming.md §7 closing note): the fresh per-call session's provisional pin transfers to
/// the `Rows`, so a held bare-handle cursor holds `oldest_live_txid`, keeps within-session
/// reclamation (v25) from recycling its snapshot's pages under compacting commit churn, and releases
/// the watermark on close. File-backed with a tiny page size so the churn actually orphans pages
/// (an in-memory db cannot exercise the persisted-free-list reuse path). The workload is the same
/// deterministic one the Go/TS twins run with white-box free-list-generation instrumentation — the
/// §8 cross-core byte-identity makes their "the churn compacts once unpinned" proof carry here.
#[test]
fn bare_handle_query_pins_watermark_under_reclamation() {
    let path =
        std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("bare_handle_watermark.jed");
    let _ = std::fs::remove_file(&path);
    let mut db = Database::create(CreateOptions {
        path: Some(path.clone()),
        page_size: 256,
        skip_fsync: true,
    })
    .unwrap();
    db.execute("CREATE TABLE t (id i64 PRIMARY KEY, v i64)", &[])
        .unwrap();
    // A multi-leaf tree; every churn commit below rewrites all of it (whole-table UPDATE), orphaning
    // the prior tree so unpinned within-session compaction would reclaim + reuse its pages.
    for i in 1..=120 {
        db.execute(&format!("INSERT INTO t VALUES ({i}, {})", i * 10), &[])
            .unwrap();
    }
    let v0 = db.version();
    assert_eq!(db.oldest_live_txid(), v0, "idle watermark = committed");

    // Open a bare-handle streaming cursor (the transient session closes before `query` returns; the
    // pin rides the `Rows`) and pull ONE row so the scan is live mid-tree.
    let mut rows = db.query("SELECT id, v FROM t ORDER BY id", &[]).unwrap();
    let first = (&mut rows).next().unwrap();
    assert_eq!(first, vec![Value::Int(1), Value::Int(10)]);
    assert_eq!(
        db.oldest_live_txid(),
        v0,
        "open bare-handle cursor pins its version"
    );

    // Churn: whole-table UPDATE commits through the same bare handle — each orphans every leaf plus
    // the spine, so on an ungated path reuse would recycle the cursor's pinned pages.
    for _ in 0..150 {
        db.execute("UPDATE t SET v = v + 1", &[]).unwrap();
    }
    assert_eq!(
        db.oldest_live_txid(),
        v0,
        "watermark held at the cursor's pin through the churn"
    );

    // Drain: the cursor must see EXACTLY its frozen snapshot (v = id * 10), untouched by the churn.
    let mut want = 2i64;
    for r in &mut rows {
        assert_eq!(
            r,
            vec![Value::Int(want), Value::Int(want * 10)],
            "SNAPSHOT ISOLATION VIOLATED: the cursor's pages were reclaimed and overwritten"
        );
        want += 1;
    }
    rows.error().unwrap();
    assert_eq!(want, 121, "drained the full pinned snapshot 2..=120");
    rows.close();
    drop(rows);
    assert_eq!(
        db.oldest_live_txid(),
        db.version(),
        "closed cursor releases its pin"
    );
}

// ---- S4: the lazy BUFFERED cursor (a blocking plan; streaming.md §4) ------------------------------

/// Every blocking shape (aggregate / non-PK `ORDER BY` / `DISTINCT` / window / join / `GROUP BY`):
/// `query()` (the lazy `Buffered` cursor) must equal `execute()` (eager) on rows AND total cost under
/// full drain (§6). These all route through `try_buffered_query` → `BufferedScan`, not the streaming
/// fast lane.
#[test]
fn buffered_matches_eager_rows_and_cost() {
    let db = seeded(40);
    let mut s = db.session(SessionOptions::default());
    for sql in [
        "SELECT count(*) FROM t", // whole-table aggregate (Final, 1 row)
        "SELECT sum(v), avg(v), min(id) FROM t", // multi-aggregate
        "SELECT v FROM t ORDER BY v", // ORDER BY the PK scan does NOT satisfy (Final sort)
        "SELECT v FROM t ORDER BY v DESC LIMIT 6", // top-N over a non-PK sort
        "SELECT DISTINCT v FROM t ORDER BY v", // no-PK DISTINCT then sort (Identity)
        "SELECT id, v + 1 FROM t GROUP BY id, v ORDER BY id", // GROUP BY + projection expr (Project)
        "SELECT id, v FROM t GROUP BY id, v HAVING v > 200 ORDER BY id", // HAVING
        "SELECT a.id, b.v FROM t a JOIN t b USING (id) ORDER BY a.id", // join + ORDER BY (Project)
        "SELECT sum(v) OVER (ORDER BY id) FROM t ORDER BY id", // window function
    ] {
        let (er, ec) = eager(&mut s, sql);
        let (sr, sc) = streamed(&mut s, sql);
        assert_eq!(sr, er, "rows mismatch: {sql}");
        assert_eq!(sc, ec, "cost mismatch: {sql}");
    }
}

/// Early exit over a `Buffered` cursor in `Project` mode (§4): the blocking part (scan + group + sort)
/// runs in full on the first pull, but a caller that stops after a prefix skips the PROJECTION of
/// every row it never pulls — so it charges LESS (`row_produced` + projection per skipped row) than a
/// full drain. The top-N-over-the-buffer win the materialized path cannot offer.
#[test]
fn buffered_early_exit_charges_less() {
    let db = seeded(1000);
    let mut s = db.session(SessionOptions::default());

    // A GROUP BY with one group per row + a projection expression → a 1000-row Project buffer.
    let sql = "SELECT id, v + 1 FROM t GROUP BY id, v ORDER BY id";
    let (full_rows, full_cost) = streamed(&mut s, sql);
    assert_eq!(full_rows.len(), 1000);

    let mut rows = s.query(sql, &[]).unwrap();
    let prefix: Vec<Vec<Value>> = (&mut rows).take(3).collect();
    let partial_cost = rows.cost();
    drop(rows);

    assert_eq!(
        prefix,
        vec![
            vec![Value::Int(1), Value::Int(11)],
            vec![Value::Int(2), Value::Int(21)],
            vec![Value::Int(3), Value::Int(31)],
        ],
        "early pull yields the prefix"
    );
    assert!(
        partial_cost < full_cost,
        "early exit over a buffered cursor must charge less (partial={partial_cost}, full={full_cost})"
    );
}

/// Snapshot pinning (§5) for the `Buffered` cursor: it captures its snapshot at `query()` time (the
/// blocking part materializes from THAT snapshot on first pull), so a concurrent writer's rows never
/// appear; the watermark holds at the cursor's version until it is closed.
#[test]
fn buffered_cursor_pins_its_snapshot_and_watermark() {
    let db = seeded(3); // version 1, ids 1..=3
    assert_eq!(db.oldest_live_txid(), 1);

    let mut reader = db.session(SessionOptions::default());
    // A blocking query (ORDER BY v — not PK order) → the buffered cursor. Pull one row (this runs the
    // blocking part over the v1 snapshot), keep the cursor live.
    let mut rows = reader.query("SELECT v FROM t ORDER BY v", &[]).unwrap();
    let first = (&mut rows).next().unwrap();
    assert_eq!(first, vec![Value::Int(10)]);
    assert_eq!(
        db.oldest_live_txid(),
        1,
        "open buffered cursor pins its version"
    );

    // A concurrent writer commits two more rows (version 2) while the cursor is open.
    {
        let mut w = db.write_session();
        w.query_outcome("INSERT INTO t VALUES (4, 40), (5, 50)", &[])
            .unwrap();
        w.commit().unwrap();
    }
    assert_eq!(db.version(), 2);
    assert_eq!(
        db.oldest_live_txid(),
        1,
        "watermark held at the cursor's pin"
    );

    // Draining the rest sees ONLY the v1 snapshot (v = 20, 30) — not the writer's rows.
    let rest: Vec<Value> = (&mut rows).map(|r| r[0].clone()).collect();
    assert_eq!(
        rest,
        vec![Value::Int(20), Value::Int(30)],
        "frozen at open-time root"
    );
    rows.error().unwrap();

    rows.close();
    drop(rows);
    assert_eq!(
        db.oldest_live_txid(),
        2,
        "closed buffered cursor releases its pin"
    );
}

/// A mid-drain cost-ceiling abort (§6) for the `Buffered` cursor: building the cursor does NOT run the
/// blocking part (it is deferred to the first pull), so `query()` succeeds and the `54P01` surfaces
/// during iteration — exactly as for the streaming cursor.
#[test]
fn buffered_mid_drain_cost_abort_surfaces() {
    let db = seeded(1000);
    let mut s = db.session(SessionOptions::default());
    s.set_max_cost(50);
    // Building the buffered cursor must not throw (the blocking scan runs on first pull).
    let mut rows = s.query("SELECT v FROM t ORDER BY v", &[]).unwrap();
    let mut n = 0;
    for _ in &mut rows {
        n += 1;
        if n > 10_000 {
            panic!("the cost ceiling should have aborted the drain");
        }
    }
    let err = rows
        .error()
        .expect_err("a mid-drain cost abort must surface");
    assert_eq!(err.code(), "54P01", "the abort is a cost-limit error");
}

// ---- the lazy streaming-SORT output (Emitter::Sorted; streaming.md §4/§7) ------------------------

/// Every streaming-external-sort shape (a single-table non-PK `ORDER BY`): `query()` (the lazy
/// `Emitter::Sorted` drive — pulling the `SortedRows` iterator one row at a time) must equal
/// `execute()` (the eager drive of the SAME emitter) on rows AND total cost under full drain (§6).
#[test]
fn sorted_matches_eager_rows_and_cost() {
    let db = seeded(40);
    let mut s = db.session(SessionOptions::default());
    for sql in [
        "SELECT v FROM t ORDER BY v",         // non-PK sort, full output
        "SELECT v FROM t ORDER BY v DESC",    // descending
        "SELECT v FROM t ORDER BY v LIMIT 7", // top-N window
        "SELECT v FROM t ORDER BY v LIMIT 7 OFFSET 5", // LIMIT + OFFSET window
        "SELECT v FROM t ORDER BY v OFFSET 35", // OFFSET near the end (tail window)
        "SELECT id, v + 1 FROM t ORDER BY v", // a projection expression (operator_eval per row)
        "SELECT v FROM t WHERE id > 20 ORDER BY v", // a residual WHERE filter
        "SELECT v FROM t WHERE id > 99999 ORDER BY v", // empty result
    ] {
        let (er, ec) = eager(&mut s, sql);
        let (sr, sc) = streamed(&mut s, sql);
        assert_eq!(sr, er, "rows mismatch: {sql}");
        assert_eq!(sc, ec, "cost mismatch: {sql}");
    }
}

/// Early exit over the lazy streaming-sort output (§4/§7) — the headline win of this slice. The sort's
/// INPUT is blocking (every row is scanned + sorted on the first pull), but the OUTPUT is now yielded
/// from the `SortedRows` iterator one row at a time, so a caller that stops after a prefix skips the
/// `row_produced` + projection of every windowed row it never pulls — charging LESS than a full drain.
/// (Before this slice the sort output was an `Emitter::Final`, fully built + charged on the first pull,
/// so an early exit charged the SAME — this test is what distinguishes the new behavior.)
#[test]
fn sorted_early_exit_charges_less() {
    let db = seeded(1000);
    let mut s = db.session(SessionOptions::default());

    // A non-PK ORDER BY with no LIMIT → the streaming external sort → a 1000-row lazy `Sorted` output.
    let sql = "SELECT v FROM t ORDER BY v";
    let (full_rows, full_cost) = streamed(&mut s, sql);
    assert_eq!(full_rows.len(), 1000);

    let mut rows = s.query(sql, &[]).unwrap();
    let prefix: Vec<Value> = (&mut rows).take(3).map(|r| r[0].clone()).collect();
    let partial_cost = rows.cost();
    drop(rows);

    assert_eq!(
        prefix,
        vec![Value::Int(10), Value::Int(20), Value::Int(30)],
        "early pull yields the sorted prefix"
    );
    assert!(
        partial_cost < full_cost,
        "early exit over the lazy sort output must charge less (partial={partial_cost}, full={full_cost})"
    );
}

/// The lazy streaming-sort output over the SPILLING merge path (`SortedRows::Merge`): a file-backed
/// database under a tiny `work_mem` forces many spilled runs + a k-way merge. A full lazy drain must
/// match the eager result (rows + cost — spill is invariant, spill.md §6), and an early exit must yield
/// exactly the prefix while leaving NO spill temp file behind (the `Merger`'s `Drop` cleanup fires when
/// the lazy cursor is dropped undrained — §5).
#[test]
fn sorted_spill_merge_streams_lazily() {
    // Isolate the spill files in a unique subdir so the run-file count is not raced by other tests
    // (spill runs land next to the database file — executor `new_sorter`).
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("sorted_spill_lazy_dir");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("db.jed");

    let count_spill_files = || -> usize {
        std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("jed-spill-"))
            .count()
    };

    let db = Database::create(CreateOptions {
        path: Some(std::path::PathBuf::from(&path)),
        skip_fsync: true,
        ..Default::default()
    })
    .unwrap();
    {
        let mut w = db.write_session();
        w.query_outcome("CREATE TABLE t (id i32 PRIMARY KEY, k i32)", &[])
            .unwrap();
        for id in 0..200i64 {
            let k = (id * 48271) % 100; // scrambled key with many duplicates
            w.query_outcome(&format!("INSERT INTO t VALUES ({id}, {k})"), &[])
                .unwrap();
        }
        w.commit().unwrap();
    }

    // Eager oracle: a default-work_mem session never spills 200 small rows (in-memory sort).
    let sql = "SELECT id, k FROM t ORDER BY k, id";
    let (er, ec) = eager(&mut db.session(SessionOptions::default()), sql);

    // K=5 fits the shared fixed-row estimate under 512 bytes (5 × (8 + 2×40) = 440), so the
    // blocking first pull owns no spill run. At 128 bytes the rule falls back to the existing
    // external sorter, whose undrained merge keeps runs live until drop.
    {
        let mut s = db.session(SessionOptions::default());
        s.set_work_mem(512);
        let mut rows = s.query(&format!("{sql} LIMIT 5"), &[]).unwrap();
        assert!(rows.next().is_some());
        assert_eq!(count_spill_files(), 0, "fitting top-k creates no run");
    }
    {
        let mut s = db.session(SessionOptions::default());
        s.set_work_mem(128);
        let mut rows = s.query(&format!("{sql} LIMIT 5"), &[]).unwrap();
        assert!(rows.next().is_some());
        assert!(
            count_spill_files() > 0,
            "top-k over work_mem falls back to external sort"
        );
        drop(rows);
    }
    assert_eq!(count_spill_files(), 0, "fallback drop cleans its runs");

    // Full lazy drain under a tiny work_mem (forces spill + merge): rows + cost match the oracle.
    {
        let mut s = db.session(SessionOptions::default());
        s.set_work_mem(128); // ~2-3 rows per run → dozens of runs + a deep merge
        let (sr, sc) = streamed(&mut s, sql);
        assert_eq!(sr, er, "spilling lazy drain rows must match eager");
        assert_eq!(sc, ec, "spilling lazy drain cost must match eager");
    }
    assert_eq!(count_spill_files(), 0, "a full drain leaves no spill file");

    // Early exit over the merge: pull a prefix, then drop the cursor. The undrained `Merger`'s `Drop`
    // deletes its run files, so none leak.
    {
        let mut s = db.session(SessionOptions::default());
        s.set_work_mem(128);
        let mut rows = s.query(sql, &[]).unwrap();
        let prefix: Vec<Vec<Value>> = (&mut rows).take(5).collect();
        assert_eq!(
            prefix,
            er[..5].to_vec(),
            "early pull yields the sorted prefix"
        );
        drop(rows);
    }
    assert_eq!(count_spill_files(), 0, "an early exit leaves no spill file");

    drop(db);
    let _ = std::fs::remove_dir_all(&dir);
}

// ---- the lazy DEFERRED cursor (a top-level set-op / WITH; streaming.md §7) ------------------------

/// Every top-level set operation / pure-query `WITH`: `query()` (the lazy `DeferredResult` cursor)
/// must equal `execute()` (eager) on rows AND total cost under full drain (§6). These route through
/// `try_deferred_query`, which reuses the eager `run_set_op` / `run_with` verbatim, so the rows + cost
/// are identical by construction (the unordered shapes are deterministic here — same snapshot, same
/// code path).
#[test]
fn deferred_matches_eager_rows_and_cost() {
    let db = seeded(20);
    let mut s = db.session(SessionOptions::default());
    for sql in [
        // Set operations (every kind), with and without a trailing ORDER BY.
        "SELECT v FROM t WHERE id <= 3 UNION SELECT v FROM t WHERE id >= 18 ORDER BY v",
        "SELECT id FROM t UNION ALL SELECT id FROM t ORDER BY id",
        "SELECT v FROM t WHERE id <= 10 INTERSECT SELECT v FROM t WHERE id >= 5 ORDER BY v",
        "SELECT v FROM t EXCEPT SELECT v FROM t WHERE id <= 12 ORDER BY v",
        "SELECT v FROM t WHERE id = 1 UNION SELECT v FROM t WHERE id = 2", // unordered, still deterministic
        // Pure-query WITH: a CTE feeding a scan, an aggregate, and a join.
        "WITH x AS (SELECT id, v FROM t WHERE v > 100) SELECT id, v FROM x ORDER BY id",
        "WITH x AS (SELECT id FROM t) SELECT count(*) FROM x",
        "WITH a AS (SELECT id, v FROM t WHERE id <= 5) SELECT a.id, a.v FROM a JOIN t USING (id) ORDER BY a.id",
        // A recursive WITH (the working-table fixpoint runs entirely on the first pull).
        "WITH RECURSIVE c(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM c WHERE n < 8) SELECT n FROM c ORDER BY n",
        // A WITH whose body is itself a set operation.
        "WITH x AS (SELECT v FROM t) SELECT v FROM x WHERE v <= 50 UNION SELECT v FROM x WHERE v >= 180 ORDER BY v",
    ] {
        let (er, ec) = eager(&mut s, sql);
        let (sr, sc) = streamed(&mut s, sql);
        assert_eq!(sr, er, "rows mismatch: {sql}");
        assert_eq!(sc, ec, "cost mismatch: {sql}");
    }
}

/// The deferred cursor's defining trait (§7): a set-op / `WITH` has no per-row top-level projection to
/// defer, so the WHOLE query runs on the FIRST pull — unlike S3/S4, an early exit charges the SAME as a
/// full drain (the only win is lazy-yield, not early-exit). This pins that the cost after one pull is
/// already final.
#[test]
fn deferred_runs_fully_on_first_pull() {
    let db = seeded(100);
    let mut s = db.session(SessionOptions::default());
    let sql = "SELECT id FROM t UNION ALL SELECT id FROM t ORDER BY id";

    let (full_rows, full_cost) = streamed(&mut s, sql);
    assert_eq!(full_rows.len(), 200);

    // Pull just one row, then read the cost: the whole run already happened on that first pull.
    let mut rows = s.query(sql, &[]).unwrap();
    let _first = (&mut rows).next().unwrap();
    let after_one = rows.cost();
    rows.error().unwrap();
    assert_eq!(
        after_one, full_cost,
        "a deferred set-op/WITH accrues its full cost on the first pull (lazy-yield only, §7)"
    );
}

/// Snapshot pinning (§5) for the deferred cursor: it captures its snapshot at `query()` time and runs
/// the set op on the first pull over THAT snapshot, so a concurrent writer's rows never appear; the
/// watermark holds at the cursor's version until it is closed.
#[test]
fn deferred_cursor_pins_its_snapshot_and_watermark() {
    let db = seeded(3); // version 1, ids 1..=3
    assert_eq!(db.oldest_live_txid(), 1);

    let mut reader = db.session(SessionOptions::default());
    // A top-level UNION → the deferred cursor. Pull one row (this runs the set op over the v1
    // snapshot), keep the cursor live.
    let mut rows = reader
        .query(
            "SELECT v FROM t WHERE id <= 2 UNION SELECT v FROM t WHERE id = 3 ORDER BY v",
            &[],
        )
        .unwrap();
    let first = (&mut rows).next().unwrap();
    assert_eq!(first, vec![Value::Int(10)]);
    assert_eq!(
        db.oldest_live_txid(),
        1,
        "open deferred cursor pins its version"
    );

    // A concurrent writer commits more rows (version 2) while the cursor is open.
    {
        let mut w = db.write_session();
        w.query_outcome("INSERT INTO t VALUES (4, 40), (5, 50)", &[])
            .unwrap();
        w.commit().unwrap();
    }
    assert_eq!(db.version(), 2);
    assert_eq!(
        db.oldest_live_txid(),
        1,
        "watermark held at the cursor's pin"
    );

    // Draining the rest sees ONLY the v1 snapshot (v = 20, 30) — not the writer's rows.
    let rest: Vec<Value> = (&mut rows).map(|r| r[0].clone()).collect();
    assert_eq!(
        rest,
        vec![Value::Int(20), Value::Int(30)],
        "frozen at open-time root"
    );
    rows.error().unwrap();

    rows.close();
    drop(rows);
    assert_eq!(
        db.oldest_live_txid(),
        2,
        "closed deferred cursor releases its pin"
    );
}

/// A mid-drain cost-ceiling abort (§6) for the deferred cursor: building the cursor does NOT run the
/// query (it is deferred to the first pull), so `query()` succeeds and the `54P01` surfaces during
/// iteration — exactly as for the streaming and buffered cursors.
#[test]
fn deferred_mid_drain_cost_abort_surfaces() {
    let db = seeded(1000);
    let mut s = db.session(SessionOptions::default());
    s.set_max_cost(50);
    // Building the deferred cursor must not throw (the set op runs on first pull).
    let mut rows = s
        .query(
            "SELECT id FROM t UNION ALL SELECT id FROM t ORDER BY id",
            &[],
        )
        .unwrap();
    let mut n = 0;
    for _ in &mut rows {
        n += 1;
        if n > 10_000 {
            panic!("the cost ceiling should have aborted the drain");
        }
    }
    let err = rows
        .error()
        .expect_err("a mid-drain cost abort must surface");
    assert_eq!(err.code(), "54P01", "the abort is a cost-limit error");
}

/// A data-modifying `WITH` (a write) must NOT take the deferred lazy path — it falls back to the
/// materialized `dispatch` (it takes the write gate and commits). Routed through `query()`, it still
/// returns the primary's `RETURNING` rows correctly.
#[test]
fn deferred_skips_data_modifying_with() {
    let db = seeded(5);
    let mut s = db.session(SessionOptions::default());
    // A writable CTE: INSERT … RETURNING fed to the primary. This is `stmt_is_write`, so it bypasses
    // try_deferred_query and runs through the write path — but `query()` still surfaces its rows.
    let mut rows = s
        .query(
            "WITH ins AS (INSERT INTO t VALUES (6, 60), (7, 70) RETURNING id) SELECT id FROM ins ORDER BY id",
            &[],
        )
        .unwrap();
    let out: Vec<Value> = (&mut rows).map(|r| r[0].clone()).collect();
    rows.error().unwrap();
    assert_eq!(out, vec![Value::Int(6), Value::Int(7)]);
    drop(rows);
    // The write committed: the rows are now visible.
    let (after, _) = eager(&mut s, "SELECT count(*) FROM t");
    assert_eq!(after, vec![vec![Value::Int(7)]]);
}

// ---- prepared-statement streaming (the prepared query path; streaming.md §7) ----------------------
//
// A prepared query (`prepare` + `query_prepared`) routes its parsed AST through the SAME lazy lanes
// as the ad-hoc `query()` — so a prepared `SELECT` streams (single-table pull / blocking-buffer /
// deferred set-op), pins its snapshot in the watermark, and offers the early-exit win, all identical
// to a one-shot query. The prepared `$N`-bound variant is exercised alongside the parameterless one.

/// A fully-drained prepared query yields the IDENTICAL rows + total cost as the ad-hoc `query()`
/// (and thus `execute()`, §6), across every lane — streaming, buffered, and deferred.
#[test]
fn prepared_query_matches_streamed() {
    let db = seeded(100);
    let mut s = db.session(SessionOptions::default());
    for sql in [
        "SELECT id, v FROM t LIMIT 5", // streaming (LIMIT short-circuit)
        "SELECT id, v FROM t ORDER BY id LIMIT 7", // streaming (PK-ordered)
        "SELECT v FROM t ORDER BY v LIMIT 6", // buffered (non-PK sort, top-N)
        "SELECT count(*) FROM t",      // buffered (aggregate)
        "SELECT DISTINCT v FROM t ORDER BY v", // buffered (DISTINCT + sort)
        "SELECT v FROM t WHERE id <= 3 UNION SELECT v FROM t WHERE id >= 98 ORDER BY v", // deferred (set op)
        "WITH x AS (SELECT id, v FROM t WHERE v > 500) SELECT id, v FROM x ORDER BY id", // deferred (WITH)
    ] {
        let (er, ec) = streamed(&mut s, sql);
        let stmt = s.prepare(sql).unwrap();
        let mut rows = s.query_prepared(&stmt, &[]).unwrap();
        let mut pr = Vec::new();
        for r in &mut rows {
            pr.push(r);
        }
        rows.error().unwrap();
        let pc = rows.cost();
        assert_eq!(pr, er, "prepared rows mismatch: {sql}");
        assert_eq!(pc, ec, "prepared cost mismatch: {sql}");
    }
}

/// A prepared query binds `$N` params and streams: the bound prepared run matches the ad-hoc bound
/// `query()` on rows + cost.
#[test]
fn prepared_query_binds_params_and_streams() {
    let db = seeded(100);
    let mut s = db.session(SessionOptions::default());
    let sql = "SELECT id, v FROM t WHERE id >= $1 ORDER BY id LIMIT 4";

    let mut ad_hoc = s.query(sql, &[Value::Int(90)]).unwrap();
    let ar: Vec<Vec<Value>> = (&mut ad_hoc).collect();
    ad_hoc.error().unwrap();
    let ac = ad_hoc.cost();

    let stmt = s.prepare(sql).unwrap();
    let mut rows = s.query_prepared(&stmt, &[Value::Int(90)]).unwrap();
    let pr: Vec<Vec<Value>> = (&mut rows).collect();
    rows.error().unwrap();
    let pc = rows.cost();

    assert_eq!(
        pr,
        vec![
            vec![Value::Int(90), Value::Int(900)],
            vec![Value::Int(91), Value::Int(910)],
            vec![Value::Int(92), Value::Int(920)],
            vec![Value::Int(93), Value::Int(930)],
        ]
    );
    assert_eq!(pr, ar, "prepared bound rows match ad-hoc");
    assert_eq!(pc, ac, "prepared bound cost matches ad-hoc");
    // A prepared statement is reusable: a second run with a different param re-streams.
    let mut rows2 = s.query_prepared(&stmt, &[Value::Int(1)]).unwrap();
    let pr2: Vec<Vec<Value>> = (&mut rows2).take(2).collect();
    drop(rows2);
    assert_eq!(
        pr2,
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(20)]
        ]
    );
}

/// Early exit (§6) on the prepared path: pulling only a prefix charges LESS than a full drain — the
/// streaming win now reaches prepared queries, not only ad-hoc ones.
#[test]
fn prepared_query_early_exit_charges_less() {
    let db = seeded(1000);
    let mut s = db.session(SessionOptions::default());
    let stmt = s.prepare("SELECT id FROM t ORDER BY id").unwrap();

    let mut full = s.query_prepared(&stmt, &[]).unwrap();
    let full_rows: Vec<Vec<Value>> = (&mut full).collect();
    full.error().unwrap();
    let full_cost = full.cost();
    assert_eq!(full_rows.len(), 1000);

    let mut rows = s.query_prepared(&stmt, &[]).unwrap();
    let prefix: Vec<Value> = (&mut rows).take(3).map(|r| r[0].clone()).collect();
    let partial_cost = rows.cost();
    drop(rows);

    assert_eq!(prefix, vec![Value::Int(1), Value::Int(2), Value::Int(3)]);
    assert!(
        partial_cost < full_cost,
        "prepared early exit must charge less (partial={partial_cost}, full={full_cost})"
    );
}

/// Snapshot pinning (§5) on the prepared path: an open prepared cursor pins its version in the
/// watermark, sees only its open-time snapshot as a writer commits, and releases on close.
#[test]
fn prepared_query_pins_its_snapshot_and_watermark() {
    let db = seeded(3);
    assert_eq!(db.oldest_live_txid(), 1);

    let mut reader = db.session(SessionOptions::default());
    let stmt = reader.prepare("SELECT id FROM t ORDER BY id").unwrap();
    let mut rows = reader.query_prepared(&stmt, &[]).unwrap();
    let first = (&mut rows).next().unwrap();
    assert_eq!(first, vec![Value::Int(1)]);
    assert_eq!(db.oldest_live_txid(), 1, "open prepared cursor pins v1");

    {
        let mut w = db.write_session();
        w.query_outcome("INSERT INTO t VALUES (4, 40), (5, 50)", &[])
            .unwrap();
        w.commit().unwrap();
    }
    assert_eq!(db.oldest_live_txid(), 1, "watermark held at the pin");

    let rest: Vec<Value> = (&mut rows).map(|r| r[0].clone()).collect();
    assert_eq!(rest, vec![Value::Int(2), Value::Int(3)], "frozen at open");
    rows.error().unwrap();
    rows.close();
    drop(rows);
    assert_eq!(db.oldest_live_txid(), 2, "closed cursor releases its pin");
}

/// A mid-drain cost abort (§6) on the prepared path: the `54P01` surfaces during iteration, not at
/// `query_prepared()` — the prepared cursor defers its work like the ad-hoc one.
#[test]
fn prepared_query_mid_drain_cost_abort_surfaces() {
    let db = seeded(1000);
    let mut s = db.session(SessionOptions::default());
    s.set_max_cost(50);
    let stmt = s.prepare("SELECT id FROM t ORDER BY id").unwrap();
    // Building the cursor is fine; the per-row meter guard aborts during the drain.
    let mut rows = s.query_prepared(&stmt, &[]).unwrap();
    let mut n = 0;
    for _ in &mut rows {
        n += 1;
        if n > 10_000 {
            panic!("the cost ceiling should have aborted the drain");
        }
    }
    let err = rows
        .error()
        .expect_err("a mid-drain cost abort must surface");
    assert_eq!(err.code(), "54P01");
}

/// The bare `Database::query_prepared` convenience streams too (it mints a transient session; the
/// cursor owns its snapshot, so it is not stranded).
#[test]
fn database_query_prepared_convenience_streams() {
    let mut db = seeded(50);
    let stmt = db
        .prepare("SELECT id, v FROM t ORDER BY id LIMIT 4")
        .unwrap();
    let mut rows = db.query_prepared(&stmt, &[]).unwrap();
    let out: Vec<Vec<Value>> = (&mut rows).collect();
    rows.error().unwrap();
    assert_eq!(
        out,
        vec![
            vec![Value::Int(1), Value::Int(10)],
            vec![Value::Int(2), Value::Int(20)],
            vec![Value::Int(3), Value::Int(30)],
            vec![Value::Int(4), Value::Int(40)],
        ]
    );
}
