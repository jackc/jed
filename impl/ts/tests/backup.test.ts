// Backup (spec/design/page-service.md §9; spec/pageservice/README.md §6.6) — a host act the SQL corpus
// cannot reach, so its byte-level contract lives here (CLAUDE.md §10): the compacted form is exactly
// the file compaction leaves, the page-identical form copies every reachable page at its own index
// and zeroes the rest, and the preconditions fail without writing. Backups through a page client of
// another core's owner are the process corpus's (spec/conformance/process/backup.process.toml).
// Mirrors impl/go/backup_test.go and impl/rust/tests/backup.rs.

import assert from "node:assert/strict";
import { existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";

import { backupPages } from "../src/backup.ts";
import { type CommitMeta, parseCommitMeta } from "../src/commit_manifest.ts";
import { metaPage } from "../src/format.ts";
import type { SharedPaging } from "../src/paging.ts";
import {
  backupForTesting,
  createDatabase,
  type Database,
  openDatabase,
  type Value,
} from "../src/tooling.ts";
import { errCode } from "./util.ts";

const PAGE = 4096;

const scratch = mkdtempSync(join(tmpdir(), "jed-backup-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

function tmp(name: string): string {
  return join(scratch, name);
}

// PROBE reads through the secondary index, so its cost covers index and table pages.
const PROBE = "SELECT id, length(pad), n FROM t WHERE n = 3 ORDER BY id";

// backupFixture builds a file database with dead pages, a persisted free list, overflow chains, a
// secondary index, and a GiST index, so a page-identical backup must walk every kind of page.
function backupFixture(path: string): Database {
  const db = createDatabase({ path, pageSize: PAGE, skipFsync: true });
  const s = db.session();
  for (const sql of [
    // growAndShrink (compact.test.ts): 2000 wide rows, all but 100 deleted.
    "CREATE TABLE t (id i64 PRIMARY KEY, pad text, n i64)",
    "CREATE INDEX t_n ON t (n)",
    "INSERT INTO t SELECT g, repeat('x', 300), g % 7 FROM generate_series(1, 2000) g",
    "DELETE FROM t WHERE id > 100",
    "CREATE TABLE big (id i64 PRIMARY KEY, body text)",
    "INSERT INTO big SELECT g, (SELECT string_agg(md5((g * 1000 + k)::text), '') FROM generate_series(1, 300) k) FROM generate_series(1, 20) g",
    "CREATE TABLE spans (id i64 PRIMARY KEY, r i64range)",
    "INSERT INTO spans SELECT g, i64range(g, g + 10) FROM generate_series(1, 300) g",
    "CREATE INDEX spans_r ON spans USING gist (r)",
    "UPDATE t SET n = n + 1 WHERE id % 2 = 0",
    "DELETE FROM big WHERE id > 10",
    "UPDATE t SET n = n + 1 WHERE id = 1",
  ]) {
    s.execute(sql);
  }
  s.close();
  return db;
}

function rowsAndCost(db: Database, sql: string): { rows: Value[][]; cost: bigint } {
  const s = db.session();
  try {
    const rows = s.query(sql);
    try {
      return { rows: [...rows], cost: rows.cost };
    } finally {
      rows.close();
    }
  } finally {
    s.close();
  }
}

function countRows(db: Database, table: string): bigint {
  const v = rowsAndCost(db, `SELECT count(*) FROM ${table}`).rows[0]![0]!;
  assert.ok(v.kind === "int");
  return v.int;
}

// committedImage is the database file up to the committed meta's page count (the file may carry
// preallocation past it).
function committedImage(path: string): { image: Uint8Array; meta: CommitMeta } {
  const image = new Uint8Array(readFileSync(path));
  const ps = new DataView(image.buffer, image.byteOffset).getUint32(8, false);
  let best: CommitMeta | null = null;
  for (let slot = 0; slot < 2; slot++) {
    const m = parseCommitMeta(image.subarray(slot * ps, (slot + 1) * ps), ps, 0xffff_ffff);
    if (m !== null && (best === null || m.txid > best.txid)) best = m;
  }
  assert.ok(best !== null, "the file has a valid meta");
  return { image: image.subarray(0, best.pageCount * ps), meta: best };
}

test("a compacted backup is the compacted file", () => {
  const path = tmp("compacted.jed");
  const db = backupFixture(path);
  const dest = tmp("compacted-backup.jed");
  backupForTesting(db, dest, false);
  const backup = new Uint8Array(readFileSync(dest));
  db.compact("main");
  const compacted = committedImage(path).image;
  assert.deepEqual(backup, compacted, "the compacted backup equals the compacted file");
  assert.ok(!existsSync(dest + ".jedtmp"), "the backup's temp file was left behind");
  db.close();
});

test("a page-identical backup copies reachable pages at their indices", () => {
  const path = tmp("identical.jed");
  const db = backupFixture(path);
  const dest = tmp("identical-backup.jed");
  backupForTesting(db, dest, true);
  const { image: source, meta: mt } = committedImage(path);
  assert.notEqual(mt.freeListHead, 0, "the fixture must persist a free list");
  const backup = new Uint8Array(readFileSync(dest));
  assert.equal(backup.length, source.length, "the backup spans the snapshot's page count");
  const core = (db as unknown as { core: Parameters<typeof backupPages>[0] }).core;
  const paging = (core.storage as { paging: SharedPaging }).paging;
  const reached = new Set(backupPages(core, paging, mt));
  const zero = new Uint8Array(PAGE);
  let zeroed = 0;
  for (let p = 2; p < mt.pageCount; p++) {
    const got = backup.subarray(p * PAGE, (p + 1) * PAGE);
    if (reached.has(p)) {
      assert.deepEqual(got, source.subarray(p * PAGE, (p + 1) * PAGE), `page ${p} differs`);
    } else {
      zeroed++;
      assert.deepEqual(got, zero, `unreachable page ${p} is not zero`);
    }
  }
  assert.ok(zeroed > 0, "the fixture must leave dead pages");
  const bootstrap = metaPage(
    PAGE,
    mt.txid,
    mt.rootPage,
    mt.pageCount,
    mt.freeListHead,
    mt.livePages,
  );
  assert.deepEqual(backup.subarray(0, PAGE), bootstrap, "slot 0 carries the bootstrap meta");
  assert.deepEqual(backup.subarray(PAGE, 2 * PAGE), bootstrap, "slot 1 carries the bootstrap meta");

  // The copy opens as the same database at the same version, with the same rows and costs.
  const copy = openDatabase(dest, { skipFsync: true });
  assert.equal(copy.version, mt.txid);
  for (const sql of [
    PROBE,
    "SELECT count(*), sum(length(body)) FROM big",
    "SELECT count(*) FROM spans WHERE r && i64range(100, 105)",
  ]) {
    assert.deepEqual(rowsAndCost(copy, sql), rowsAndCost(db, sql), sql);
  }
  // It keeps the source's free list and stays writable.
  copy.execute("INSERT INTO t VALUES (9001, 'tail', 1)");
  copy.close();
  db.close();
});

test("a backup does not wait for the writer or readers", () => {
  const db = backupFixture(tmp("concurrent.jed"));
  const writer = db.session();
  writer.begin(true);
  writer.execute("INSERT INTO t VALUES (5000, 'pending', 1)");
  const reader = db.readSession();
  reader.query("SELECT count(*) FROM t").close();
  for (const [i, pageIdentical] of [true, false].entries()) {
    const dest = tmp(`concurrent-backup${"ab"[i]}.jed`);
    backupForTesting(db, dest, pageIdentical);
    const copy = openDatabase(dest, { skipFsync: true });
    assert.equal(
      countRows(copy, "t"),
      100n,
      "the backup holds the committed rows, not the pending insert",
    );
    copy.close();
  }
  reader.close();
  writer.commit();
  writer.close();
  db.close();
});

test("backup preconditions", () => {
  const db = backupFixture(tmp("preconditions.jed"));
  const dest = tmp("taken.jed");
  writeFileSync(dest, "keep");
  for (const pageIdentical of [true, false]) {
    assert.equal(
      errCode(() => backupForTesting(db, dest, pageIdentical)),
      "58P02",
      "an existing destination",
    );
  }
  assert.equal(readFileSync(dest, "utf8"), "keep", "a refused backup overwrote its destination");
  db.close();

  const mem = createDatabase({ pageSize: PAGE });
  mem.execute("CREATE TABLE m (id i64 PRIMARY KEY, v text)");
  mem.execute("INSERT INTO m SELECT g, g::text FROM generate_series(1, 50) g");
  assert.equal(
    errCode(() => backupForTesting(mem, tmp("mem.jed"), true)),
    "0A000",
    "a page-identical backup of an in-memory database",
  );
  // The compacted form copies an in-memory database to a file (SQLite's VACUUM INTO from :memory:).
  const memDest = tmp("mem-compacted.jed");
  backupForTesting(mem, memDest, false);
  const copy = openDatabase(memDest, { skipFsync: true });
  assert.equal(countRows(copy, "m"), 50n);
  copy.close();
  mem.close();
});
