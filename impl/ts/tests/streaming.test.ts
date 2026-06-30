// S3: the lazy STREAMING result cursor (spec/design/streaming.md §3/§4/§5/§6). The conformance corpus
// drives the materialized execute() path, so streaming — which only affects query() → Rows — is
// internal machinery the corpus cannot reach (CLAUDE.md §10). These per-core tests pin the contract: a
// fully-drained streaming query yields the IDENTICAL rows + total cost as the eager path (§6); a caller
// that stops early reads (and charges) less (the early-exit win, §6); the cursor pins its snapshot for
// its life (§5); and a mid-drain error surfaces (§6).

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
