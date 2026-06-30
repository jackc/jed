// L2 — defer inline values at fault (spec/design/lazy-record.md §12). On the demand-paged path
// every variable-length / structured present value (text/bytea/decimal/json/jsonb/composite/
// array/range) is loaded as an owned-span unfetched (form 0x00) instead of being eagerly decoded;
// the scan layer resolves exactly the query's touched columns, an untouched one is dropped still
// deferred. The reshape is cost-, result-, and byte-neutral (§8), so a demand-paged file and a
// fully-resident in-memory database must observe identical rows and identical cost for every query
// shape — that mode-identity is the leak-catcher (an unresolved deferral escapes the scan layer as
// a loud poison throw, never silent NULL). Mirrors impl/rust/tests/lazy_inline_values.rs and
// impl/go/lazy_inline_values_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { DEFAULT_PAGE_SIZE } from "../src/executor.ts";
import { close, create, Engine, execute, open } from "../src/tooling.ts";
import { render } from "../src/value.ts";
import { errCode } from "./util.ts";

const PAGE_LEAF = 2; // page_type for a B-tree leaf node

// Schema + rows exercising every deferrable type alongside a join partner and a secondary index.
// The default page size keeps every value inline-plain, so on a paged reopen each lands as an
// inline-deferred unfetched — the L2 case (nothing spills).
function seed(db: Engine): void {
  execute(db, "CREATE TYPE addr AS (street text, zip i32)");
  execute(
    db,
    "CREATE TABLE t (id i32 PRIMARY KEY, name text, data bytea, amount decimal(12,2), " +
      "doc jsonb, tags i32[], home addr, span i32range)",
  );
  execute(db, "CREATE INDEX t_name ON t (name)");
  execute(
    db,
    "INSERT INTO t VALUES " +
      "(1, 'alice', '\\xdeadbeef', 100.50, '{\"k\": 1, \"tag\": \"x\"}', ARRAY[10, 20, 30], ROW('Main St', 90210), '[1,5)'), " +
      "(2, 'bob', '\\xcafe', 2.25, '{\"k\": 2}', ARRAY[1, NULL, 3], ROW('Oak Ave', 12345), '[10,20]'), " +
      "(3, 'carol', NULL, NULL, NULL, NULL, ROW('Elm', NULL), 'empty'), " +
      "(4, 'dave', '\\x00ff', 9999.99, '{\"k\": 4, \"nested\": {\"a\": [1,2,3]}}', '{}', ROW(NULL, 7), '(,9)')",
  );
  execute(db, "CREATE TABLE u (id i32 PRIMARY KEY, t_id i32, note text)");
  execute(
    db,
    "INSERT INTO u VALUES (1, 1, 'first'), (2, 1, 'again'), (3, 3, 'lonely'), (4, 99, 'orphan')",
  );
}

// rowsSorted runs sql and returns its rows rendered to strings and sorted — an order-insensitive
// multiset compare (a query without ORDER BY has unspecified order; sorting both sides is sound).
function rowsSorted(db: Engine, sql: string): string[] {
  const o = execute(db, sql);
  if (o.kind !== "query") throw new Error(`expected a query: ${sql}`);
  return o.rows.map((r) => r.map((v) => render(v)).join("\x1f")).sort();
}

function costOf(db: Engine, sql: string): bigint {
  return execute(db, sql).cost;
}

test("paged inline values match resident across query shapes", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-l2-shapes-"));
  try {
    const path = join(dir, "shapes.jed");
    const filedb = create(path, {});
    seed(filedb);
    close(filedb);

    const mem = new Engine();
    seed(mem);
    const paged = open(path);

    const queries = [
      "SELECT * FROM t",
      "SELECT id FROM t",
      "SELECT name FROM t",
      "SELECT data FROM t",
      "SELECT amount FROM t",
      "SELECT doc FROM t",
      "SELECT tags FROM t",
      "SELECT home FROM t",
      "SELECT span FROM t",
      "SELECT id FROM t WHERE name = 'bob'",
      "SELECT id FROM t WHERE amount > 100",
      "SELECT id FROM t WHERE data = '\\xcafe'",
      "SELECT id FROM t WHERE name IS NULL",
      "SELECT id FROM t WHERE data IS NULL",
      "SELECT tags[1] FROM t",
      "SELECT (home).zip FROM t",
      "SELECT (home).street FROM t",
      "SELECT doc->>'k' FROM t",
      "SELECT id FROM t WHERE (doc->>'k') = '2'",
      "SELECT id FROM t WHERE lower(span) = 1",
      "SELECT name FROM t ORDER BY name",
      "SELECT id, name FROM t ORDER BY name DESC",
      "SELECT name, amount FROM t ORDER BY id",
      "SELECT DISTINCT name FROM t",
      "SELECT count(*), max(name), min(amount) FROM t",
      "SELECT amount, count(*) FROM t GROUP BY amount",
      "SELECT name FROM t GROUP BY name HAVING count(*) = 1",
      "SELECT name FROM t WHERE name = 'carol'",
      "SELECT id, name FROM t WHERE name > 'bob' ORDER BY name",
      "SELECT t.name, u.note FROM t JOIN u ON u.t_id = t.id",
      "SELECT t.name FROM t JOIN u ON u.t_id = t.id WHERE u.note = 'first'",
      "SELECT name FROM t WHERE id IN (SELECT t_id FROM u WHERE note = 'lonely')",
      "SELECT name FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.t_id = t.id)",
      "SELECT name FROM t WHERE id = (SELECT min(t_id) FROM u)",
      "SELECT name, row_number() OVER (ORDER BY id) FROM t",
      "SELECT name, count(*) OVER () FROM t",
      "WITH c AS (SELECT id, name FROM t) SELECT name FROM c WHERE id = 1",
      "WITH c AS (SELECT name, amount FROM t WHERE amount IS NOT NULL) SELECT name FROM c ORDER BY amount",
    ];
    for (const sql of queries) {
      assert.deepEqual(
        rowsSorted(paged, sql),
        rowsSorted(mem, sql),
        `rows differ (paged vs resident): ${sql}`,
      );
      assert.equal(
        costOf(paged, sql),
        costOf(mem, sql),
        `cost differs (paged vs resident): ${sql}`,
      );
    }
    close(paged);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("mutations preserve untouched inline values", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-l2-mut-"));
  try {
    const path = join(dir, "mut.jed");
    const filedb = create(path, {});
    seed(filedb);
    close(filedb);

    const mem = new Engine();
    seed(mem);

    const mutations = [
      "UPDATE t SET amount = amount + 1 WHERE id = 1",
      "UPDATE t SET name = 'robert' WHERE id = 2",
      "UPDATE t SET tags = ARRAY[7, 8] WHERE id = 4",
      "DELETE FROM t WHERE id = 3",
      "INSERT INTO t VALUES (5, 'erin', '\\xab', 1.00, '{\"k\":5}', ARRAY[9], ROW('New', 1), '[2,3)')",
      "UPDATE u SET note = 'edited' WHERE t_id = 1",
    ];
    for (const m of mutations) execute(mem, m);
    {
      const paged = open(path);
      for (const m of mutations) execute(paged, m);
      close(paged);
    }
    const paged = open(path);
    for (const sql of [
      "SELECT * FROM t",
      "SELECT id, name, amount, doc, tags, home, span, data FROM t ORDER BY id",
      "SELECT * FROM u",
    ]) {
      assert.deepEqual(rowsSorted(paged, sql), rowsSorted(mem, sql), `final state differs: ${sql}`);
    }
    close(paged);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// The v7 per-page checksum (format.md *Page header*), replicated so a corrupted page stays
// checksum-valid and the failure isolates to decode time, not the open-time checksum gate.
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

test("untouched corrupt inline body defers its error (read-on-touch)", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-l2-corrupt-"));
  try {
    const path = join(dir, "corrupt.jed");
    const marker = "Zq7Zq7Zq7Zq7Zq7Zq7Zq7Zq7Zq7Zq7Zq"; // 32 chars, not in catalog text
    const filedb = create(path, {});
    execute(filedb, "CREATE TABLE t (id i32 PRIMARY KEY, body text, n i32)");
    execute(filedb, `INSERT INTO t VALUES (1, '${marker}', 42), (2, 'clean', 7)`);
    close(filedb);

    // Corrupt the first content byte of the marker body to 0xFF (an invalid UTF-8 lead byte),
    // leaving the length prefix intact so the skip-walk advances identically, then repair the page
    // CRC so the corruption is checksum-valid (isolating the failure to decode time).
    const ps = DEFAULT_PAGE_SIZE;
    const bytes = readFileSync(path);
    const needle = Buffer.from(marker, "utf8");
    const at = bytes.indexOf(needle);
    assert.ok(at >= 0, "marker text body present in the file");
    const pageIdx = Math.floor(at / ps);
    assert.equal(bytes[pageIdx * ps], PAGE_LEAF, "marker lives in a leaf page");
    bytes[at] = 0xff;
    const page = bytes.subarray(pageIdx * ps, (pageIdx + 1) * ps);
    new DataView(page.buffer, page.byteOffset, page.byteLength).setUint32(12, pageCrc(page), false);
    writeFileSync(path, bytes);

    const db = open(path);
    // Open faulted the leaf (skip-walk only); untouching queries never construct the body.
    assert.equal(rowsSorted(db, "SELECT id FROM t").length, 2);
    assert.deepEqual(rowsSorted(db, "SELECT id, n FROM t WHERE n = 42"), ["1\x1f42"]);
    // The clean row's body resolves fine.
    assert.deepEqual(rowsSorted(db, "SELECT body FROM t WHERE id = 2"), ["clean"]);
    // Touching the corrupted body runs the real decode: XX001.
    assert.equal(
      errCode(() => execute(db, "SELECT body FROM t WHERE id = 1")),
      "XX001",
    );
    assert.equal(
      errCode(() => execute(db, "SELECT * FROM t ORDER BY id")),
      "XX001",
    );
    close(db);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("untouched deferred column rides a spilling sort", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-l2-spill-"));
  try {
    const path = join(dir, "spill.jed");
    const mem = new Engine();
    const filedb = create(path, {});
    for (const db of [mem, filedb]) {
      execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, k i32, label text, doc jsonb)");
    }
    for (let id = 0; id < 200; id++) {
      const k = (id * 48271) % 100;
      const row = `INSERT INTO t VALUES (${id}, ${k}, 'label-${id}-xxxxxxxxxx', '{"id": ${id}}')`;
      execute(mem, row);
      execute(filedb, row);
    }
    close(filedb);

    const paged = open(path);
    paged.setWorkMem(128); // ~2-3 rows per run → dozens of spilled runs + a deep k-way merge
    for (const sql of [
      "SELECT id FROM t ORDER BY k, id",
      "SELECT id, k FROM t ORDER BY k DESC, id DESC",
      "SELECT id FROM t ORDER BY k, id LIMIT 13 OFFSET 9",
      "SELECT label FROM t ORDER BY k, id LIMIT 5",
    ]) {
      assert.deepEqual(
        rowsSorted(paged, sql),
        rowsSorted(mem, sql),
        `spilling sort differs: ${sql}`,
      );
      assert.equal(costOf(paged, sql), costOf(mem, sql), `cost differs: ${sql}`);
    }
    close(paged);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
