// Chain sharing on rewrite (spec/design/large-values.md §15). A rewritten record keeps an unchanged
// large value's stored form: an UPDATE that does not assign the column, and the other rows of a leaf a
// commit rewrites, write the value from its stored bytes and keep its overflow chain — no chain read,
// no decompression, no chain write. The plan, the bytes other than page numbers, and the cost are
// those of a fresh plan, so cost is the same whether a row was read from disk or is still resident.
// These tests pin the physical sharing (corrupt chain payloads survive a rewrite untouched; no
// overflow page is written) and the cost identity. Mirrors impl/rust/tests/chain_sharing.rs and
// impl/go/chain_sharing_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { createDatabase, openDatabase, verifyLivePages } from "../src/tooling.ts";
import { render } from "../src/value.ts";
import { type Handle, errCode, fillerText, queryOutcome } from "./util.ts";
import { memDb } from "./mem_db.ts";
import { checkImage, checkLive } from "../src/sim/check.ts";

const PAGE_SIZE = 256;
const PAGE_OVERFLOW = 4; // page_type for an overflow slab (large-values.md §12)

// Row 1 external-plain (a 3-page chain of incompressible filler), row 2 external-compressed (a
// 1-page chain), row 3 inline-compressed, rows 4..12 small — at ps=256 (RECORD_MAX 114) — in one
// statement, so every chain is written exactly once.
function seed(db: Handle): void {
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, body text, n i32)");
  const plain = fillerText(600);
  const extc = fillerText(200) + "y".repeat(200);
  const inlc = "x".repeat(600);
  const small: string[] = [];
  for (let g = 4; g <= 12; g++) small.push(`(${g}, 'small ${g}', ${g})`);
  db.execute(
    `INSERT INTO t VALUES (1, '${plain}', 1), (2, '${extc}', 2), (3, '${inlc}', 3), ${small.join(", ")}`,
  );
}

function createSeeded(path: string): void {
  const db = createDatabase({ path, pageSize: PAGE_SIZE, skipFsync: true });
  seed(db);
  db.close();
}

function rendered(db: Handle, sql: string): string[][] {
  const o = queryOutcome(db, sql);
  if (o.kind !== "query") throw new Error("expected a query");
  return o.rows.map((r) => r.map((v) => render(v)));
}

function costOf(db: Handle, sql: string): bigint {
  return queryOutcome(db, sql).cost;
}

// The CRC-32/IEEE page checksum over the page minus its own field at [12, 16) (format.md *Page
// header*), so a corrupted payload stays checksum-valid.
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

// Rewrite the file as its from-scratch image with every overflow page's payload set to 0xFF (headers
// and checksums kept valid): any read of a chain's bytes now fails XX001.
function corruptOverflowPayloads(path: string): void {
  const db = openDatabase(path, { skipFsync: true });
  const bytes = db.toImage(PAGE_SIZE, db.txid);
  db.close();
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
  assert.equal(corrupted, 4, "a 3-page and a 1-page chain");
  writeFileSync(path, bytes);
}

// Every overflow page in the file, by page index (hex-encoded for comparison).
function overflowPages(path: string): Map<number, string> {
  const bytes = readFileSync(path);
  const out = new Map<number, string>();
  for (let i = 2; (i + 1) * PAGE_SIZE <= bytes.length; i++) {
    if (bytes[i * PAGE_SIZE] === PAGE_OVERFLOW) {
      out.set(i, bytes.subarray(i * PAGE_SIZE, (i + 1) * PAGE_SIZE).toString("hex"));
    }
  }
  return out;
}

function withDir(prefix: string, fn: (dir: string) => void): void {
  const dir = mkdtempSync(join(tmpdir(), prefix));
  try {
    fn(dir);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
}

// Neither an UPDATE that does not assign the spilled column nor a commit that rewrites the leaf
// around it reads the chain: with every chain payload corrupt, both succeed, and the corruption is
// still there afterwards (the chain was kept, not rewritten).
test("chain sharing: rewrites never read a kept chain", () => {
  verifyLivePages.enabled = true;
  try {
    withDir("jed-chain-corrupt-", (dir) => {
      const path = join(dir, "corrupt.jed");
      createSeeded(path);
      corruptOverflowPayloads(path);
      let db = openDatabase(path, { skipFsync: true });
      db.execute("UPDATE t SET n = n + 100");
      db.execute("INSERT INTO t VALUES (13, 'new', 13)");
      db.execute("UPDATE t SET id = 0 WHERE id = 2");
      assert.deepEqual(rendered(db, "SELECT sum(n) FROM t"), [["1291"]]);
      db.close();
      db = openDatabase(path, { skipFsync: true });
      try {
        for (const id of [0, 1]) {
          assert.equal(
            errCode(() => db.execute(`SELECT body FROM t WHERE id = ${id}`)),
            "XX001",
            `id ${id}: the kept chain is still the corrupt one`,
          );
        }
        assert.deepEqual(rendered(db, "SELECT body FROM t WHERE id = 3"), [["x".repeat(600)]]);
      } finally {
        db.close();
      }
    });
  } finally {
    verifyLivePages.enabled = false;
  }
});

// A rewrite keeps a chain even when the statement reads the column (in its WHERE), as long as it
// does not assign it: no overflow page is written or changed, and the values, a compaction of the
// result, and the costs all come back exactly.
test("chain sharing: read but unassigned values keep their chains", () => {
  verifyLivePages.enabled = true;
  try {
    withDir("jed-chain-read-", (dir) => {
      const path = join(dir, "read.jed");
      createSeeded(path);
      const before = overflowPages(path);
      assert.equal(before.size, 4);
      let db = openDatabase(path, { skipFsync: true });
      db.execute("UPDATE t SET n = -n WHERE length(body) > 3");
      db.execute("UPDATE t SET id = id + 100 WHERE id <= 2");
      const want = rendered(db, "SELECT id, body, n FROM t ORDER BY id");
      db.close();
      assert.deepEqual(overflowPages(path), before, "no chain page written");

      db = openDatabase(path, { skipFsync: true });
      try {
        const queries = ["SELECT * FROM t", "SELECT body FROM t WHERE id = 101"];
        assert.deepEqual(rendered(db, "SELECT id, body, n FROM t ORDER BY id"), want);
        const costs = queries.map((q) => costOf(db, q));
        db.compact("main");
        assert.deepEqual(rendered(db, "SELECT id, body, n FROM t ORDER BY id"), want);
        assert.deepEqual(
          queries.map((q) => costOf(db, q)),
          costs,
          "compaction keeps every stored form",
        );
      } finally {
        db.close();
      }
    });
  } finally {
    verifyLivePages.enabled = false;
  }
});

// Cost stays logical (cost.md §3): the same statements charge the same whether the rows were read
// back from disk (deferred values, kept chains) or are still resident in memory (planned from their
// values).
test("chain sharing: rewrite costs do not depend on residency", () => {
  withDir("jed-chain-cost-", (dir) => {
    const path = join(dir, "cost.jed");
    const mem = memDb(PAGE_SIZE).session();
    seed(mem);
    createSeeded(path);
    const paged = openDatabase(path, { skipFsync: true });
    try {
      for (const sql of [
        "UPDATE t SET n = n + 1",
        "UPDATE t SET n = 7 WHERE body > 'a'",
        "UPDATE t SET id = id + 50 WHERE id IN (1, 2, 3)",
        "UPDATE t SET body = body || 'z' WHERE id = 51",
        "UPDATE t SET body = 'short' WHERE id = 52",
        "INSERT INTO t VALUES (14, 'more', 14)",
        "SELECT * FROM t",
        "SELECT body FROM t WHERE id = 53",
      ]) {
        assert.equal(costOf(mem, sql), costOf(paged, sql), sql);
      }
      assert.deepEqual(
        rendered(mem, "SELECT * FROM t ORDER BY id"),
        rendered(paged, "SELECT * FROM t ORDER BY id"),
      );
    } finally {
      paged.close();
    }
  });
});

// The statements of the ownership test below, in three sessions over the file (each phase starts from
// a fresh open, so its rows are read from disk). FILLER is replaced with a 900-byte incompressible
// value.
const OWNERSHIP_PHASES: string[][] = [
  [
    "BEGIN",
    "SAVEPOINT a",
    "UPDATE t SET n = n + 1000",
    "UPDATE t SET id = id + 100 WHERE id <= 3",
    "ROLLBACK TO SAVEPOINT a",
    "UPDATE t SET n = n + 1 WHERE id <= 3",
    "SAVEPOINT b",
    "UPDATE t SET id = id + 200 WHERE id = 1",
    "RELEASE SAVEPOINT b",
    "SAVEPOINT c",
    "UPDATE t SET id = id + 400 WHERE id = 2",
    "ROLLBACK TO SAVEPOINT c",
    "COMMIT",
  ],
  [
    "INSERT INTO t VALUES (2, 'ignored', 0) ON CONFLICT (id) DO UPDATE SET id = 300",
    "INSERT INTO t SELECT id + 1000, body, n FROM t WHERE id IN (201, 300)",
  ],
  [
    "DELETE FROM t WHERE id IN (201, 300)",
    "INSERT INTO t VALUES (13, 'FILLER', 13)",
    "UPDATE t SET n = n + 1",
  ],
];

// A chain belongs to exactly one record per snapshot (large-values.md §15.3), across savepoint
// rollbacks, primary-key re-keys (UPDATE and upsert DO UPDATE), and row copies: rows rolled back to a
// savepoint keep their original chains, a re-keyed row takes its chain along, and a copy gets its
// own. After each phase the live checker and the image checker (live page count, chain exclusivity)
// pass, and the rows equal an in-memory replay.
test("chain sharing: kept chains stay exclusive across savepoints and re-keys", () => {
  verifyLivePages.enabled = true;
  try {
    withDir("jed-chain-ownership-", (dir) => {
      const path = join(dir, "ownership.jed");
      const mem = memDb(PAGE_SIZE).session();
      seed(mem);
      createSeeded(path);
      const originals = rendered(mem, "SELECT body FROM t WHERE id IN (1, 2) ORDER BY id");
      const q = "SELECT id, body, n FROM t ORDER BY id";
      OWNERSHIP_PHASES.forEach((phase, i) => {
        const db = openDatabase(path, { skipFsync: true });
        const s = db.session();
        for (const raw of phase) {
          const sql = raw.replace("FILLER", fillerText(900));
          mem.execute(sql);
          s.execute(sql);
        }
        assert.deepEqual(rendered(s, q), rendered(mem, q), `phase ${i}`);
        checkLive(db);
        const txid = db.txid;
        s.close();
        db.close();
        checkImage(new Uint8Array(readFileSync(path)), txid);
      });
      const db = openDatabase(path, { skipFsync: true });
      try {
        assert.deepEqual(
          rendered(db, "SELECT body FROM t WHERE id IN (1201, 1300) ORDER BY id"),
          originals,
          "the copies kept their own chains",
        );
      } finally {
        db.close();
      }
    });
  } finally {
    verifyLivePages.enabled = false;
  }
});
