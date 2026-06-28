// Demand paging (P6.4b, spec/design/pager.md §1/§4): a file-backed database with many leaf pages,
// reopened with a tiny buffer-pool budget, still scans and mutates correctly while keeping only a
// bounded number of leaves resident — the residency win, exercised end to end. Files are written under
// a fresh mkdtemp dir, never the repo tree.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { close, create, EngineError, execute, loadEngine, open } from "../src/lib.ts";
import { residentLeaves } from "../src/file.ts";
import type { Value } from "../src/lib.ts";

function intOf(v: Value): bigint {
  if (v.kind !== "int") throw new Error("expected an int value");
  return v.int;
}

test("demand paging: scans + mutates correctly with bounded residency", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-paging-"));
  try {
    const path = join(dir, "paging.jed");
    const n = 600;
    const CAP = 3;

    // Build a multi-level tree at a small page size, so a few hundred rows span many pages.
    const db = create(path, { pageSize: 256 });
    execute(db, "CREATE TABLE t (k i32 PRIMARY KEY, v i32)");
    execute(db, "BEGIN"); // one commit, not 600
    for (let k = 0; k < n; k++) execute(db, `INSERT INTO t VALUES (${k}, ${k * 2})`);
    execute(db, "COMMIT");
    close(db);

    // Reopen demand-paged with a 3-leaf budget.
    let db2 = open(path, { cacheBytes: CAP * 256 });
    // A PK table's skeleton load faults no leaves (it reads them only to count rows, uncached), so the
    // pool starts empty — and the file holds many pages.
    assert.equal(residentLeaves(db2), 0, "skeleton load caches no leaf");
    assert.ok(
      db2.pageCount > CAP * 5,
      `file has many more pages (${db2.pageCount}) than the budget`,
    );

    // A full scan faults every leaf through the bounded pool: results exact, residency bounded.
    const rows = db2.rowsInKeyOrder("t");
    assert.equal(rows.length, n);
    for (let i = 0; i < rows.length; i++) {
      assert.equal(intOf(rows[i]![0]!), BigInt(i));
      assert.equal(intOf(rows[i]![1]!), BigInt(i * 2));
    }
    assert.ok(
      residentLeaves(db2) <= CAP,
      `resident leaves ${residentLeaves(db2)} exceed budget ${CAP}`,
    );
    close(db2);

    // Mutate through the pool (each statement faults the leaf it touches), reopen, verify.
    db2 = open(path, { cacheBytes: CAP * 256 });
    execute(db2, "DELETE FROM t WHERE k = 100");
    execute(db2, "UPDATE t SET v = 999 WHERE k = 200");
    execute(db2, "INSERT INTO t VALUES (600, 1200)");
    assert.ok(residentLeaves(db2) <= CAP, "mutations keep residency bounded");
    close(db2); // autocommit already persisted each statement

    db2 = open(path, { cacheBytes: CAP * 256 });
    const after = db2.rowsInKeyOrder("t");
    assert.equal(after.length, n, "one deleted, one inserted");
    for (const r of after) {
      const k = intOf(r[0]!);
      assert.notEqual(k, 100n, "k=100 was deleted");
      if (k === 200n) assert.equal(intOf(r[1]!), 999n, "k=200 was updated");
      if (k === 600n) assert.equal(intOf(r[1]!), 1200n, "k=600 was inserted");
    }
    close(db2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// P6.4c (memory-budget API + large-file hardening, spec/design/pager.md §6): a database whose leaf
// pages far exceed a tiny cacheBytes budget opens via the public open(path, { cacheBytes }), and a
// repeated point-query workload keeps residentLeaves within the budget throughout (each scan faults
// leaves through the pool, which evicts under CLOCK).
test("memory budget: bounds residency under repeated lookups on a large file", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-budget-"));
  try {
    const path = join(dir, "budget.jed");
    const n = 2000;
    const CAP = 4;

    const db = create(path, { pageSize: 256 });
    execute(db, "CREATE TABLE t (k i32 PRIMARY KEY, v i32)");
    execute(db, "BEGIN");
    for (let k = 0; k < n; k++) execute(db, `INSERT INTO t VALUES (${k}, ${k + 1})`);
    execute(db, "COMMIT");
    close(db);

    const db2 = open(path, { cacheBytes: CAP * 256 });
    // The data dwarfs the budget: far more pages than CAP, yet nothing resident until a read.
    assert.ok(
      db2.pageCount > CAP * 20,
      `file (${db2.pageCount} pages) should dwarf the ${CAP}-page budget`,
    );
    assert.equal(residentLeaves(db2), 0);

    // A spread of point queries (each a full scan, no index) repeatedly faults leaves through the
    // bounded pool; residency never exceeds the budget, and every answer is correct.
    for (let k = 0; k < n; k += 97) {
      const o = execute(db2, `SELECT v FROM t WHERE k = ${k}`);
      assert.equal(o.kind, "query");
      if (o.kind === "query") {
        assert.equal(o.rows.length, 1);
        assert.equal(intOf(o.rows[0]![0]!), BigInt(k + 1));
      }
      assert.ok(
        residentLeaves(db2) <= CAP,
        `resident ${residentLeaves(db2)} exceeds budget ${CAP} at k=${k}`,
      );
    }
    close(db2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// P6.4c (spec/design/pager.md §3, api.md §2.1): a byte budget smaller than a single page still keeps
// one leaf resident — the max(1, cacheBytes / pageSize) floor — and still scans correctly. This is the
// pageSize > cacheBytes case.
test("memory budget: a sub-page budget keeps exactly one leaf resident", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-tiny-"));
  try {
    const path = join(dir, "tiny.jed");
    const n = 400;

    const db = create(path, { pageSize: 256 });
    execute(db, "CREATE TABLE t (k i32 PRIMARY KEY, v i32)");
    execute(db, "BEGIN");
    for (let k = 0; k < n; k++) execute(db, `INSERT INTO t VALUES (${k}, ${k + 1})`);
    execute(db, "COMMIT");
    close(db);

    // A 1-byte budget is far below the 256-byte page size: it must clamp to one resident leaf, not zero.
    const db2 = open(path, { cacheBytes: 1 });
    const rows = db2.rowsInKeyOrder("t");
    assert.equal(rows.length, n);
    for (let i = 0; i < rows.length; i++) {
      assert.equal(intOf(rows[i]![0]!), BigInt(i));
      assert.equal(intOf(rows[i]![1]!), BigInt(i + 1));
    }
    assert.equal(residentLeaves(db2), 1, "a sub-page budget keeps exactly one leaf resident");
    close(db2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// P6.4c page-size hardening (format.md *Page model*): create rejects a page size above MAX_PAGE_SIZE
// (64 KiB) — without the cap a huge page size forces a multi-gigabyte allocation.
test("page-size hardening: create rejects an oversized page size", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-huge-"));
  try {
    let code = "";
    let message = "";
    try {
      create(join(dir, "huge.jed"), { pageSize: 1 << 20 });
    } catch (e) {
      if (e instanceof EngineError) {
        code = e.code();
        message = e.message;
      }
    }
    assert.equal(code, "0A000", "oversized page size is feature_not_supported");
    assert.ok(message.includes("too large"), `message names the cause: ${message}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// P6.4c page-size hardening (format.md *Page model*): the read path rejects a file whose meta records
// an out-of-range page_size as corrupt — the range check runs before any allocation against that size,
// so a hostile file cannot force a giant allocation (CLAUDE.md §13).
test("page-size hardening: load rejects an oversized page_size", () => {
  // A crafted meta header recording page_size = 70000 (> MAX_PAGE_SIZE) in big-endian at offset 8.
  const image = new Uint8Array(200);
  image.set([0x4a, 0x45, 0x44, 0x42], 0); // "JEDB"
  new DataView(image.buffer).setUint32(8, 70000, false);
  let code = "";
  try {
    loadEngine(image);
  } catch (e) {
    if (e instanceof EngineError) code = e.code();
  }
  assert.equal(code, "XX001", "an out-of-range page size is data_corrupted");
});

// Page-size hardening (format.md *Page model*): a page size in range but not a power of two is
// rejected — 0A000 on create, XX001 on the read path. Power-of-two keeps page boundaries
// sector-aligned (CLAUDE.md §9) and collapses the legal set to nine values.
test("page-size hardening: rejects a non-power-of-two page size", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-pow2-"));
  try {
    let code = "";
    let message = "";
    try {
      create(join(dir, "pow2.jed"), { pageSize: 1000 }); // in [256, 65536] but not a power of two
    } catch (e) {
      if (e instanceof EngineError) {
        code = e.code();
        message = e.message;
      }
    }
    assert.equal(code, "0A000", "non-power-of-two page size is feature_not_supported");
    assert.ok(message.includes("power of two"), `message names the cause: ${message}`);

    // Read path: a crafted meta recording page_size = 1000 reads as corrupt.
    const image = new Uint8Array(4096);
    image.set([0x4a, 0x45, 0x44, 0x42], 0); // "JEDB"
    new DataView(image.buffer).setUint32(8, 1000, false);
    let readCode = "";
    try {
      loadEngine(image);
    } catch (e) {
      if (e instanceof EngineError) readCode = e.code();
    }
    assert.equal(readCode, "XX001", "a non-power-of-two page size is data_corrupted");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// The new floor (format.md *Page model*): 256 is the smallest legal page size; 128 — a power of two
// but below MIN_PAGE_SIZE — is rejected on create.
test("page-size hardening: rejects a page size below the 256 floor", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-floor-"));
  try {
    let code = "";
    let message = "";
    try {
      create(join(dir, "tiny.jed"), { pageSize: 128 });
    } catch (e) {
      if (e instanceof EngineError) {
        code = e.code();
        message = e.message;
      }
    }
    assert.equal(code, "0A000", "sub-256 page size is feature_not_supported");
    assert.ok(message.includes("too small"), `message names the cause: ${message}`);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
