// The reclamation triggers. A file commit lists the pages it orphans in its own free list
// (incremental-reclaim.md), so steady churn never walks the database and the file stays at its live
// pages plus what the last commit wrote. The in-memory post-commit path keeps the periodic rebuild
// (format.md *Reclamation*): the walk runs once the pages off the free list pass compact_growth × the
// live count at the last reclamation, not once the high-water does — the high-water never shrinks, so
// measuring it made every commit after the first crossing walk the whole database again. These tests
// run a steady single-row UPDATE churn on each path.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { capFirstBody } from "../src/cap.ts";
import { reclaimWalks } from "../src/format.ts";
import { close, create, type Database, execute } from "../src/tooling.ts";
import { memDb } from "./mem_db.ts";

const ROUNDS = 200;
const SETUP =
  "INSERT INTO t SELECT g, 'pad-pad-pad-pad-pad-pad-pad-pad' || g::text FROM generate_series(1, 1000) g";
const churn = (k: number): string => `UPDATE t SET pad = 'r${k}' WHERE id = 500`;

// countWalks crosses the threshold with a warm-up churn, then counts the steady commits that walked.
function countWalks(run: (sql: string) => void, generation: () => bigint): number {
  run("CREATE TABLE t (id i32 PRIMARY KEY, pad text)");
  run(SETUP);
  for (let k = 0; k < ROUNDS; k++) run(churn(k));
  assert.notEqual(generation(), 0n, "the warm-up churn never reclaimed");
  let walks = 0;
  for (let k = 0; k < ROUNDS; k++) {
    const before = generation();
    run(churn(ROUNDS + k));
    if (generation() !== before) walks++;
  }
  return walks;
}

// assertFewWalks checks that the churn walked a small fraction of its commits: one walk per commit
// is the high-water regression.
function assertFewWalks(walks: number): void {
  assert.ok(
    walks > 0 && walks <= ROUNDS / 10,
    `${walks} of ${ROUNDS} steady commits walked the database (want between 1 and ${ROUNDS / 10})`,
  );
}

test("file commits list their orphans", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-trigger-"));
  try {
    const db = create(join(dir, "trigger.jed"), { pageSize: 256 });
    execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)");
    execute(db, SETUP);
    const walks = reclaimWalks.count;
    for (let k = 0; k < 2 * ROUNDS; k++) {
      execute(db, churn(k));
      // Every page below the high-water is meta, live, listed free, or a free-list chain or manifest
      // page the commit itself wrote.
      const first = capFirstBody(db.paging!.cap());
      const unlisted =
        db.pageCount - first - db.live.live - db.freePages.length - db.freeChain.length;
      assert.ok(
        unlisted >= 0 && unlisted <= 2,
        `round ${k}: ${db.pageCount} pages, ${db.live.live} live, ${db.freePages.length} free, ` +
          `${db.freeChain.length} chain: ${unlisted} dead pages unlisted`,
      );
    }
    assert.equal(
      reclaimWalks.count - walks,
      0,
      `steady commits walked the database (want 0 of ${2 * ROUNDS})`,
    );
    close(db);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("in-memory commits past the reclamation threshold do not walk every time", () => {
  const db = memDb(256);
  const storage = (
    db as unknown as { core: { storage: { reclaimWithinSession: boolean; freeGenTxid: bigint } } }
  ).core.storage;
  storage.reclaimWithinSession = true;
  const sess = db.session();
  assertFewWalks(
    countWalks(
      (sql) => sess.execute(sql),
      () => storage.freeGenTxid,
    ),
  );
  (db as Database).close();
});
