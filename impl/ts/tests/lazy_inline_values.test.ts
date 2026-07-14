// L2/L3 — defer inline values at fault (spec/design/lazy-record.md §12). On the demand-paged path
// every variable-length / structured present value (text/bytea/decimal/json/jsonb/composite/
// array/range) is loaded as a deferred unfetched (form 0x00) — a zero-copy subarray view of the
// shared page block (form (a), L3) — instead of being eagerly decoded; the scan layer resolves
// exactly the query's touched columns, an untouched one is dropped still deferred. The reshape is
// cost-, result-, and byte-neutral (§8) regardless of representation (form (a)/(b)), so a paged file
// and a fully-resident in-memory database must observe identical rows and identical cost for every
// query shape — that mode-identity is the leak-catcher (an unresolved deferral escapes the scan
// layer as a loud poison throw, never silent NULL). Mirrors impl/rust/tests/lazy_inline_values.rs
// and impl/go/lazy_inline_values_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import type { ColType } from "../src/catalog.ts";
import { Decimal } from "../src/decimal.ts";
import { DEFAULT_PAGE_SIZE } from "../src/executor.ts";
import {
  decodeLeafNode,
  encodeLeafPax,
  encodeValue,
  makePage,
  recordSize,
  type OverflowPageOut,
  resolveUnfetched,
  resolveUnfetchedSelf,
} from "../src/format.ts";
import { colAt, keyAt, nodeLen, rowAt } from "../src/pmap.ts";
import { createDatabase, openDatabase } from "../src/tooling.ts";
import {
  arrayValue,
  byteaValue,
  decimalValue,
  intValue,
  render,
  textValue,
  type Value,
} from "../src/value.ts";
import { type Handle, errCode, queryOutcome } from "./util.ts";
import { memDb } from "./mem_db.ts";

const PAGE_LEAF = 2; // page_type for a B-tree leaf node

// Schema + rows exercising every deferrable type alongside a join partner and a secondary index.
// The default page size keeps every value inline-plain, so on a paged reopen each lands as an
// inline-deferred unfetched — the L2 case (nothing spills).
function seed(db: Handle): void {
  db.execute("CREATE TYPE addr AS (street text, zip i32)");
  db.execute(
    "CREATE TABLE t (id i32 PRIMARY KEY, name text, data bytea, amount decimal(12,2), " +
      "doc jsonb, tags i32[], home addr, span i32range)",
  );
  db.execute("CREATE INDEX t_name ON t (name)");
  db.execute(
    "INSERT INTO t VALUES " +
      "(1, 'alice', '\\xdeadbeef', 100.50, '{\"k\": 1, \"tag\": \"x\"}', ARRAY[10, 20, 30], ROW('Main St', 90210), '[1,5)'), " +
      "(2, 'bob', '\\xcafe', 2.25, '{\"k\": 2}', ARRAY[1, NULL, 3], ROW('Oak Ave', 12345), '[10,20]'), " +
      "(3, 'carol', NULL, NULL, NULL, NULL, ROW('Elm', NULL), 'empty'), " +
      "(4, 'dave', '\\x00ff', 9999.99, '{\"k\": 4, \"nested\": {\"a\": [1,2,3]}}', '{}', ROW(NULL, 7), '(,9)')",
  );
  db.execute("CREATE TABLE u (id i32 PRIMARY KEY, t_id i32, note text)");
  db.execute(
    "INSERT INTO u VALUES (1, 1, 'first'), (2, 1, 'again'), (3, 3, 'lonely'), (4, 99, 'orphan')",
  );
}

// rowsSorted runs sql and returns its rows rendered to strings and sorted — an order-insensitive
// multiset compare (a query without ORDER BY has unspecified order; sorting both sides is sound).
function rowsSorted(db: Handle, sql: string): string[] {
  const o = queryOutcome(db, sql);
  if (o.kind !== "query") throw new Error(`expected a query: ${sql}`);
  return o.rows.map((r) => r.map((v) => render(v)).join("\x1f")).sort();
}

function costOf(db: Handle, sql: string): bigint {
  return queryOutcome(db, sql).cost;
}

test("paged inline values match resident across query shapes", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-l2-shapes-"));
  try {
    const path = join(dir, "shapes.jed");
    const filedb = createDatabase({ path, skipFsync: true });
    seed(filedb);
    filedb.close();

    const mem = memDb().session();
    seed(mem);
    const paged = openDatabase(path, { skipFsync: true });

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
    paged.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("mutations preserve untouched inline values", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-l2-mut-"));
  try {
    const path = join(dir, "mut.jed");
    const filedb = createDatabase({ path, skipFsync: true });
    seed(filedb);
    filedb.close();

    const mem = memDb().session();
    seed(mem);

    const mutations = [
      "UPDATE t SET amount = amount + 1 WHERE id = 1",
      "UPDATE t SET name = 'robert' WHERE id = 2",
      "UPDATE t SET tags = ARRAY[7, 8] WHERE id = 4",
      "DELETE FROM t WHERE id = 3",
      "INSERT INTO t VALUES (5, 'erin', '\\xab', 1.00, '{\"k\":5}', ARRAY[9], ROW('New', 1), '[2,3)')",
      "UPDATE u SET note = 'edited' WHERE t_id = 1",
    ];
    for (const m of mutations) mem.execute(m);
    {
      const paged = openDatabase(path, { skipFsync: true });
      for (const m of mutations) paged.execute(m);
      paged.close();
    }
    const paged = openDatabase(path, { skipFsync: true });
    for (const sql of [
      "SELECT * FROM t",
      "SELECT id, name, amount, doc, tags, home, span, data FROM t ORDER BY id",
      "SELECT * FROM u",
    ]) {
      assert.deepEqual(rowsSorted(paged, sql), rowsSorted(mem, sql), `final state differs: ${sql}`);
    }
    paged.close();
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
    const filedb = createDatabase({ path, skipFsync: true });
    filedb.execute("CREATE TABLE t (id i32 PRIMARY KEY, body text, n i32)");
    filedb.execute(`INSERT INTO t VALUES (1, '${marker}', 42), (2, 'clean', 7)`);
    filedb.close();

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

    const db = openDatabase(path, { skipFsync: true });
    // Open faulted the leaf (skip-walk only); untouching queries never construct the body.
    assert.equal(rowsSorted(db, "SELECT id FROM t").length, 2);
    assert.deepEqual(rowsSorted(db, "SELECT id, n FROM t WHERE n = 42"), ["1\x1f42"]);
    // The clean row's body resolves fine.
    assert.deepEqual(rowsSorted(db, "SELECT body FROM t WHERE id = 2"), ["clean"]);
    // Touching the corrupted body runs the real decode: XX001.
    assert.equal(
      errCode(() => db.execute("SELECT body FROM t WHERE id = 1")),
      "XX001",
    );
    assert.equal(
      errCode(() => db.execute("SELECT * FROM t ORDER BY id")),
      "XX001",
    );
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("untouched deferred column rides a spilling sort", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-l2-spill-"));
  try {
    const path = join(dir, "spill.jed");
    const mem = memDb().session();
    const filedb = createDatabase({ path, skipFsync: true });
    for (const db of [mem, filedb]) {
      db.execute("CREATE TABLE t (id i32 PRIMARY KEY, k i32, label text, doc jsonb)");
    }
    for (let id = 0; id < 200; id++) {
      const k = (id * 48271) % 100;
      const row = `INSERT INTO t VALUES (${id}, ${k}, 'label-${id}-xxxxxxxxxx', '{"id": ${id}}')`;
      mem.execute(row);
      filedb.execute(row);
    }
    filedb.close();

    const paged = openDatabase(path, { skipFsync: true }).session();
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
    paged.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// L3, form (a) zero-copy block-shared deferral (spec/design/lazy-record.md §5a/§12). A faulted
// leaf's deferred inline values are SUBARRAY views of the one shared page block, never per-value
// copies (form (b), L2). This is the resident-memory dividend (§9): resident leaf bytes track
// ≈ pageSize. The property is invisible to results and cost (§8), so it is asserted white-box:
// every deferred value's `comp` subarray shares the page block's identical ArrayBuffer (a
// `.slice()` copy would own a fresh ArrayBuffer of exactly its own length). Mirrors the Rust
// faulted_leaf_shares_one_block_across_deferred_values and the Go equivalent.
test("a faulted leaf shares one page block across its deferred values", () => {
  const sc = (s: "i32" | "text" | "bytea" | "decimal"): ColType => ({ kind: "scalar", scalar: s });
  const i32 = sc("i32");
  // Variable-length / structured columns so every present value defers (§6); the i32 column stays
  // eagerly decoded (deferring a fixed-width scalar buys nothing).
  const colTypes: ColType[] = [
    i32,
    sc("text"),
    sc("bytea"),
    sc("decimal"),
    { kind: "array", elem: i32 }, // i32[]
  ];
  const ps = 8192; // large page → every value stays inline-plain (no spill)
  const capacity = ps - 16; // PAGE_HEADER

  const rows: Value[][] = [];
  for (let i = 0; i < 3; i++) {
    rows.push([
      intValue(BigInt(i)),
      textValue(`name-${i}-padding-padding`),
      byteaValue(new Uint8Array([i, i, i, i])),
      decimalValue(Decimal.fromDigitsScale(false, "12345", 2)),
      arrayValue([intValue(BigInt(i)), intValue(BigInt(i + 1))]),
    ]);
  }

  // Encode the records into one PAX leaf page payload (everything inline at this page size).
  let takeSeq = 100;
  const take = (): number => ++takeSeq;
  const ovf: OverflowPageOut[] = [];
  const keys = rows.map((_, i) => {
    const key = new Uint8Array(4);
    new DataView(key.buffer).setUint32(0, i, false);
    return key;
  });
  const payload = encodeLeafPax(colTypes, keys, rows, capacity, take, ovf);
  assert.equal(ovf.length, 0, "values must stay inline (no overflow) for the form-(a) case");
  const block = makePage(ps, 2 /* PAGE_LEAF */, rows.length, 0, payload);

  // Fault the leaf → Packed form (packed-leaf.md §5): the block + PAX directories are retained and NO
  // value is decoded (the decoded row vector is empty); rows reconstruct on demand, producing the same
  // inline-deferred unfetched (form (a)) block views the eager fault used to.
  const node = decodeLeafNode(block, 2, colTypes, null);
  assert.ok(node.packed !== undefined, "a faulted leaf is Packed (packed-leaf.md §5)");
  assert.equal(node.keys.length, 0, "a Packed leaf owns no per-record key objects");
  assert.equal(node.weights.length, 0, "a Packed leaf owns no eager weights");
  assert.equal(nodeLen(node), keys.length);
  keys.forEach((key, i) => {
    assert.deepEqual(keyAt(node, i), key);
    assert.equal(node.packed!.weight(i), recordSize(colTypes, key, rows[i]!, capacity));
  });
  assert.equal(
    node.vals.length,
    0,
    "a Packed leaf holds no decoded row vector (resident ≈ pageSize, §9)",
  );

  let deferred = 0;
  rows.forEach((_r, ri) => {
    const row = rowAt(node, ri);
    row.forEach((v, ci) => {
      if (v.kind !== "unfetched" || v.ref.form !== 0x00) return;
      deferred++;
      const comp = v.ref.comp!;
      // Form (a): the body is a SUBARRAY view of the page block, so it shares the block's identical
      // ArrayBuffer (one allocation, page_size bytes). A form-(b) `.slice()` copy owns a fresh
      // ArrayBuffer of exactly its own length.
      assert.ok(
        comp.buffer === block.buffer,
        `row ${ri} col ${ci}: deferred body must view the shared page block (form (a))`,
      );
      assert.equal(
        comp.buffer.byteLength,
        ps,
        `row ${ri} col ${ci}: the shared block is the whole page`,
      );
      // It still resolves to exactly the eager value (form (a) is decode-neutral).
      const got = resolveUnfetched(colTypes[ci]!, v.ref, () => {
        throw new Error("inline values read no overflow pages");
      });
      assert.deepEqual(
        encodeValue(colTypes[ci]!, got),
        encodeValue(colTypes[ci]!, rows[ri]![ci]!),
        `row ${ri} col ${ci}: resolved value differs from the eager value`,
      );
    });
  });
  // 3 rows × 4 deferrable columns (text/bytea/decimal/array) = 12 deferred values; the i32 column
  // stays eager (§6).
  assert.equal(deferred, 12, "every deferrable present value defers (form (a))");
});

// The touched-column path (packed-leaf.md §4/§6, the PAX dividend): colAt reconstructs ONLY the
// requested column of a Packed leaf, byte-identically to the whole-row reconstruction. Since B4
// (bplus-reshape.md §5) the masked row form is gone — reconstruction is uniformly lazy and a
// deferred value carries its own resolution handles, so resolveUnfetchedSelf reconstructs it with
// NO caller-supplied type or pager (the path the evaluator's column access takes when the static
// touched set missed). Mirrors the Rust packed_leaf_reconstructs_only_touched_columns.
test("a Packed leaf reconstructs only the touched columns", () => {
  const sc = (s: "i32" | "text" | "i64"): ColType => ({ kind: "scalar", scalar: s });
  const colTypes: ColType[] = [sc("i32"), sc("text"), sc("i64")];
  const ps = 8192;
  const capacity = ps - 16;
  const rows: Value[][] = [];
  for (let i = 0; i < 4; i++) {
    rows.push([intValue(BigInt(i)), textValue(`row-${i}`), intValue(BigInt(i * 1000))]);
  }
  let takeSeq = 100;
  const take = (): number => ++takeSeq;
  const ovf: OverflowPageOut[] = [];
  const keys = rows.map((_, i) => {
    const key = new Uint8Array(4);
    new DataView(key.buffer).setUint32(0, i, false);
    return key;
  });
  const payload = encodeLeafPax(colTypes, keys, rows, capacity, take, ovf);
  assert.equal(ovf.length, 0);
  const block = makePage(ps, PAGE_LEAF, rows.length, 0, payload);
  const node = decodeLeafNode(block, 2, colTypes, null);

  // Resolve a (possibly-deferred) value to its comparable eager bytes.
  const resolve = (v: Value, c: number): Uint8Array => {
    const got =
      v.kind === "unfetched"
        ? resolveUnfetched(colTypes[c]!, v.ref, () => {
            throw new Error("inline values read no overflow pages");
          })
        : v;
    return encodeValue(colTypes[c]!, got);
  };

  for (let i = 0; i < rows.length; i++) {
    const whole = rowAt(node, i);
    // colAt(c) equals the whole row's column c.
    for (let c = 0; c < colTypes.length; c++) {
      assert.deepEqual(
        resolve(colAt(node, i, c), c),
        resolve(whole[c]!, c),
        `row ${i} col ${c}: colAt differs from whole row`,
      );
    }
    // The B4 demand-fault backstop: a deferred value carries its own resolution handles, so
    // resolveUnfetchedSelf reconstructs it with NO caller-supplied type or pager.
    for (let c = 0; c < colTypes.length; c++) {
      const v = whole[c]!;
      if (v.kind !== "unfetched") continue;
      assert.deepEqual(
        encodeValue(colTypes[c]!, resolveUnfetchedSelf(v.ref)),
        resolve(v, c),
        `row ${i} col ${c}: self-resolution differs from context resolution`,
      );
    }
  }
});
