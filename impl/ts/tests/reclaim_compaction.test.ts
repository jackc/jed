// Within-session free-list compaction (Phase A of routing temp stores through a MemoryBlockStore —
// spec/design/temp-tables.md §6, spec/design/bplus-reshape.md). A reclaim domain
// (Engine.reclaimWithinSession) rebuilds its free-list from the live reachable set at commit, so a
// never-reopened in-RAM store reuses its copy-on-write orphans instead of leaking a page per commit.
// These per-core tests cover what the corpus cannot: the internal high-water bound (~2× live) and the
// watermark gate (compaction defers while an older reader is pinned). The main domain leaves the flag
// off, so its reconstruct-on-open behavior (reclamation.test.ts) is unchanged — asserted here too.

import assert from "node:assert/strict";
import { test } from "node:test";
import { type Database, queryOutcome, type Session, type Value } from "../src/tooling.ts";
import { memDb } from "./mem_db.ts";

// setReclaim toggles within-session compaction on a Database's (single) main storage domain — a
// white-box reach into the private core (the analogue of the Go test's db.core.storage field). There is
// no public toggle: the main domain is reconstruct-on-open by default; only temp domains opt in
// internally. This exercises the same compaction path against the main in-memory domain in isolation.
function setReclaim(db: Database, on: boolean): void {
  (
    db as unknown as { core: { storage: { reclaimWithinSession: boolean } } }
  ).core.storage.reclaimWithinSession = on;
}

function rowsOf(sess: Session, sql: string): Value[][] {
  const o = queryOutcome(sess, sql);
  if (o.kind !== "query") throw new Error("expected a query");
  return o.rows;
}

function textAt(rows: Value[][], i: number): string {
  const v = rows[i]![0]!;
  if (v.kind !== "text") throw new Error("expected a text value");
  return v.text;
}

// churnInMemory builds a small multi-level tree in an in-memory database at page 256, then updates one
// row `rounds` times (each an autocommit copy-on-write commit that orphans its root→leaf path + the
// rewritten catalog). Returns the committed page high-water afterward. `reclaim` toggles within-session
// compaction on the (single) storage domain.
function churnInMemory(reclaim: boolean, rounds: number): { pageCount: number; db: Database } {
  const db = memDb(256);
  setReclaim(db, reclaim);
  const sess = db.session();
  sess.execute("CREATE TABLE t (id i32 PRIMARY KEY, pad text)");
  const base = "x".repeat(40);
  for (let i = 1; i <= 30; i++) {
    sess.execute(`INSERT INTO t VALUES (${i}, 'r${String(i).padStart(2, "0")}-${base}')`);
  }
  const pad = "y".repeat(40);
  for (let k = 0; k < rounds; k++) {
    sess.execute(`UPDATE t SET pad = 'a${k}-${pad}' WHERE id = 15`);
  }
  return { pageCount: db.pageCount, db };
}

test("within-session compaction bounds in-memory churn", () => {
  const rounds = 300;

  // Control: reclaim OFF is the pre-Phase-A behavior — a never-reopened in-memory store leaks a page per
  // commit, so the high-water grows roughly linearly with the churn count.
  const off = churnInMemory(false, rounds);
  const leaked = off.pageCount;
  off.db.close();
  assert.ok(
    leaked > rounds,
    `control (reclaim off) should leak ~1 page/commit; high-water only ${leaked} after ${rounds} rounds`,
  );

  // Reclaim ON: the high-water plateaus at ~2× the live page count (a few dozen pages), independent of
  // the churn count — bounded well under the leaked control.
  const on = churnInMemory(true, rounds);
  const bounded = on.pageCount;
  assert.ok(
    bounded <= 128,
    `reclaim on should bound the high-water at ~2×live; got ${bounded} (leaked control was ${leaked})`,
  );
  assert.ok(
    bounded * 4 <= leaked,
    `reclaim on (${bounded}) should be far below the leaked control (${leaked})`,
  );

  // The churned value and every row survive the reuse (a reclaimed page was dead, never a live one).
  const sess = on.db.session();
  const want = `a${rounds - 1}-${"y".repeat(40)}`;
  const got = rowsOf(sess, "SELECT pad FROM t WHERE id = 15");
  assert.equal(got.length, 1);
  assert.equal(textAt(got, 0), want);
  assert.equal(rowsOf(sess, "SELECT id FROM t").length, 30);
  on.db.close();
});

test("compaction defers while an older reader is pinned", () => {
  const db = memDb(256);
  setReclaim(db, true);
  const sess = db.session();
  sess.execute("CREATE TABLE t (id i32 PRIMARY KEY, pad text)");
  const base = "x".repeat(40);
  for (let i = 1; i <= 30; i++) {
    sess.execute(`INSERT INTO t VALUES (${i}, 'r${String(i).padStart(2, "0")}-${base}')`);
  }
  const pad = "y".repeat(40);

  // Pin an older version with an open read session: compaction must NOT free pages it may still observe,
  // so it defers and the high-water leaks while the reader is open.
  const reader = db.readSession();
  for (let k = 0; k < 200; k++) {
    sess.execute(`UPDATE t SET pad = 'p${k}-${pad}' WHERE id = 15`);
  }
  const withReaderOpen = db.pageCount;
  assert.ok(
    withReaderOpen > 200,
    `with an older reader pinned, compaction should defer and leak; high-water only ${withReaderOpen}`,
  );

  // Close the reader (watermark advances to committed): a further churn now compacts, so the high-water
  // stops climbing — it grows by a handful of pages (the first post-close commit extends before its own
  // compaction reclaims), not by another ~200.
  reader.close();
  for (let k = 200; k < 400; k++) {
    sess.execute(`UPDATE t SET pad = 'q${k}-${pad}' WHERE id = 15`);
  }
  const afterReaderClosed = db.pageCount;
  assert.ok(
    afterReaderClosed - withReaderOpen <= 64,
    `after the reader closed, compaction should reuse pages, not keep growing: ${withReaderOpen} then ${afterReaderClosed} (+${afterReaderClosed - withReaderOpen})`,
  );
  db.close();
});
