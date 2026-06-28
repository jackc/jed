// Slice A — out-of-line large values / overflow pages (spec/design/large-values.md §12). A value
// that would push a record past RECORD_MAX spills to a chain of overflow pages (page_type 4),
// leaving a fixed pointer in the record. These per-core tests cover what a static golden cannot (an
// incremental file's bytes depend on commit history): a multi-page chain round-trips, small values
// never spill, the free-list keeps live chains and reclaims dead ones, and the default demand-paged
// file path reads a spilled value back exactly.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { close, create, Engine, execute, loadEngine, open, toImage } from "../src/tooling.ts";
import { fillerText } from "./util.ts";

const PAGE_OVERFLOW = 4; // page_type for an overflow slab (large-values.md §12)

// countPageType counts body pages of a given page_type in an image (meta slots start with the magic,
// so they never collide with a small page_type byte).
function countPageType(image: Uint8Array, ps: number, ty: number): number {
  let c = 0;
  for (let i = 0; i + ps <= image.length; i += ps) if (image[i] === ty) c++;
  return c;
}

// rowsOf runs a SELECT and returns the rows (asserting it is a query).
function rowsOf(db: Engine, sql: string) {
  const o = execute(db, sql);
  assert.equal(o.kind, "query");
  if (o.kind !== "query") throw new Error("expected a query");
  return o.rows;
}

function textOf(v: { kind: string; text?: string } | undefined): string | null {
  return v && v.kind === "text" ? (v.text ?? null) : null;
}

// A ~1250-byte text value forces a multi-page overflow chain at page 256 (RECORD_MAX = 114, cap = 240).
const BIG = fillerText(1250); // incompressible, so Slice B keeps it external-plain

function bigValueDB(): Engine {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, body text)");
  execute(db, `INSERT INTO t VALUES (1, '${BIG}')`);
  execute(db, "INSERT INTO t VALUES (2, 'tiny')");
  return db;
}

test("external value spans an overflow chain and round-trips byte-identically", () => {
  const db = bigValueDB();
  const image = toImage(db, 256, 1n);
  assert.ok(
    countPageType(image, 256, PAGE_OVERFLOW) >= 2,
    "a large value spans several overflow pages",
  );

  const loaded = loadEngine(image);
  // Re-serialization is byte-identical (deterministic spill + chain allocation).
  assert.deepEqual(toImage(loaded, 256, 1n), image);
  const rows = rowsOf(loaded, "SELECT id, body FROM t ORDER BY id");
  assert.equal(rows.length, 2);
  assert.equal(textOf(rows[0]![1]), BIG);
  assert.equal(textOf(rows[1]![1]), "tiny");
});

test("small values never spill", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i16)");
  execute(db, "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)");
  const image = toImage(db, 256, 1n);
  assert.equal(
    countPageType(image, 256, PAGE_OVERFLOW),
    0,
    "inline-fitting values are never externalized",
  );
});

test("load reclaims only dead overflow pages", () => {
  const image = toImage(bigValueDB(), 256, 1n);
  const loaded = loadEngine(image);
  const ovf = countPageType(image, 256, PAGE_OVERFLOW);
  assert.ok(ovf >= 2);
  // Live chain pages are reachable, so they are NOT on the free-list (else a later commit would
  // reuse a still-referenced page).
  assert.ok(
    loaded.freePages.length < ovf,
    `live overflow pages (${ovf}) must not be free (${loaded.freePages.length})`,
  );
});

test("external value through the default demand-paged file path, with reclamation", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-lv-"));
  const path = join(dir, "large_values.jed");
  const big = fillerText(1500); // incompressible ≫ RECORD_MAX at ps 256 ⇒ a multi-page overflow chain
  try {
    let db = create(path, { pageSize: 256 });
    execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, body text)");
    execute(db, `INSERT INTO t VALUES (1, '${big}')`);
    execute(db, "INSERT INTO t VALUES (2, 'small')");
    close(db);

    // Reopen demand-paged (the default open): the big value reconstructs exactly through the
    // pager-backed chain read.
    db = open(path);
    let rows = rowsOf(db, "SELECT id, body FROM t ORDER BY id");
    assert.equal(rows.length, 2);
    assert.equal(textOf(rows[0]![1]), big);
    assert.equal(textOf(rows[1]![1]), "small");
    close(db);

    // Delete the big row; its chain is orphaned (leaked this session).
    db = open(path);
    execute(db, "DELETE FROM t WHERE id = 1");
    close(db);

    // Reopen: the free-list reconstruction collects only live chains, so the dead chain's pages are
    // now free. Re-inserting a large value reuses them — the high-water grows by a handful of pages,
    // not by a whole fresh chain (~7 pages).
    db = open(path);
    const before = db.pageCount;
    execute(db, `INSERT INTO t VALUES (3, '${big}')`);
    const after = db.pageCount;
    close(db);
    assert.ok(
      after <= before + 3,
      `re-insert did not reuse reclaimed pages (pageCount ${before} → ${after})`,
    );

    // Final correctness through the paged path.
    db = open(path);
    rows = rowsOf(db, "SELECT body FROM t WHERE id = 3");
    assert.equal(rows.length, 1);
    assert.equal(textOf(rows[0]![0]), big);
    close(db);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
