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
use jed::{Database, Outcome, Session, SessionOptions};

/// Seed an in-memory shared db with `t(id i32 PK, v i32)` holding `1..=n` (v = id * 10).
fn seeded(n: i64) -> Database {
    let db = Database::new_in_memory();
    let mut w = db.write_session();
    w.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)", &[])
        .unwrap();
    for i in 1..=n {
        w.execute(&format!("INSERT INTO t VALUES ({i}, {})", i * 10), &[])
            .unwrap();
    }
    w.commit().unwrap();
    db
}

/// The materialized (`execute()`) result: rows + total cost — the oracle the streaming cursor must
/// match under full drain (§6).
fn eager(sess: &mut Session, sql: &str) -> (Vec<Vec<Value>>, i64) {
    match sess.execute(sql, &[]).unwrap() {
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
        w.execute("INSERT INTO t VALUES (4, 40), (5, 50)", &[])
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
        w.execute("INSERT INTO t VALUES (4, 40), (5, 50)", &[])
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
