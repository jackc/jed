// P6.2 — free-list / page reclamation (spec/fileformat/format.md, *Reclamation*). The commit
// allocator reuses pages a prior root abandoned instead of always extending the file: on open the
// free-list is reconstructed as [2, pageCount) minus the committed root's reachable pages, and a commit
// draws dirty/catalog pages from it (lowest-first) before extending. These per-core tests cover what a
// static golden cannot (the bytes depend on commit history): that reopening reclaims the dead pages a
// churn left so a later churn reuses them (the file stops growing), that reuse round-trips, and that a
// torn latest commit *after reuse* still falls back to the intact prior snapshot (a reused page was
// dead, so overwriting it never damaged the fallback).

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { close, create, Database, execute, open } from "../src/lib.ts";

const PS = 256;

function tmpDir(): string {
  return mkdtempSync(join(tmpdir(), "jed-"));
}

// slotTxid returns the txid of meta slot `slot` in a raw file image (spec/fileformat/format.md).
function slotTxid(b: Uint8Array, slot: number): bigint {
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const ps = dv.getUint32(8, false);
  return dv.getBigUint64(slot * ps + 12, false);
}

// pageCount equals the file length in pages (format invariant pageCount = fileSize/pageSize), so it
// directly reports whether a commit extended the file or reused a free page.
function pageCount(path: string): number {
  return Math.floor(statSync(path).size / PS);
}

function ids(db: Database): bigint[] {
  const o = execute(db, "SELECT id FROM t");
  assert.equal(o.kind, "query");
  if (o.kind !== "query") throw new Error("expected a query");
  return o.rows.map((r) => {
    const v = r[0]!;
    assert.equal(v.kind, "int");
    return v.kind === "int" ? v.int : -1n;
  });
}

// padOf returns the pad text of the row with `id`, or null if absent.
function padOf(db: Database, id: number): string | null {
  const o = execute(db, `SELECT pad FROM t WHERE id = ${id}`);
  assert.equal(o.kind, "query");
  if (o.kind !== "query" || o.rows.length === 0) return null;
  const v = o.rows[0]![0]!;
  return v.kind === "text" ? v.text : null;
}

function setup(path: string, rows: number): Database {
  const db = create(path, { pageSize: PS });
  execute(db, "CREATE TABLE t (id int32 PRIMARY KEY, pad text)");
  const base = "x".repeat(40);
  for (let i = 1; i <= rows; i++) {
    execute(db, `INSERT INTO t VALUES (${i}, 'r${String(i).padStart(2, "0")}-${base}')`);
  }
  return db;
}

function expectedIds(n: number): bigint[] {
  return Array.from({ length: n }, (_, i) => BigInt(i + 1));
}

test("reopening reclaims dead pages so a later churn reuses them", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "reclaim_reuse.jed");
    const db = setup(path, 30); // a multi-level tree at page 256
    const pad = "y".repeat(40);

    // Churn within this session: each UPDATE commit copies the root→leaf path + rewrites the catalog
    // to fresh pages and leaks the old ones (P6.2 does not reclaim mid-session), so the file grows.
    for (let k = 0; k < 60; k++) {
      execute(db, `UPDATE t SET pad = 'a${k}-${pad}' WHERE id = 15`);
    }
    const sizeAfterChurn1 = pageCount(path);
    close(db);

    // Reopen: the free-list is reconstructed from the ~60 churn iterations' dead pages.
    const db2 = open(path);
    const pcReopen = pageCount(path);
    assert.equal(pcReopen, sizeAfterChurn1, "reopen does not change the file");

    // The very first post-reopen commit reuses a free page rather than extending the file.
    execute(db2, `UPDATE t SET pad = 'b0-${pad}' WHERE id = 15`);
    assert.equal(pageCount(path), pcReopen, "the first commit after reopen reuses a dead page");

    // A whole second churn — shorter than the first, so the reclaimed pool covers it — does not grow
    // the file: the page count after equals the count after the first churn.
    for (let k = 1; k < 40; k++) {
      execute(db2, `UPDATE t SET pad = 'b${k}-${pad}' WHERE id = 15`);
    }
    assert.equal(pageCount(path), sizeAfterChurn1, "reusing reclaimed pages, the churn does not grow the file");

    // And the data is exactly right (reuse never clobbered a live page).
    assert.equal(padOf(db2, 15), `b39-${pad}`);
    assert.deepEqual(ids(db2), expectedIds(30));
    close(db2);
    const db3 = open(path);
    assert.equal(padOf(db3, 15), `b39-${pad}`);
    assert.deepEqual(ids(db3), expectedIds(30));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a heavy insert/delete churn reopens correctly with reuse", () => {
  // Insert/delete churn dirties a different node set than updates (split/merge rebalance) and, across a
  // reopen, exercises reuse over both. The live snapshot must reopen exactly.
  const dir = tmpDir();
  try {
    const path = join(dir, "reclaim_churn.jed");
    const db = setup(path, 25);
    const pad = "z".repeat(40);
    for (let k = 0; k < 40; k++) {
      execute(db, `INSERT INTO t VALUES (1000, 'k${k}-${pad}')`);
      execute(db, "DELETE FROM t WHERE id = 1000");
    }
    close(db);

    const db2 = open(path);
    for (let k = 0; k < 40; k++) {
      execute(db2, `INSERT INTO t VALUES (2000, 'm${k}-${pad}')`);
      execute(db2, "DELETE FROM t WHERE id = 2000");
    }
    execute(db2, `INSERT INTO t VALUES (26, 'p-${pad}')`);
    execute(db2, `INSERT INTO t VALUES (27, 'q-${pad}')`);
    close(db2);

    const db3 = open(path);
    assert.deepEqual(ids(db3), expectedIds(27));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a torn commit after reuse falls back to the intact prior snapshot", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "reclaim_torn.jed");
    const db = setup(path, 20);
    const pad = "w".repeat(40);
    for (let k = 0; k < 30; k++) {
      execute(db, `UPDATE t SET pad = 'c${k}-${pad}' WHERE id = 10`);
    }
    close(db);

    // Reopen so the free-list holds the churn's dead pages, then do two commits that reuse them.
    const db2 = open(path);
    execute(db2, `UPDATE t SET pad = 'A-${pad}' WHERE id = 10`); // prior snapshot
    const orig11 = padOf(db2, 11)!;
    execute(db2, `UPDATE t SET pad = 'B-${pad}' WHERE id = 11`); // newest commit
    close(db2);

    // Corrupt the newest meta slot's checksum (a torn write of the commit that reused free pages).
    const img = readFileSync(path);
    const newest = slotTxid(img, 1) > slotTxid(img, 0) ? 1 : 0;
    const priorTxid = slotTxid(img, 1 - newest);
    img[newest * PS + 32] ^= 0xff; // flip a CRC byte of the newest slot's meta header
    writeFileSync(path, img);

    // The loader falls back to the prior snapshot — intact even though the torn commit reused
    // (overwrote) free pages, because those pages were dead and the prior snapshot never referenced
    // them. Row 11's update vanishes; row 10's prior-commit value and every row survive.
    const db3 = open(path);
    assert.equal(db3.txid, priorTxid, "fell back to the prior committed snapshot");
    assert.equal(padOf(db3, 11), orig11, "the torn commit's row-11 update vanished");
    assert.equal(padOf(db3, 10), `A-${pad}`);
    assert.deepEqual(ids(db3), expectedIds(20));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
