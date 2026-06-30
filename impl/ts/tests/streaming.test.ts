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
import { test } from "node:test";
import { Database, EngineError, type Session } from "../src/lib.ts";
import type { Value } from "../src/value.ts";

// seededKV builds an in-memory shared db with t(id i32 PK, v i32) holding 1..=n (v = id * 10).
function seededKV(n: number): Database {
  const db = Database.newInMemory();
  const w = db.writeSession();
  w.execute("CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  for (let i = 1; i <= n; i++) w.execute(`INSERT INTO t VALUES (${i}, ${i * 10})`);
  w.commit();
  return db;
}

// eagerResult: the materialized (execute) rows + total cost — the oracle the streaming cursor matches.
function eagerResult(s: Session, sql: string): { rows: Value[][]; cost: bigint } {
  const out = s.execute(sql);
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
