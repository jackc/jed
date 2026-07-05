// Physical lazy (read-on-touch) materialization of large values
// (spec/design/large-values.md §14, phase 2). A lazily-loaded record holds unfetched
// references for its external/compressed values; the scan layer resolves exactly the query's
// touched columns through the pager, the open-time reachability walk follows chains by
// headers only, and a dirty leaf's re-encode resolves what it must at commit. These tests pin
// all three physically: corrupting every overflow-chain *payload* on disk is invisible to
// open and to untouching queries, and surfaces as XX001 only when the spilled column is
// touched. Mirrors impl/rust/tests/lazy_large_values.rs and impl/go/lazy_large_values_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { createDatabase, openDatabase } from "../src/tooling.ts";
import { render } from "../src/value.ts";
import { type Handle, errCode, fillerText, queryOutcome } from "./util.ts";
import { memDb } from "./mem_db.ts";

const PAGE_SIZE = 256;
const PAGE_OVERFLOW = 4; // page_type for an overflow slab (large-values.md §12)

// One row per stored form at ps=256 (RECORD_MAX 114, cap 240, v7): id 1 external-plain
// (incompressible 600-char filler → a 3-page chain), id 2 external-compressed (half filler /
// half run → the ~212-byte block spills to a 1-page chain), id 3 inline-compressed (a
// 600-char run), id 4 plain inline.
function seed(db: Handle): void {
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, body text)");
  const plain = fillerText(600);
  const extc = fillerText(200) + "y".repeat(200);
  const inlc = "x".repeat(600);
  db.execute(`INSERT INTO t VALUES (1, '${plain}'), (2, '${extc}'), (3, '${inlc}'), (4, 'tiny')`);
}

// The v7 per-page checksum (format.md *Page header*) — replicated here (like fillerText) so the
// corruption below can stay checksum-valid: CRC-32/IEEE over the page minus its own 4-byte field
// at [12,16).
function pageCrc(page: Uint8Array): number {
  let crc = 0xffffffff;
  const feed = (b: number): void => {
    crc ^= b;
    for (let i = 0; i < 8; i++) {
      const mask = -(crc & 1);
      crc = (crc >>> 1) ^ (0xedb88320 & mask);
    }
  };
  for (let i = 0; i < 12; i++) feed(page[i]!);
  for (let i = 16; i < page.length; i++) feed(page[i]!);
  return (crc ^ 0xffffffff) >>> 0;
}

// Overwrite every overflow page's payload (offset 16+, v7) with 0xFF, keeping the 16-byte header
// (page_type / item_count / next_page) intact — so the header-only chain walk still works — then
// recompute the v7 per-page CRC so the page stays checksum-valid. This isolates the decode-time
// failure (non-UTF-8 / malformed LZ4 block) from the per-page checksum: a checksum-inconsistent
// corruption is instead caught at open (the dedicated test in checksum.test.ts).
function corruptOverflowPayloads(path: string): void {
  const bytes = readFileSync(path);
  let corrupted = 0;
  for (let i = 2; (i + 1) * PAGE_SIZE <= bytes.length; i++) {
    if (bytes[i * PAGE_SIZE] === PAGE_OVERFLOW) {
      bytes.fill(0xff, i * PAGE_SIZE + 16, (i + 1) * PAGE_SIZE);
      const page = bytes.subarray(i * PAGE_SIZE, (i + 1) * PAGE_SIZE);
      new DataView(page.buffer, page.byteOffset, page.byteLength).setUint32(
        12,
        pageCrc(page),
        false,
      );
      corrupted++;
    }
  }
  assert.ok(corrupted >= 4, "expected several overflow pages to corrupt");
  writeFileSync(path, bytes);
}

function rowsOf(db: Handle, sql: string) {
  const o = queryOutcome(db, sql);
  if (o.kind !== "query") throw new Error("expected a query");
  return o.rows;
}

function costOf(db: Handle, sql: string): bigint {
  return queryOutcome(db, sql).cost;
}

test("lazy: chains are read only when touched", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-lazy-"));
  const path = join(dir, "touch.jed");
  try {
    let db = createDatabase({ path, pageSize: PAGE_SIZE, skipFsync: true });
    seed(db);
    db.close();
    corruptOverflowPayloads(path);

    // Open walks live chains by headers only — corrupt payloads are invisible.
    db = openDatabase(path, { skipFsync: true });

    // Untouching queries never read a chain or decompress a block.
    assert.equal(rowsOf(db, "SELECT id FROM t").length, 4);
    assert.equal(render(rowsOf(db, "SELECT count(*) FROM t")[0]![0]), "4");

    // Touching the spilled column reads the chain: the corruption surfaces as XX001 —
    // non-UTF-8 for the external-plain text, a malformed LZ4 block for external-compressed.
    for (const id of [1, 2]) {
      assert.equal(
        errCode(() => db.execute(`SELECT body FROM t WHERE id = ${id}`)),
        "XX001",
        `id ${id}`,
      );
    }

    // The inline-compressed and plain rows live in the (uncorrupted) leaf: still exact.
    assert.equal(render(rowsOf(db, "SELECT body FROM t WHERE id = 3")[0]![0]), "x".repeat(600));
    assert.equal(render(rowsOf(db, "SELECT body FROM t WHERE id = 4")[0]![0]), "tiny");
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("lazy: values round-trip exactly through the paged path", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-lazy-"));
  const path = join(dir, "roundtrip.jed");
  try {
    let db = createDatabase({ path, pageSize: PAGE_SIZE, skipFsync: true });
    seed(db);
    db.close();
    db = openDatabase(path, { skipFsync: true });
    const got = rowsOf(db, "SELECT body FROM t").map((r) => render(r[0]));
    assert.deepEqual(got, [
      fillerText(600),
      fillerText(200) + "y".repeat(200),
      "x".repeat(600),
      "tiny",
    ]);
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("lazy: UPDATE of other columns preserves spilled values", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-lazy-"));
  const path = join(dir, "update.jed");
  const big = fillerText(600);
  try {
    let db = createDatabase({ path, pageSize: PAGE_SIZE, skipFsync: true });
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY, body text, n i32)");
    db.execute(`INSERT INTO t VALUES (1, '${big}', 10), (2, 'small', 20)`);
    db.close();

    db = openDatabase(path, { skipFsync: true });
    // Dirties the leaf carrying row 1's unfetched body without touching it: row 2's rewrite
    // resolves nothing, row 1 resolves at commit (large-values.md §14 — resolve-at-commit;
    // chain sharing stays the deferred follow-on).
    db.execute("UPDATE t SET n = 99 WHERE id = 2");
    // Rewrites row 1 itself: the rewrite materializes its body (part of the write work).
    db.execute("UPDATE t SET n = 11 WHERE id = 1");
    db.close();

    db = openDatabase(path, { skipFsync: true });
    const rows = rowsOf(db, "SELECT body, n FROM t");
    assert.equal(render(rows[0]![0]), big);
    assert.equal(render(rows[0]![1]), "11");
    assert.equal(render(rows[1]![0]), "small");
    assert.equal(render(rows[1]![1]), "99");
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("lazy: paged and resident costs match", () => {
  // Logical cost is mode-independent (cost.md §3): a demand-paged file and a fully-resident
  // in-memory database charge identical costs — the unfetched-reference units equal the
  // resident disposition plan's by construction.
  const dir = mkdtempSync(join(tmpdir(), "jed-lazy-"));
  const path = join(dir, "cost.jed");
  try {
    const mem = memDb(PAGE_SIZE).session();
    seed(mem);
    const filedb = createDatabase({ path, pageSize: PAGE_SIZE, skipFsync: true });
    seed(filedb);
    filedb.close();
    const paged = openDatabase(path, { skipFsync: true });
    for (const sql of [
      "SELECT * FROM t",
      "SELECT id FROM t",
      "SELECT count(*) FROM t",
      "SELECT min(body) FROM t",
      "SELECT body FROM t WHERE id = 1",
      "SELECT body FROM t WHERE id = 4",
      "SELECT id FROM t WHERE body = 'tiny'",
    ]) {
      assert.equal(costOf(mem, sql), costOf(paged, sql), sql);
    }
    paged.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
