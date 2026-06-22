// P6.1 part B — incremental copy-on-write commit (spec/fileformat/format.md, *Allocation &
// incremental commit*). A commit appends only the dirty pages a mutation introduced and publishes the
// new root by alternating the meta slot, leaving the prior snapshot's pages intact. These per-core
// tests cover what a static golden cannot (the bytes depend on commit history): that a commit grows
// the file incrementally rather than rewriting it, that the meta slots alternate, and that a torn
// write of the latest commit falls back to the prior durable snapshot.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { close, create, Database, execute, open, toImage } from "../src/lib.ts";

function tmpDir(): string {
  return mkdtempSync(join(tmpdir(), "jed-"));
}

// slotTxid returns the txid of meta slot `slot` in a raw file image (page_size is the u32 at offset 8;
// the meta header's txid is at offset 12 within the slot's page — spec/fileformat/format.md).
function slotTxid(b: Uint8Array, slot: number): bigint {
  const dv = new DataView(b.buffer, b.byteOffset, b.byteLength);
  const ps = dv.getUint32(8, false);
  return dv.getBigUint64(slot * ps + 12, false);
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

test("a single-row commit appends only the dirty path", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "incremental_small_growth.jed");
    const ps = 256;
    const db = create(path, { pageSize: 256 });
    execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)");
    // Enough rows for a multi-level tree at 256-byte pages (≈3 records/leaf). Each insert
    // autocommits, so the file already holds many leaked pages by the end of the loop.
    const pad = "x".repeat(48);
    for (let i = 1; i <= 30; i++) {
      execute(db, `INSERT INTO t VALUES (${i}, 'row-${String(i).padStart(2, "0")}-${pad}')`);
    }

    // The whole tree spans many pages; a from-scratch image (no leaks) measures it.
    const wholePages = Math.floor(toImage(db, db.pageSize, db.txid).length / ps);
    assert.ok(wholePages >= 10, `the tree should span several pages (got ${wholePages})`);

    // One more row: the incremental commit appends only the rebuilt root→leaf path + catalog —
    // far fewer pages than the whole tree, and bounded by tree height, not table size. We track the
    // committed pageCount delta, not the file length — the file is preallocated in chunks ahead of
    // the high-water (spec/design/pager.md §7), so its physical size jumps by a chunk, not the
    // dirty-page count.
    const before = db.pageCount;
    execute(db, `INSERT INTO t VALUES (31, 'row-31-${pad}')`);
    const appended = db.pageCount - before;
    assert.ok(appended >= 2, `the commit must append its dirty path + catalog (got ${appended})`);
    assert.ok(
      appended < wholePages,
      `an incremental commit (${appended} pages) must not rewrite the whole ${wholePages}-page tree`,
    );
    assert.ok(
      appended <= 8,
      `the dirty path is bounded by tree height, not table size (got ${appended})`,
    );

    // And it reopens to the full, correct contents (leaked pages and all).
    close(db);
    const db2 = open(path);
    assert.deepEqual(
      ids(db2),
      Array.from({ length: 31 }, (_, i) => BigInt(i + 1)),
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a delete-heavy history reopens correctly", () => {
  // Deletes commit through the same incremental path but rebalance the tree (merge-then-split),
  // dirtying a different node set than inserts. Across many autocommitted inserts and deletes — each
  // leaking pages — the live snapshot must still reopen exactly (spec/fileformat/format.md).
  const dir = tmpDir();
  try {
    const path = join(dir, "incremental_deletes.jed");
    const db = create(path, { pageSize: 256 });
    execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)");
    const pad = "x".repeat(48);
    for (let i = 1; i <= 30; i++) {
      execute(db, `INSERT INTO t VALUES (${i}, 'row-${String(i).padStart(2, "0")}-${pad}')`);
    }
    for (let i = 1; i <= 20; i++) {
      execute(db, `DELETE FROM t WHERE id = ${i}`);
    }
    close(db);

    const db2 = open(path);
    assert.deepEqual(
      ids(db2),
      Array.from({ length: 10 }, (_, i) => BigInt(i + 21)),
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("meta slots alternate across commits", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "incremental_alternation.jed");
    const db = create(path);

    // create seeds BOTH slots at txid 1, so two valid metas exist from the first moment.
    let img = readFileSync(path);
    assert.equal(slotTxid(img, 0), 1n);
    assert.equal(slotTxid(img, 1), 1n);

    execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)"); // txid 2 → slot 0
    execute(db, "INSERT INTO t VALUES (1)"); // txid 3 → slot 1
    close(db);

    // Each commit writes only the alternate slot, leaving the prior published meta intact.
    img = readFileSync(path);
    assert.equal(slotTxid(img, 0), 2n, "even txid lands in slot 0");
    assert.equal(slotTxid(img, 1), 3n, "odd txid lands in slot 1");

    const db2 = open(path);
    assert.equal(db2.txid, 3n, "open adopts the highest valid txid");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a torn latest commit falls back to the prior snapshot", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "incremental_torn_meta.jed");
    const db = create(path);
    execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)"); // txid 2 (slot 0)
    execute(db, "INSERT INTO t VALUES (1)"); // txid 3 (slot 1)
    execute(db, "INSERT INTO t VALUES (2)"); // txid 4 (slot 0) — the newest commit
    close(db);

    // Simulate a torn write of the newest commit: corrupt slot 0's checksum (txid 4). The loader must
    // fall back to slot 1 (txid 3) — whose body pages copy-on-write never overwrote — so row 2's
    // commit vanishes but the prior snapshot (row 1 only) is intact and uncorrupted.
    const img = readFileSync(path);
    assert.equal(slotTxid(img, 0), 4n, "slot 0 holds the newest commit");
    img[32] ^= 0xff; // flip a CRC byte of slot 0's meta header
    writeFileSync(path, img);

    const db2 = open(path);
    assert.equal(db2.txid, 3n, "fell back to the prior committed snapshot");
    assert.deepEqual(ids(db2), [1n], "only the prior snapshot's row survives the torn write");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a commit preallocates file growth in chunks and reuses the slack", () => {
  // Chunked file preallocation (spec/design/pager.md §7, TODO.md durable-commit win): a commit that
  // grows past the allocation high-water extends the file by whole 1 MiB chunks of real zero blocks,
  // so the physical file is a multiple of the chunk and runs ahead of the committed pageCount. The
  // slack is unreferenced (the committed image round-trips exactly), and a later commit that fits
  // within it does not grow the file at all (the steady-state metadata-free path). The logical
  // pageCount is the real high-water — independent of the physical size.
  const CHUNK = 1024 * 1024; // PREALLOC_CHUNK_BYTES (pager.ts)
  const dir = tmpDir();
  try {
    const path = join(dir, "prealloc_chunks.jed");
    // A from-scratch image is just the empty catalog — far below one chunk — so the file starts
    // un-aligned (create writes exactly pageCount pages, no preallocation).
    const db = create(path); // default 8 KiB page size
    execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, pad text)");

    // One commit big enough to push the tree past a chunk: ~400 rows of a ~3.5 KiB pad ≈ 1.4 MiB of
    // tree, > the 128-page (1 MiB) chunk at the default 8 KiB page size.
    const pad = "p".repeat(3500);
    execute(db, "BEGIN");
    for (let i = 0; i < 400; i++) {
      execute(db, `INSERT INTO t VALUES (${i}, '${pad}')`);
    }
    execute(db, "COMMIT");

    const logical = db.pageCount * db.pageSize;
    const physical = statSync(path).size;
    assert.ok(
      db.pageCount > 128,
      `the batch should span more than one chunk's worth of pages (got ${db.pageCount})`,
    );
    assert.equal(
      physical % CHUNK,
      0,
      `physical file should grow in whole chunks (got ${physical})`,
    );
    assert.ok(
      physical >= logical && physical >= CHUNK,
      `preallocation should run ahead of the ${logical}-byte committed image (physical ${physical})`,
    );
    close(db);

    // The committed image round-trips exactly through the preallocated file (trailing slack is inert
    // zeros past the high-water).
    const db2 = open(path);
    const physicalBefore = statSync(path).size;
    assert.equal(ids(db2).length, 400, "all 400 rows survive the reopen");

    // A small commit fits within the preallocated slack, so the physical file does not grow at all —
    // the steady-state metadata-free commit path.
    execute(db2, `INSERT INTO t VALUES (1000, '${pad}')`);
    assert.equal(
      statSync(path).size,
      physicalBefore,
      "a commit within the slack reuses it without growing the file",
    );
    close(db2);

    // And the extra row is durable.
    const db3 = open(path);
    assert.equal(ids(db3).length, 401, "the in-slack commit persisted");
    close(db3);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
