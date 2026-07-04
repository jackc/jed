// Free-list / page reclamation (spec/fileformat/format.md, *Reclamation*). The commit allocator reuses
// pages a prior root abandoned instead of always extending the file. Since v25 the free-list is
// persisted (meta offset 28 → a page_type 7 chain) and reclamation is continuous within-session: a file
// commit reclaims this commit's fresh orphans in-commit (periodically — once the high-water passes ~2×
// the live count), so the high-water oscillates in [live, 2×live] across a long churn rather than growing
// monotonically, and open reads the persisted free-list directly (no reconstruction walk). These per-core
// tests cover what a static golden cannot (the bytes depend on commit history): that within-session churn
// stays bounded, that reopening reads the persisted free-list and a later churn stays bounded, that reuse
// round-trips, and that a torn latest commit *after reuse* still falls back to the intact prior snapshot.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { close, create, type Engine, execute, open } from "../src/tooling.ts";

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

// pageCountOf is the committed logical page high-water (db.pageCount) — the count the meta records,
// which directly reports whether a commit extended the high-water or reused a free page. We track
// this, not the file length: the file is preallocated in chunks ahead of the high-water
// (spec/design/pager.md §7), so its physical size no longer equals pageCount*pageSize.
function pageCountOf(db: Engine): number {
  return db.pageCount;
}

function ids(db: Engine): bigint[] {
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
function padOf(db: Engine, id: number): string | null {
  const o = execute(db, `SELECT pad FROM t WHERE id = ${id}`);
  assert.equal(o.kind, "query");
  if (o.kind !== "query" || o.rows.length === 0) return null;
  const v = o.rows[0]![0]!;
  return v.kind === "text" ? v.text : null;
}

function setup(path: string, rows: number): Engine {
  const db = create(path, { pageSize: PS });
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)");
  const base = "x".repeat(40);
  for (let i = 1; i <= rows; i++) {
    execute(db, `INSERT INTO t VALUES (${i}, 'r${String(i).padStart(2, "0")}-${base}')`);
  }
  return db;
}

function expectedIds(n: number): bigint[] {
  return Array.from({ length: n }, (_, i) => BigInt(i + 1));
}

test("within-session churn stays bounded and reopens from the persisted free-list", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "reclaim_reuse.jed");
    const db = setup(path, 30); // a multi-level tree at page 256
    const pad = "y".repeat(40);

    // Churn within this session: each UPDATE commit copies the root→leaf path + rewrites the catalog to
    // fresh pages, and v25 reclaims the pages the prior root abandoned in-commit (periodically), so the
    // high-water oscillates in [live, 2×live] rather than growing monotonically with the 60 updates.
    // (We track the committed pageCount, not the file length — preallocated in chunks, pager.md §7.)
    for (let k = 0; k < 60; k++) {
      execute(db, `UPDATE t SET pad = 'a${k}-${pad}' WHERE id = 15`);
    }
    const pcAfterChurn1 = pageCountOf(db);
    assert.ok(
      pcAfterChurn1 < 60,
      `within-session reclamation should bound the high-water (got ${pcAfterChurn1})`,
    );
    close(db);

    // Reopen: the free-list is read directly from the persisted chain (no reconstruction walk).
    const db2 = open(path);
    assert.equal(
      pageCountOf(db2),
      pcAfterChurn1,
      "reopen reads the persisted high-water, unchanged",
    );

    // The first post-reopen commit reuses free pages from the persisted list rather than extending.
    execute(db2, `UPDATE t SET pad = 'b0-${pad}' WHERE id = 15`);
    assert.ok(
      pageCountOf(db2) <= pcAfterChurn1 + 4,
      `the first commit after reopen reuses the persisted free-list (got ${pageCountOf(db2)})`,
    );

    // A whole second churn stays bounded too — reusing reclaimed pages, the high-water does not grow
    // with the churn count.
    for (let k = 1; k < 40; k++) {
      execute(db2, `UPDATE t SET pad = 'b${k}-${pad}' WHERE id = 15`);
    }
    assert.ok(
      pageCountOf(db2) <= 2 * pcAfterChurn1,
      `the second churn stays bounded (got ${pageCountOf(db2)}, ~2×${pcAfterChurn1})`,
    );

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

test("persisted free-list heads a page_type 7 chain", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "reclaim_persisted.jed");
    const db = setup(path, 40);
    const big = "z".repeat(40);
    for (let round = 0; round < 40; round++) {
      for (let id = 1; id <= 40; id++) {
        execute(db, `UPDATE t SET pad = 'r${round}-${id}-${big}' WHERE id = ${id}`);
      }
    }
    close(db);

    const img = readFileSync(path);
    const dv = new DataView(img.buffer, img.byteOffset, img.byteLength);
    const live = slotTxid(img, 1) > slotTxid(img, 0) ? 1 : 0;
    const head = dv.getUint32(live * PS + 28, false); // v25 free_list_head (meta offset 28)
    assert.ok(head >= 2, `the meta should record a persisted free-list head, got ${head}`);
    assert.equal(img[head * PS], 7, "the free-list head page is page_type 7");
    const pageCount = dv.getUint32(live * PS + 24, false);
    let freelistPages = 0;
    for (let i = 0; i < pageCount; i++) if (img[i * PS] === 7) freelistPages++;
    assert.ok(freelistPages >= 1, "the file carries at least one persisted free-list page");

    const db2 = open(path);
    assert.deepEqual(ids(db2), expectedIds(40));
    assert.ok(
      pageCountOf(db2) < 200,
      `reopened file is bounded by within-session reclamation (got ${pageCountOf(db2)})`,
    );
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
