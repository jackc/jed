// Large values held by reference in eager-lane buffers (spec/design/large-values.md §16). A touched
// text column that is not a key column stays an unfetched reference in the eager lane's relation
// buffer and resolves only when an expression reads it. With every chain payload corrupt on disk, a
// query raises XX001 exactly when an expression reads a corrupt value, and succeeds when none does.
// Mirrors impl/go/buffered_large_values_test.go and impl/rust/tests/buffered_large_values.rs. The
// seeding and corruption helpers mirror lazy_large_values.test.ts.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { createDatabase, EngineError, openDatabase } from "../src/tooling.ts";
import { render, type Value } from "../src/value.ts";
import { type Handle, fillerText, queryOutcome } from "./util.ts";

const PAGE_SIZE = 256;
const PAGE_OVERFLOW = 4; // page_type for an overflow slab (large-values.md §12)

// Queries over seed's table, whose rows 1 and 2 hold the corrupt chains. ok lists the expected rows
// (rendered, sorted) when no expression reads a corrupt value; null expects XX001.
const CASES: { sql: string; ok: string[] | null }[] = [
  // Aggregate: the CASE reads body only for the intact rows 3 and 4, the FILTER only for them.
  { sql: "SELECT sum(CASE WHEN id > 2 THEN length(body) ELSE 0 END) FROM t", ok: ["604"] },
  { sql: "SELECT count(body) FILTER (WHERE id > 2) FROM t", ok: ["2"] },
  { sql: "SELECT sum(length(body)) FROM t", ok: null },
  // Join: b.body is no join key, so it is read only where the CASE takes it.
  {
    sql: "SELECT a.id, CASE WHEN a.id > 2 THEN length(b.body) END FROM t a JOIN t b ON a.id = b.id",
    ok: ["1|NULL", "2|NULL", "3|600", "4|4"],
  },
  { sql: "SELECT a.id, length(b.body) FROM t a JOIN t b ON a.id = b.id", ok: null },
  // Window: the window reads only its key and the projection reads body only for rows 3 and 4.
  {
    sql: "SELECT id, CASE WHEN id > 2 THEN length(body) END, row_number() OVER (ORDER BY id) FROM t",
    ok: ["1|NULL|1", "2|NULL|2", "3|600|3", "4|4|4"],
  },
  { sql: "SELECT id, lag(length(body)) OVER (ORDER BY id) FROM t", ok: null },
  // A correlated subquery reads o.body into its primary-key bound without evaluating it, so body is
  // a key column: resolved when t is scanned, the corrupt rows raise XX001.
  {
    sql: "SELECT id, (SELECT count(*) FROM u WHERE u.k = o.body), row_number() OVER (ORDER BY id) FROM t o",
    ok: null,
  },
];

// One row per stored form at ps=256 (lazy_large_values.test.ts seed): id 1 external-plain, id 2
// external-compressed, id 3 inline-compressed, id 4 plain inline; plus a text-keyed table the
// correlated case probes (Go bufferedSeed).
function seed(db: Handle): void {
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, body text)");
  const plain = fillerText(600);
  const extc = fillerText(200) + "y".repeat(200);
  const inlc = "x".repeat(600);
  db.execute(`INSERT INTO t VALUES (1, '${plain}'), (2, '${extc}'), (3, '${inlc}'), (4, 'tiny')`);
  db.execute("CREATE TABLE u (k text PRIMARY KEY)");
  db.execute("INSERT INTO u VALUES ('tiny')");
}

// The v7 per-page checksum: CRC-32/IEEE over the page minus its own 4-byte field at [12,16).
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

// Overwrite every overflow page's payload with 0xFF, keeping its header and a valid page checksum
// (lazy_large_values.test.ts corruptOverflowPayloads).
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

function renderSorted(rows: Value[][]): string[] {
  return rows.map((r) => r.map(render).join("|")).sort();
}

test("buffered: large values resolve when read", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-buffered-"));
  const path = join(dir, "buffered.jed");
  try {
    let db = createDatabase({ path, pageSize: PAGE_SIZE, skipFsync: true });
    seed(db);
    // A fully flushed snapshot image isolates lazy value decoding from v33 recovery validation,
    // which deliberately reads every newest-generation dirty page before accepting its descriptor.
    const checkpoint = db.toImage(PAGE_SIZE, 1n);
    db.close();
    writeFileSync(path, checkpoint);
    corruptOverflowPayloads(path);
    db = openDatabase(path, { skipFsync: true });
    try {
      const s = db.session();
      // An unlimited work_mem keeps aggregation and hash joins in the eager lane (memory.md §5.4).
      s.setWorkMem(0);
      for (const c of CASES) {
        let out: ReturnType<typeof queryOutcome> | null = null;
        let err: unknown = null;
        try {
          out = queryOutcome(s, c.sql);
        } catch (e) {
          err = e;
        }
        if (c.ok === null) {
          assert.ok(
            err instanceof EngineError && err.code() === "XX001",
            `${c.sql}: want XX001, got ${String(err)}`,
          );
          continue;
        }
        if (err !== null) throw err;
        assert.equal(out!.kind, "query", c.sql);
        if (out!.kind !== "query") continue;
        assert.deepEqual(renderSorted(out!.rows), c.ok, c.sql);
      }
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// The same queries cost the same and return the same rows on disk-read rows (buffered references) as
// on resident rows.
test("buffered: disk-read rows match resident rows", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-buffered-"));
  const path = join(dir, "buffered_intact.jed");
  try {
    let disk = createDatabase({ path, pageSize: PAGE_SIZE, skipFsync: true });
    seed(disk);
    disk.close();
    disk = openDatabase(path, { skipFsync: true });
    const resident = createDatabase({
      path: join(dir, "buffered_resident.jed"),
      pageSize: PAGE_SIZE,
      skipFsync: true,
    });
    try {
      const ds = disk.session();
      const rs = resident.session();
      ds.setWorkMem(0);
      rs.setWorkMem(0);
      rs.execute("BEGIN");
      seed(rs);
      for (const c of CASES) {
        const a = queryOutcome(ds, c.sql);
        const b = queryOutcome(rs, c.sql);
        if (a.kind !== "query" || b.kind !== "query") throw new Error(`${c.sql}: expected a query`);
        assert.deepEqual(renderSorted(a.rows), renderSorted(b.rows), c.sql);
        assert.equal(a.cost, b.cost, c.sql);
      }
      rs.execute("ROLLBACK");
    } finally {
      resident.close();
      disk.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
