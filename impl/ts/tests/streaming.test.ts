// S3/S4: the lazy result cursor (spec/design/streaming.md §3/§4/§5/§6). The conformance corpus drives
// the materialized execute() path, so the lazy cursor — which only affects query() → Rows — is internal
// machinery the corpus cannot reach (CLAUDE.md §10). These per-core tests pin the contract: a
// fully-drained query yields the IDENTICAL rows + total cost as the eager path (§6); a caller that stops
// early reads (and charges) less (the early-exit win, §6); the cursor pins its snapshot for its life
// (§5); and a mid-drain error surfaces (§6).
//
// The first group covers the S3 Streaming cursor (single-table no-blocking-operator scan); the second
// (suffixed "buffered") covers the S4 Buffered cursor — a blocking plan (non-PK ORDER BY, DISTINCT,
// aggregate, window, join) whose input buffers but whose OUTPUT is yielded one row at a time.

import assert from "node:assert/strict";
import { mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { createDatabase, type Database, EngineError, type Session } from "../src/lib.ts";
import { setAfterPersistHook } from "../src/shared.ts";
import {
  Engine,
  execute,
  intValue,
  prepare,
  query,
  queryOutcome,
  queryPrepared,
} from "../src/tooling.ts";
import type { Value } from "../src/value.ts";
import { memDb } from "./mem_db.ts";

// seededKV builds an in-memory shared db with t(id i32 PK, v i32) holding 1..=n (v = id * 10).
function seededKV(n: number): Database {
  const db = memDb();
  const w = db.writeSession();
  w.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  for (let i = 1; i <= n; i++) w.execute(`INSERT INTO t VALUES (${i}, ${i * 10})`);
  w.commit();
  return db;
}

// eagerResult: the materialized (execute) rows + total cost — the oracle the streaming cursor matches.
function eagerResult(s: Session, sql: string): { rows: Value[][]; cost: bigint } {
  const out = queryOutcome(s, sql);
  assert.equal(out.kind, "query", `not a query: ${sql}`);
  if (out.kind !== "query") throw new Error("unreachable");
  return { rows: out.rows, cost: out.cost };
}

// streamResult: the streaming (query) rows, fully drained, + final cost.
function streamResult(s: Session, sql: string): { rows: Value[][]; cost: bigint } {
  const cursor = s.query(sql);
  const rows: Value[][] = [];
  for (const r of cursor) rows.push(r);
  const cost = cursor.cost;
  cursor.close();
  return { rows, cost };
}

// Every streamable shape: query() (lazy) must equal execute() (eager) on rows AND total cost.
test("streaming matches eager rows and cost", () => {
  const db = seededKV(100);
  const s = db.session();
  try {
    for (const sql of [
      "SELECT id, v FROM t LIMIT 5",
      "SELECT id, v FROM t LIMIT 5 OFFSET 10",
      "SELECT id, v FROM t ORDER BY id",
      "SELECT id, v FROM t ORDER BY id LIMIT 7",
      "SELECT id, v FROM t ORDER BY id DESC LIMIT 7",
      "SELECT id, v FROM t WHERE v > 500 ORDER BY id",
      "SELECT id FROM t WHERE id >= 90 ORDER BY id",
      "SELECT v FROM t ORDER BY id LIMIT 3",
      "SELECT id, v + 1 FROM t ORDER BY id LIMIT 4",
      "SELECT id FROM t WHERE id = 9999", // empty
    ]) {
      const eager = eagerResult(s, sql);
      const stream = streamResult(s, sql);
      assert.deepEqual(stream.rows, eager.rows, `rows mismatch: ${sql}`);
      assert.equal(stream.cost, eager.cost, `cost mismatch: ${sql}`);
    }
  } finally {
    s.close();
  }
});

// A non-streamable shape still works through query() — it falls back to the buffered cursor.
test("non-streamable falls back and matches", () => {
  const db = seededKV(20);
  const s = db.session();
  try {
    for (const sql of [
      "SELECT count(*) FROM t",
      "SELECT v FROM t ORDER BY v",
      "SELECT DISTINCT v FROM t",
      "SELECT a.id FROM t a JOIN t b USING (id)",
    ]) {
      const eager = eagerResult(s, sql);
      const stream = streamResult(s, sql);
      assert.deepEqual(stream.rows, eager.rows, `rows mismatch: ${sql}`);
      assert.equal(stream.cost, eager.cost, `cost mismatch: ${sql}`);
    }
  } finally {
    s.close();
  }
});

// Early exit (§6): pulling only a prefix does LESS work than draining — fewer storageRowRead charges.
test("streaming early exit charges less", () => {
  const db = seededKV(1000);
  const s = db.session();
  try {
    const full = streamResult(s, "SELECT id FROM t ORDER BY id");
    assert.equal(full.rows.length, 1000);

    const cursor = s.query("SELECT id FROM t ORDER BY id");
    const prefix: bigint[] = [];
    for (const r of cursor) {
      const v = r[0]!;
      if (v.kind === "int") prefix.push(v.int);
      if (prefix.length === 3) break; // break runs the cursor's return path — no further faulting
    }
    const partial = cursor.cost;
    cursor.close();

    assert.deepEqual(prefix, [1n, 2n, 3n], "early pull yields the prefix");
    assert.ok(
      partial < full.cost,
      `early exit must charge less (partial=${partial}, full=${full.cost})`,
    );
  } finally {
    s.close();
  }
});

// Snapshot pinning (§5): a streaming cursor reads the snapshot it opened on even as a concurrent writer
// commits, and the watermark holds at its version until it is closed.
test("streaming cursor pins its snapshot and watermark", () => {
  const db = seededKV(3); // version 1, ids 1..=3
  assert.equal(db.version, 1n);
  assert.equal(db.oldestLiveTxid(), 1n);

  const reader = db.session();
  try {
    const cursor = reader.query("SELECT id FROM t ORDER BY id");
    const it = cursor[Symbol.iterator]();
    const first = it.next();
    assert.equal(first.done, false);
    assert.equal(first.value[0]!.kind === "int" ? first.value[0]!.int : -1n, 1n);
    assert.equal(db.oldestLiveTxid(), 1n, "open cursor pins its version");

    // A concurrent writer commits two more rows (version 2) while the cursor is open.
    const w = db.writeSession();
    w.execute("INSERT INTO t VALUES (4, 40), (5, 50)");
    w.commit();
    assert.equal(db.version, 2n);
    assert.equal(db.oldestLiveTxid(), 1n, "watermark held at the cursor's pin");

    // Draining the rest sees ONLY the v1 snapshot (ids 2, 3) — not the writer's rows.
    const rest: bigint[] = [];
    for (let r = it.next(); !r.done; r = it.next()) {
      const v = r.value[0]!;
      if (v.kind === "int") rest.push(v.int);
    }
    assert.deepEqual(rest, [2n, 3n], "frozen at open-time root");

    // Closing the cursor releases the pin; the watermark advances.
    cursor.close();
    assert.equal(db.oldestLiveTxid(), 2n, "closed cursor releases its pin");

    const fresh = streamResult(reader, "SELECT id FROM t ORDER BY id");
    assert.equal(fresh.rows.length, 5, "a fresh read sees the writer's rows");
  } finally {
    reader.close();
  }
});

// A mid-drain cost-ceiling abort (§6): the 54P01 throws during iteration, not at query() time.
test("streaming mid-drain cost abort throws during iteration", () => {
  const db = seededKV(1000);
  const s = db.session({ maxCost: 50n });
  try {
    const cursor = s.query("SELECT id FROM t ORDER BY id"); // building the cursor must not throw
    let caught: unknown;
    try {
      let n = 0;
      for (const _ of cursor) {
        if (++n > 10000) throw new Error("the cost ceiling should have aborted the drain");
      }
    } catch (e) {
      caught = e;
    }
    cursor.close();
    assert.ok(caught instanceof EngineError, "a mid-drain cost abort must throw");
    assert.equal((caught as EngineError).code(), "54P01", "the abort is a cost-limit error");
  } finally {
    s.close();
  }
});

// The bare Database.query convenience streams too: the transient mint-a-session does not strand the
// cursor (it owns its snapshot).
test("database query convenience streams", () => {
  const db = seededKV(50);
  const cursor = db.query("SELECT id, v FROM t ORDER BY id LIMIT 4");
  const got: bigint[][] = [];
  for (const r of cursor) {
    const a = r[0]!;
    const b = r[1]!;
    got.push([a.kind === "int" ? a.int : -1n, b.kind === "int" ? b.int : -1n]);
  }
  cursor.close();
  assert.deepEqual(got, [
    [1n, 10n],
    [2n, 20n],
    [3n, 30n],
    [4n, 40n],
  ]);
});

// The bare-handle db.query() path pins the reader-liveness watermark exactly like a session query
// (streaming.md §7 closing note): the fresh per-call session's provisional pin transfers to the Rows,
// so a held bare-handle cursor holds oldestLiveTxid, keeps within-session reclamation (v25) from
// recycling its snapshot's pages under compacting commit churn, and releases the watermark on close.
// File-backed with a tiny page size so the churn actually orphans pages (an in-memory db cannot
// exercise the persisted-free-list reuse path). The afterPersistHook watches the free-list generation:
// it must never pass the pin while the cursor is open (reclamation deferred), and must pass it on the
// first commit after close (the churn produced real gated garbage — the test is not vacuous).
test("bare-handle query pins its watermark under reclamation", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-bare-"));
  const path = join(dir, "bare.jed");
  try {
    const db = createDatabase({ path, pageSize: 256, skipFsync: true });
    try {
      db.execute("CREATE TABLE t (id i64 PRIMARY KEY, v i64)");
      // A multi-leaf tree; every churn commit below rewrites all of it (whole-table UPDATE),
      // orphaning the prior tree so unpinned compaction would reclaim + reuse its pages.
      for (let i = 1; i <= 120; i++) db.execute(`INSERT INTO t VALUES (${i}, ${i * 10})`);
      const v0 = db.version;
      assert.equal(db.oldestLiveTxid(), v0, "idle watermark = committed");

      // Open a bare-handle streaming cursor (the transient session closes before query() returns;
      // the pin rides the Rows) and pull ONE row so the scan is live mid-tree.
      const cursor = db.query("SELECT id, v FROM t ORDER BY id");
      const it = cursor[Symbol.iterator]();
      const first = it.next();
      assert.equal(first.done, false);
      const f0 = first.value[0]!;
      const f1 = first.value[1]!;
      assert.equal(f0.kind === "int" ? f0.int : -1n, 1n);
      assert.equal(f1.kind === "int" ? f1.int : -1n, 10n);
      assert.equal(db.oldestLiveTxid(), v0, "open bare-handle cursor pins its version");

      // Churn: whole-table UPDATE commits through the same bare handle — each orphans every leaf
      // plus the spine, so on an ungated path reuse would recycle the cursor's pinned pages.
      let lastFreeGen = 0n;
      setAfterPersistHook((_committed, freeGen) => {
        lastFreeGen = freeGen;
      });
      for (let i = 0; i < 150; i++) db.execute("UPDATE t SET v = v + 1");
      assert.equal(db.oldestLiveTxid(), v0, "watermark held at the cursor's pin through the churn");
      assert.ok(
        lastFreeGen <= v0,
        `free-list generation ${lastFreeGen} advanced past the held pin ${v0} — reclamation ran under a live reader`,
      );

      // Drain: the cursor must see EXACTLY its frozen snapshot (v = id * 10), untouched by the churn.
      let want = 2n;
      for (let r = it.next(); !r.done; r = it.next()) {
        const id = r.value[0]!;
        const v = r.value[1]!;
        assert.equal(
          id.kind === "int" ? id.int : -1n,
          want,
          "SNAPSHOT ISOLATION VIOLATED: the cursor's pages were reclaimed and overwritten",
        );
        assert.equal(
          v.kind === "int" ? v.int : -1n,
          want * 10n,
          "SNAPSHOT ISOLATION VIOLATED: the cursor's pages were reclaimed and overwritten",
        );
        want++;
      }
      assert.equal(want, 121n, "drained the full pinned snapshot 2..=120");
      cursor.close();
      assert.equal(db.oldestLiveTxid(), db.version, "closed cursor releases its pin");

      // Self-validation (not vacuous): the first commit after the pin releases compacts the deferred
      // churn garbage — the generation advances past v0, proving the hold above was the gate at work.
      db.execute("UPDATE t SET v = v + 1");
      assert.ok(
        lastFreeGen > v0,
        `post-close commit did not compact (freeGenTxid=${lastFreeGen} <= ${v0}) — the churn never produced gated garbage`,
      );
    } finally {
      setAfterPersistHook(null);
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// ---- S4: the lazy BUFFERED cursor (a blocking plan; streaming.md §4) ------------------------------

// Every blocking shape (aggregate / non-PK ORDER BY / DISTINCT / window / join / GROUP BY): query() (the
// lazy buffered cursor) must equal execute() (eager) on rows AND total cost under full drain (§6). These
// all route through tryBufferedQuery → a bufferedRows generator, not the streaming fast lane.
test("buffered matches eager rows and cost", () => {
  const db = seededKV(40);
  const s = db.session();
  try {
    for (const sql of [
      "SELECT count(*) FROM t", // whole-table aggregate (final, 1 row)
      "SELECT sum(v), avg(v), min(id) FROM t", // multi-aggregate
      "SELECT v FROM t ORDER BY v", // ORDER BY the PK scan does NOT satisfy (final sort)
      "SELECT v FROM t ORDER BY v DESC LIMIT 6", // top-N over a non-PK sort
      "SELECT DISTINCT v FROM t ORDER BY v", // no-PK DISTINCT then sort (identity)
      "SELECT id, v + 1 FROM t GROUP BY id, v ORDER BY id", // GROUP BY + projection expr (project)
      "SELECT id, v FROM t GROUP BY id, v HAVING v > 200 ORDER BY id", // HAVING
      "SELECT a.id, b.v FROM t a JOIN t b USING (id) ORDER BY a.id", // join + ORDER BY (project)
      "SELECT sum(v) OVER (ORDER BY id) FROM t ORDER BY id", // window function
    ]) {
      const eager = eagerResult(s, sql);
      const stream = streamResult(s, sql);
      assert.deepEqual(stream.rows, eager.rows, `rows mismatch: ${sql}`);
      assert.equal(stream.cost, eager.cost, `cost mismatch: ${sql}`);
    }
  } finally {
    s.close();
  }
});

// Early exit over a buffered cursor in project mode (§4): the blocking part (scan + group + sort) runs
// in full on the first pull, but a caller that stops after a prefix skips the PROJECTION of every row it
// never pulls — so it charges LESS than a full drain. The top-N-over-the-buffer win.
test("buffered early exit charges less", () => {
  const db = seededKV(1000);
  const s = db.session();
  try {
    const sql = "SELECT id, v + 1 FROM t GROUP BY id, v ORDER BY id";
    const full = streamResult(s, sql);
    assert.equal(full.rows.length, 1000);

    const cursor = s.query(sql);
    const prefix: bigint[][] = [];
    for (const r of cursor) {
      const a = r[0]!;
      const b = r[1]!;
      prefix.push([a.kind === "int" ? a.int : -1n, b.kind === "int" ? b.int : -1n]);
      if (prefix.length === 3) break;
    }
    const partial = cursor.cost;
    cursor.close();

    assert.deepEqual(prefix, [
      [1n, 11n],
      [2n, 21n],
      [3n, 31n],
    ]);
    assert.ok(
      partial < full.cost,
      `early exit over a buffered cursor must charge less (partial=${partial}, full=${full.cost})`,
    );
  } finally {
    s.close();
  }
});

// Snapshot pinning (§5) for the buffered cursor: it captures its snapshot at query() time (the blocking
// part materializes from THAT snapshot on first pull), so a concurrent writer's rows never appear; the
// watermark holds at the cursor's version until it is closed.
test("buffered cursor pins its snapshot and watermark", () => {
  const db = seededKV(3); // version 1, ids 1..=3
  assert.equal(db.oldestLiveTxid(), 1n);

  const reader = db.session();
  try {
    // A blocking query (ORDER BY v — not PK order) → the buffered cursor.
    const cursor = reader.query("SELECT v FROM t ORDER BY v");
    const it = cursor[Symbol.iterator]();
    const first = it.next();
    assert.equal(first.done, false);
    assert.equal(first.value[0]!.kind === "int" ? first.value[0]!.int : -1n, 10n);
    assert.equal(db.oldestLiveTxid(), 1n, "open buffered cursor pins its version");

    const w = db.writeSession();
    w.execute("INSERT INTO t VALUES (4, 40), (5, 50)");
    w.commit();
    assert.equal(db.version, 2n);
    assert.equal(db.oldestLiveTxid(), 1n, "watermark held at the cursor's pin");

    // Draining the rest sees ONLY the v1 snapshot (v = 20, 30) — not the writer's rows.
    const rest: bigint[] = [];
    for (let r = it.next(); !r.done; r = it.next()) {
      const v = r.value[0]!;
      if (v.kind === "int") rest.push(v.int);
    }
    assert.deepEqual(rest, [20n, 30n], "frozen at open-time root");

    cursor.close();
    assert.equal(db.oldestLiveTxid(), 2n, "closed buffered cursor releases its pin");
  } finally {
    reader.close();
  }
});

// A mid-drain cost-ceiling abort (§6) for the buffered cursor: building the cursor does NOT run the
// blocking part (deferred to the first pull), so query() succeeds and the 54P01 throws during iteration.
test("buffered mid-drain cost abort throws during iteration", () => {
  const db = seededKV(1000);
  const s = db.session({ maxCost: 50n });
  try {
    const cursor = s.query("SELECT v FROM t ORDER BY v"); // building the cursor must not throw
    let caught: unknown;
    try {
      let n = 0;
      for (const _ of cursor) {
        if (++n > 10000) throw new Error("the cost ceiling should have aborted the drain");
      }
    } catch (e) {
      caught = e;
    }
    cursor.close();
    assert.ok(caught instanceof EngineError, "a mid-drain cost abort must throw");
    assert.equal((caught as EngineError).code(), "54P01", "the abort is a cost-limit error");
  } finally {
    s.close();
  }
});

// ---- the lazy streaming-SORT output ("sorted" Emitter; streaming.md §4/§7) ------------------------

// Every streaming-external-sort shape (a single-table non-PK ORDER BY): query() (the lazy "sorted" drive
// — pulling the SortedRows iterator one row at a time) must equal execute() (the eager drive of the SAME
// emitter) on rows AND total cost under full drain (§6).
test("sorted output matches eager rows and cost", () => {
  const db = seededKV(40);
  const s = db.session();
  try {
    for (const sql of [
      "SELECT v FROM t ORDER BY v", // non-PK sort, full output
      "SELECT v FROM t ORDER BY v DESC", // descending
      "SELECT v FROM t ORDER BY v LIMIT 7", // top-N window
      "SELECT v FROM t ORDER BY v LIMIT 7 OFFSET 5", // LIMIT + OFFSET window
      "SELECT v FROM t ORDER BY v OFFSET 35", // OFFSET near the end (tail window)
      "SELECT id, v + 1 FROM t ORDER BY v", // a projection expression (operator_eval per row)
      "SELECT v FROM t WHERE id > 20 ORDER BY v", // a residual WHERE filter
      "SELECT v FROM t WHERE id > 99999 ORDER BY v", // empty result
    ]) {
      const eager = eagerResult(s, sql);
      const stream = streamResult(s, sql);
      assert.deepEqual(stream.rows, eager.rows, `rows mismatch: ${sql}`);
      assert.equal(stream.cost, eager.cost, `cost mismatch: ${sql}`);
    }
  } finally {
    s.close();
  }
});

// Early exit over the lazy streaming-sort output (§4/§7) — the headline win of this slice. The sort's
// INPUT is blocking (every row scanned + sorted on the first pull), but the OUTPUT is now yielded from
// the SortedRows iterator one row at a time, so a caller that stops after a prefix skips the rowProduced +
// projection of every windowed row it never pulls — charging LESS than a full drain. (Before this slice
// the sort output was a "final" Emitter, fully built + charged on the first pull, so an early exit charged
// the SAME — this test is what distinguishes the new behavior.)
test("sorted output early exit charges less", () => {
  const db = seededKV(1000);
  const s = db.session();
  try {
    const sql = "SELECT v FROM t ORDER BY v"; // non-PK ORDER BY, no LIMIT → a 1000-row lazy sorted output
    const full = streamResult(s, sql);
    assert.equal(full.rows.length, 1000);

    const cursor = s.query(sql);
    const prefix: bigint[] = [];
    for (const r of cursor) {
      const v = r[0]!;
      prefix.push(v.kind === "int" ? v.int : -1n);
      if (prefix.length === 3) break;
    }
    const partial = cursor.cost;
    cursor.close();

    assert.deepEqual(prefix, [10n, 20n, 30n], "early pull yields the sorted prefix");
    assert.ok(
      partial < full.cost,
      `early exit over the lazy sort output must charge less (partial=${partial}, full=${full.cost})`,
    );
  } finally {
    s.close();
  }
});

// The lazy streaming-sort output over the SPILLING merge path (SortedRows over a Merger): a file-backed
// database under a tiny workMem forces many spilled runs + a k-way merge. A full lazy drain must match the
// eager result (rows + cost — spill is invariant, spill.md §6), and an early exit must yield exactly the
// prefix while leaving NO spill temp file behind (the generator's finally — reached on early return —
// releases any undrained runs, §5).
test("sorted output spilling merge streams lazily", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-sorted-lazy-"));
  try {
    const db = createDatabase({ path: join(dir, "db.jed"), skipFsync: true });
    const w = db.writeSession();
    w.execute("CREATE TABLE t (id i32 PRIMARY KEY, k i32)");
    for (let id = 0; id < 200; id++) {
      const k = (id * 48271) % 100; // scrambled key with many duplicates
      w.execute(`INSERT INTO t VALUES (${id}, ${k})`);
    }
    w.commit();

    const sql = "SELECT id, k FROM t ORDER BY k, id";

    // Eager oracle: a default-workMem session never spills 200 small rows (in-memory sort).
    const oracle = db.session();
    const eager = eagerResult(oracle, sql);
    oracle.close();

    // Full lazy drain under a tiny workMem (forces spill + merge): rows + cost match the oracle.
    const s = db.session();
    s.setWorkMem(128); // ~2-3 rows per run → dozens of runs + a deep merge
    const stream = streamResult(s, sql);
    s.close();
    assert.deepEqual(stream.rows, eager.rows, "spilling lazy drain rows must match eager");
    assert.equal(stream.cost, eager.cost, "spilling lazy drain cost must match eager");
    assert.equal(
      readdirSync(dir).filter((n) => n.startsWith("jed-spill-")).length,
      0,
      "a full drain leaves no spill file",
    );

    // Early exit over the merge: pull a prefix, then close the cursor. The generator's finally releases
    // the undrained merge's run files, so none leak.
    const s2 = db.session();
    s2.setWorkMem(128);
    const cursor = s2.query(sql);
    const got: Value[][] = [];
    for (const r of cursor) {
      got.push(r);
      if (got.length === 5) break;
    }
    cursor.close();
    s2.close();
    assert.deepEqual(got, eager.rows.slice(0, 5), "early pull yields the sorted prefix");
    assert.equal(
      readdirSync(dir).filter((n) => n.startsWith("jed-spill-")).length,
      0,
      "an early exit leaves no spill file",
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// ---- the lazy DEFERRED cursor (a top-level set-op / WITH; streaming.md §7) ------------------------

// Every top-level set operation / pure-query WITH: query() (the lazy deferred cursor) must equal
// execute() (eager) on rows AND total cost under full drain (§6). These route through tryDeferredQuery,
// which reuses the eager runSetOp / runWith verbatim, so the rows + cost are identical by construction
// (the unordered shapes are deterministic here — same snapshot, same code path).
test("deferred set-op/WITH matches eager rows and cost", () => {
  const db = seededKV(20);
  const s = db.session();
  try {
    for (const sql of [
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
    ]) {
      const eager = eagerResult(s, sql);
      const stream = streamResult(s, sql);
      assert.deepEqual(stream.rows, eager.rows, `rows mismatch: ${sql}`);
      assert.equal(stream.cost, eager.cost, `cost mismatch: ${sql}`);
    }
  } finally {
    s.close();
  }
});

// The deferred cursor's defining trait (§7): a set-op / WITH has no per-row top-level projection to
// defer, so the WHOLE query runs on the FIRST pull — unlike S3/S4, an early exit charges the SAME as a
// full drain (the only win is lazy-yield, not early-exit). This pins that the cost after one pull is
// already final.
test("deferred set-op/WITH runs fully on first pull", () => {
  const db = seededKV(100);
  const s = db.session();
  try {
    const sql = "SELECT id FROM t UNION ALL SELECT id FROM t ORDER BY id";
    const full = streamResult(s, sql);
    assert.equal(full.rows.length, 200);

    const cursor = s.query(sql);
    const it = cursor[Symbol.iterator]();
    assert.equal(it.next().done, false, "expected at least one row");
    const afterOne = cursor.cost;
    cursor.close();
    assert.equal(
      afterOne,
      full.cost,
      "a deferred set-op/WITH accrues its full cost on the first pull (lazy-yield only, §7)",
    );
  } finally {
    s.close();
  }
});

// Snapshot pinning (§5) for the deferred cursor: it captures its snapshot at query() time and runs the
// set op on the first pull over THAT snapshot, so a concurrent writer's rows never appear; the watermark
// holds at the cursor's version until it is closed.
test("deferred cursor pins its snapshot and watermark", () => {
  const db = seededKV(3); // version 1, ids 1..=3
  assert.equal(db.oldestLiveTxid(), 1n);

  const reader = db.session();
  try {
    // A top-level UNION → the deferred cursor.
    const cursor = reader.query(
      "SELECT v FROM t WHERE id <= 2 UNION SELECT v FROM t WHERE id = 3 ORDER BY v",
    );
    const it = cursor[Symbol.iterator]();
    const first = it.next();
    assert.equal(first.done, false);
    assert.equal(first.value[0]!.kind === "int" ? first.value[0]!.int : -1n, 10n);
    assert.equal(db.oldestLiveTxid(), 1n, "open deferred cursor pins its version");

    const w = db.writeSession();
    w.execute("INSERT INTO t VALUES (4, 40), (5, 50)");
    w.commit();
    assert.equal(db.version, 2n);
    assert.equal(db.oldestLiveTxid(), 1n, "watermark held at the cursor's pin");

    // Draining the rest sees ONLY the v1 snapshot (v = 20, 30) — not the writer's rows.
    const rest: bigint[] = [];
    for (let r = it.next(); !r.done; r = it.next()) {
      const v = r.value[0]!;
      if (v.kind === "int") rest.push(v.int);
    }
    assert.deepEqual(rest, [20n, 30n], "frozen at open-time root");

    cursor.close();
    assert.equal(db.oldestLiveTxid(), 2n, "closed deferred cursor releases its pin");
  } finally {
    reader.close();
  }
});

// A mid-drain cost-ceiling abort (§6) for the deferred cursor: building the cursor does NOT run the
// query (deferred to the first pull), so query() succeeds and the 54P01 throws during iteration.
test("deferred mid-drain cost abort throws during iteration", () => {
  const db = seededKV(1000);
  const s = db.session({ maxCost: 50n });
  try {
    const cursor = s.query("SELECT id FROM t UNION ALL SELECT id FROM t ORDER BY id"); // build must not throw
    let caught: unknown;
    try {
      let n = 0;
      for (const _ of cursor) {
        if (++n > 10000) throw new Error("the cost ceiling should have aborted the drain");
      }
    } catch (e) {
      caught = e;
    }
    cursor.close();
    assert.ok(caught instanceof EngineError, "a mid-drain cost abort must throw");
    assert.equal((caught as EngineError).code(), "54P01", "the abort is a cost-limit error");
  } finally {
    s.close();
  }
});

// A data-modifying WITH (a write) must NOT take the deferred lazy path — it falls back to the
// materialized dispatch (it takes the write gate and commits). Routed through query(), it still returns
// the primary's RETURNING rows correctly.
test("deferred path skips a data-modifying WITH", () => {
  const db = seededKV(5);
  const s = db.session();
  try {
    // A writable CTE: INSERT … RETURNING fed to the primary. This is stmtIsWrite, so it bypasses
    // tryDeferredQuery and runs through the write path — but query() still surfaces its rows.
    const cursor = s.query(
      "WITH ins AS (INSERT INTO t VALUES (6, 60), (7, 70) RETURNING id) SELECT id FROM ins ORDER BY id",
    );
    const got: bigint[] = [];
    for (const r of cursor) {
      const v = r[0]!;
      if (v.kind === "int") got.push(v.int);
    }
    cursor.close();
    assert.deepEqual(got, [6n, 7n]);
    // The write committed: the rows are now visible.
    const after = eagerResult(s, "SELECT count(*) FROM t");
    assert.equal(after.rows[0]![0]!.kind === "int" ? after.rows[0]![0]!.int : -1n, 7n);
  } finally {
    s.close();
  }
});

// ---- prepared-statement streaming (the low-level PreparedStatement; streaming.md §7) --------------
//
// A prepared query (prepare(db, sql) + stmt.query(params)) routes its parsed AST through the SAME lazy
// lanes as the ad-hoc query(db, sql, params) — so a prepared SELECT streams (single-table pull /
// blocking-buffer / deferred set-op) and offers the early-exit win, identical to a one-shot query. The
// low-level PreparedStatement binds to a bare Engine (the watermark pin lives on the shared-core
// Session path — the bare Engine pins nothing, like the ad-hoc query(db,…) free function).

// seededEngine builds a bare in-memory Engine with t(id i32 PK, v i32) holding 1..=n (v = id * 10).
function seededEngine(n: number): Engine {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  for (let i = 1; i <= n; i++) execute(db, `INSERT INTO t VALUES (${i}, ${i * 10})`);
  return db;
}

// drainPrepared: a prepared query's rows, fully drained, + final cost.
function drainPrepared(
  db: Engine,
  sql: string,
  params: Value[] = [],
): { rows: Value[][]; cost: bigint } {
  const cursor = queryPrepared(db, prepare(db, sql), params);
  const rows: Value[][] = [];
  for (const r of cursor) rows.push(r);
  const cost = cursor.cost;
  cursor.close();
  return { rows, cost };
}

// A fully-drained prepared query yields the IDENTICAL rows + total cost as the materialized executeStmtParams
// (the eager oracle, §6), across every lane — streaming, buffered, and deferred.
test("prepared query matches eager rows and cost", () => {
  const db = seededEngine(100);
  for (const sql of [
    "SELECT id, v FROM t LIMIT 5", // streaming (LIMIT short-circuit)
    "SELECT id, v FROM t ORDER BY id LIMIT 7", // streaming (PK-ordered)
    "SELECT v FROM t ORDER BY v LIMIT 6", // buffered (non-PK sort, top-N)
    "SELECT count(*) FROM t", // buffered (aggregate)
    "SELECT DISTINCT v FROM t ORDER BY v", // buffered (DISTINCT + sort)
    "SELECT v FROM t WHERE id <= 3 UNION SELECT v FROM t WHERE id >= 98 ORDER BY v", // deferred (set op)
    "WITH x AS (SELECT id, v FROM t WHERE v > 500) SELECT id, v FROM x ORDER BY id", // deferred (WITH)
  ]) {
    const eager = db.executeStmtParams(db.parse(sql), []);
    assert.equal(eager.kind, "query", `not a query: ${sql}`);
    if (eager.kind !== "query") throw new Error("unreachable");
    const prepared = drainPrepared(db, sql);
    assert.deepEqual(prepared.rows, eager.rows, `prepared rows mismatch: ${sql}`);
    assert.equal(prepared.cost, eager.cost, `prepared cost mismatch: ${sql}`);
  }
});

// A prepared query binds $N params and streams: the bound prepared run matches the ad-hoc bound query()
// on rows + cost, and the statement is reusable across runs with different params.
test("prepared query binds params and streams", () => {
  const db = seededEngine(100);
  const sql = "SELECT id, v FROM t WHERE id >= $1 ORDER BY id LIMIT 4";

  const adHocCursor = query(db, sql, [intValue(90n)]);
  const adHoc: Value[][] = [];
  for (const r of adHocCursor) adHoc.push(r);
  const adHocCost = adHocCursor.cost;
  adHocCursor.close();

  const prepared = drainPrepared(db, sql, [intValue(90n)]);
  const want = [
    [90n, 900n],
    [91n, 910n],
    [92n, 920n],
    [93n, 930n],
  ];
  assert.deepEqual(
    prepared.rows.map((r) => r.map((v) => (v.kind === "int" ? v.int : -1n))),
    want,
  );
  assert.deepEqual(prepared.rows, adHoc, "prepared bound rows match ad-hoc");
  assert.equal(prepared.cost, adHocCost, "prepared bound cost matches ad-hoc");
  // Reusable: a second run with a different param re-streams.
  const reused = drainPrepared(db, sql, [intValue(1n)]);
  assert.equal(reused.rows[0]![0]!.kind === "int" ? reused.rows[0]![0]!.int : -1n, 1n);
});

// Early exit (§6) on the prepared path: pulling only a prefix charges LESS than a full drain — the
// streaming win now reaches prepared queries.
test("prepared query early exit charges less", () => {
  const db = seededEngine(1000);
  const full = drainPrepared(db, "SELECT id FROM t ORDER BY id");
  assert.equal(full.rows.length, 1000);

  const cursor = queryPrepared(db, prepare(db, "SELECT id FROM t ORDER BY id"));
  const prefix: bigint[] = [];
  for (const r of cursor) {
    const v = r[0]!;
    if (v.kind === "int") prefix.push(v.int);
    if (prefix.length === 3) break;
  }
  const partial = cursor.cost;
  cursor.close();

  assert.deepEqual(prefix, [1n, 2n, 3n]);
  assert.ok(
    partial < full.cost,
    `prepared early exit must charge less (partial=${partial}, full=${full.cost})`,
  );
});

// A mid-drain cost abort (§6) on the prepared path: the 54P01 throws during iteration, not at
// stmt.query() — the prepared cursor defers its work like the ad-hoc one.
test("prepared query mid-drain cost abort surfaces", () => {
  const db = seededEngine(1000);
  db.setMaxCost(50n);
  // Building the cursor is fine; the per-row meter guard aborts during the drain.
  const cursor = queryPrepared(db, prepare(db, "SELECT id FROM t ORDER BY id"));
  let code = "";
  try {
    let n = 0;
    for (const _ of cursor) {
      if (++n > 10000) throw new Error("the cost ceiling should have aborted the drain");
    }
  } catch (e) {
    if (e instanceof EngineError) code = e.code();
    else throw e;
  }
  cursor.close();
  assert.equal(code, "54P01", "a mid-drain cost abort must surface");
});
