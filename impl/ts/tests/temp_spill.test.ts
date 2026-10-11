// Temp-table spill-to-disk (spec/design/temp-tables.md §6, slice 3): once a session-local temp domain's
// page bytes outgrow tempBuffers on a host with a spill sink, the domain migrates from its in-RAM store to
// a scratch file and its page cache is bounded by tempBuffers. These per-core tests reach what the
// corpus cannot (the corpus runs in-memory databases, which have no spill target): the residency flip
// itself, the bounded pool, the unlinked scratch file, and the results/cost/54P03 neutrality of spilling.
// Mirrors Rust temp_spill_internal_tests (shared.rs) and Go temp_spill_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  type Database,
  type Engine,
  EngineError,
  queryOutcome,
  type Session,
  setSpillDirForTest,
  type Value,
} from "../src/tooling.ts";
import { memDb } from "./mem_db.ts";

const WORKLOAD = [
  "CREATE TEMP TABLE s (id i32 PRIMARY KEY, grp i32, pad text)",
  "CREATE INDEX s_grp ON s (grp)",
  "INSERT INTO s SELECT g, g % 7, repeat('x', 40 + g % 13) FROM generate_series(1, 400) AS g",
  "UPDATE s SET pad = repeat('y', 3 + id % 9) WHERE id % 5 = 0",
  "DELETE FROM s WHERE id % 11 = 0",
  "INSERT INTO s SELECT g, g % 7, repeat('z', g % 17) FROM generate_series(401, 600) AS g",
];

const QUERIES = [
  "SELECT count(*), sum(id) FROM s",
  "SELECT id, pad FROM s WHERE id = 250",
  "SELECT id FROM s WHERE grp = 3 ORDER BY id",
  "SELECT pad FROM s ORDER BY id DESC LIMIT 5",
  "SELECT grp, count(*) FROM s GROUP BY grp ORDER BY grp",
];

// tempStorageOf reaches the session's private temp domain (Rust's sess.engine.temp_storage) — a
// white-box cast, since the domain is an internal storage engine with no public surface.
function tempStorageOf(sess: Session): Engine {
  const ts = (sess as unknown as { engine: Engine }).engine.tempStorage;
  if (ts === null) throw new Error("the session has no temp domain");
  return ts;
}

function spillDb(dir: string): Database {
  const db = memDb(256);
  setSpillDirForTest(db, dir);
  return db;
}

function scratchDir(tag: string): string {
  return mkdtempSync(join(tmpdir(), `jed-temp-spill-${tag}-`));
}

// run runs the workload + queries under tempBuffers, returning each query's rows and every statement's
// cost, plus whether the domain spilled. The caller closes the session.
function run(
  db: Database,
  tempBuffers: number,
): { out: { rows: Value[][]; cost: bigint }[]; spilled: boolean; sess: Session } {
  const sess = db.session({ tempBuffers, maxTempStorageBytes: 0 });
  const out: { rows: Value[][]; cost: bigint }[] = [];
  for (const sql of WORKLOAD) {
    out.push({ rows: [], cost: queryOutcome(sess, sql).cost });
  }
  for (const sql of QUERIES) {
    const o = queryOutcome(sess, sql);
    if (o.kind !== "query") throw new Error(`expected a query: ${sql}`);
    out.push({ rows: o.rows, cost: o.cost });
  }
  return { out, spilled: tempStorageOf(sess).spilled, sess };
}

test("a spilled temp domain matches the resident one's rows and cost", () => {
  const dir = scratchDir("parity");
  try {
    const resident = run(spillDb(dir), 0);
    assert.ok(!resident.spilled, "tempBuffers = 0 must never spill");
    resident.sess.close();

    const spilled = run(spillDb(dir), 4 * 256);
    assert.ok(spilled.spilled, "a temp domain past tempBuffers should spill");
    assert.deepStrictEqual(spilled.out, resident.out, "spilling changed rows or cost");

    // The page cache is bounded by tempBuffers (4 leaves at page 256), not pinned.
    const paging = tempStorageOf(spilled.sess).paging!;
    assert.equal(paging.capacity(), 4);
    assert.ok(paging.residentLeaves() <= 4);
    spilled.sess.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a temp domain without a spill target stays resident", () => {
  // An in-memory database has no spill sink, so temp stays resident past tempBuffers.
  const r = run(memDb(256), 4 * 256);
  assert.ok(!r.spilled);
  assert.equal(r.out.length, WORKLOAD.length + QUERIES.length);
  r.sess.close();
});

test("the scratch file is unlinked while open and released at session close", () => {
  const dir = scratchDir("unlink");
  try {
    const r = run(spillDb(dir), 4 * 256);
    assert.ok(r.spilled);
    if (process.platform !== "win32") {
      assert.deepStrictEqual(
        readdirSync(dir),
        [],
        "the scratch file should be unlinked while open",
      );
    }
    r.sess.close();
    assert.deepStrictEqual(readdirSync(dir), []);
    assert.equal(
      (r.sess as unknown as { engine: Engine }).engine.tempStorage,
      null,
      "session close drops the temp domain",
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("spilling does not move the 54P03 point", () => {
  const dir = scratchDir("ceiling");
  try {
    const abortAt = (tempBuffers: number): { at: number; spilled: boolean } => {
      const db = spillDb(dir);
      const sess = db.session({ tempBuffers, maxTempStorageBytes: 40 * 256 });
      try {
        queryOutcome(sess, "CREATE TEMP TABLE lt (id i32 PRIMARY KEY, pad text)");
        const pad = "z".repeat(40);
        for (let i = 1; i <= 1000; i++) {
          try {
            queryOutcome(sess, `INSERT INTO lt VALUES (${i}, 'r-${pad}')`);
          } catch (e) {
            if (!(e instanceof EngineError) || e.code() !== "54P03") throw e;
            return { at: i, spilled: tempStorageOf(sess).spilled };
          }
        }
        throw new Error("never hit 54P03");
      } finally {
        sess.close();
      }
    };
    const resident = abortAt(0);
    const spilled = abortAt(2 * 256);
    assert.ok(!resident.spilled);
    assert.ok(spilled.spilled);
    assert.equal(spilled.at, resident.at);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("the pool capacity follows a later tempBuffers change", () => {
  const dir = scratchDir("retune");
  try {
    const r = run(spillDb(dir), 4 * 256);
    assert.ok(r.spilled);
    r.sess.setTempBuffers(16 * 256);
    queryOutcome(r.sess, "INSERT INTO s VALUES (100000, 1, 'x')");
    assert.equal(tempStorageOf(r.sess).paging!.capacity(), 16);
    r.sess.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
