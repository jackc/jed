// Host-API surface of the committed-storage limit (spec/design/memory.md §8): the create, open, and
// attach options, the runtime setter and the storageBytes gauge, the 42704 rejections, the multi-root
// precheck, the reader watermark's hold on forced compaction (in-memory, Q4a), and the file form's
// live-page measure (§8.7). Trip points themselves are pinned in
// spec/conformance/suites/resource/storage_memory.test and storage_file.test. Mirrors
// impl/rust/tests/storage_memory.rs.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  attachFile,
  attachMemory,
  createDatabase,
  type Database,
  EngineError,
  openDatabase,
  queryOutcome,
  render,
  verifyLivePages,
} from "../src/tooling.ts";

const PAGE = 8192n;

const is = (code: string) => (e: unknown) => e instanceof EngineError && e.code() === code;

function batch(lo: number, n: number): string {
  return `INSERT INTO t SELECT g, repeat('x', 1000) FROM generate_series(${lo}, ${lo + n - 1}) g`;
}

test("storage limit options and gauge", () => {
  let db = createDatabase();
  const fresh = db.storageBytes("main");
  assert.ok(fresh > 0n && fresh % PAGE === 0n, "a fresh image holds its meta and catalog pages");
  let s = db.session();
  s.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)");
  // Unlimited by default: a large insert commits.
  s.execute(batch(0, 200));
  const full = db.storageBytes("MAIN");
  assert.ok(full > fresh);
  // Lowering the limit below the current size blocks growth, not the database.
  db.setMaxStorageBytes("main", fresh);
  assert.throws(() => s.execute(batch(200, 200)), is("54P06"));
  assert.equal(db.storageBytes("main"), full, "a rejected commit writes nothing");
  db.setMaxStorageBytes("main", -1n);
  s.execute(batch(200, 200));
  s.close();
  db.close();

  // The create option limits from the first commit.
  db = createDatabase({ maxStorageBytes: 8n * PAGE });
  s = db.session();
  s.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)");
  assert.throws(() => s.execute(batch(0, 200)), is("54P06"));

  // An in-memory attachment's limit, set on attach or later.
  db.attach("aux", attachMemory({ maxStorageBytes: PAGE }), false);
  assert.throws(() => s.execute("CREATE TABLE aux.a (id i32 PRIMARY KEY, v text)"), is("54P06"));
  db.setMaxStorageBytes("aux", 0n);
  s.execute("CREATE TABLE aux.a (id i32 PRIMARY KEY, v text)");
  assert.ok(db.storageBytes("aux") > PAGE);

  // Unknown databases and the session-local temp domain are not attachments.
  assert.throws(() => db.storageBytes("nope"), is("42704"));
  assert.throws(() => db.setMaxStorageBytes("temp", PAGE), is("42704"));
  s.close();
  db.close();
});

// fileDb creates a fresh file-backed database with the live-page self-check on (memory.md §8.7): every
// commit in these tests also recounts its live pages by reachability and throws on a mismatch.
function fileDb(dir: string, tag: string, maxStorageBytes = 0n): { db: Database; path: string } {
  verifyLivePages.enabled = true;
  const path = join(dir, `${tag}.jed`);
  return { db: createDatabase({ path, skipFsync: true, maxStorageBytes }), path };
}

function withDir(body: (dir: string) => void): void {
  const dir = mkdtempSync(join(tmpdir(), "jed-storage-file-"));
  try {
    body(dir);
  } finally {
    verifyLivePages.enabled = false;
    rmSync(dir, { recursive: true, force: true });
  }
}

test("file storage limit measures live pages", () => {
  withDir((dir) => {
    let { db, path } = fileDb(dir, "live");
    // A fresh file holds one live page: its catalog.
    assert.equal(db.storageBytes("main"), PAGE);
    let s = db.session();
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)");
    s.execute(batch(0, 200));
    const full = db.storageBytes("main");
    assert.ok(full > PAGE && full % PAGE === 0n);
    // A delete lowers the measure at once; the file keeps its high-water and free pages.
    const highWater = db.pageCount;
    s.execute("DELETE FROM t WHERE id >= 100");
    const half = db.storageBytes("main");
    assert.ok(half < full);
    assert.ok(db.pageCount >= highWater);

    // The setter limits the file from the next commit; a rejected commit writes nothing.
    db.setMaxStorageBytes("main", half);
    assert.throws(() => s.execute(batch(100, 100)), is("54P06"));
    assert.equal(db.storageBytes("main"), half);
    // A commit that does not grow the live count is admitted at the limit.
    s.execute("UPDATE t SET v = 'short' WHERE id < 10");
    assert.equal(s.get("SELECT count(*) AS n FROM t")!.n, 100n);
    const measured = db.storageBytes("main");
    s.close();
    db.close();

    // The count is persisted: a reopen reports it without walking the file, and the open option sets
    // the limit.
    db = openDatabase(path, { skipFsync: true, maxStorageBytes: measured });
    assert.equal(db.storageBytes("main"), measured);
    s = db.session();
    assert.throws(() => s.execute(batch(100, 100)), is("54P06"));
    db.setMaxStorageBytes("main", 0n);
    s.execute(batch(100, 100));
    s.close();
    db.close();
  });
});

test("file storage limit counts every live structure", () => {
  // Indexes (B-tree, GIN, GiST), overflow chains, a drop, and a host compaction, each checked by the
  // reachability recount.
  withDir((dir) => {
    const { db } = fileDb(dir, "structures");
    const s = db.session();
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY, k i32, v text, a i32[], r i32range)");
    s.execute("CREATE INDEX t_k ON t (k)");
    s.execute("CREATE INDEX t_a ON t USING gin (a)");
    s.execute("CREATE INDEX t_r ON t USING gist (r)");
    // An incompressible value larger than a record spills into its own overflow chain per row.
    let x = 0x4a454442;
    let filler = "";
    for (let i = 0; i < 5000; i++) {
      x = (x ^ (x << 13)) >>> 0;
      x = (x ^ (x >>> 17)) >>> 0;
      x = (x ^ (x << 5)) >>> 0;
      filler += String.fromCharCode(65 + (x % 26));
    }
    s.run("INSERT INTO t VALUES (0, 0, $1, '{0,1}', '[0,5)')", filler);
    const rows: string[] = [];
    for (let g = 1; g <= 300; g++) rows.push(`(${g}, '{${g},${g + 1}}', '[${g},${g + 5})')`);
    s.execute(`INSERT INTO t (id, a, r) VALUES ${rows.join(", ")}`);
    s.execute("UPDATE t SET k = id % 7, v = (SELECT v FROM t WHERE id = 0)");
    const full = db.storageBytes("main");
    s.execute("UPDATE t SET v = left(v, 10) WHERE id % 3 = 0");
    s.execute("DELETE FROM t WHERE id > 150");
    assert.ok(db.storageBytes("main") < full);
    s.execute("CREATE TABLE u (id i32 PRIMARY KEY, v text)");
    s.execute("INSERT INTO u SELECT id, v FROM t");
    s.execute("DROP TABLE t");
    const before = db.storageBytes("main");
    db.compact("main");
    // Compaction renumbers pages but keeps every live one.
    assert.equal(db.storageBytes("main"), before);
    s.execute("DROP TABLE u");
    assert.equal(db.storageBytes("main"), PAGE);
    s.close();
    db.close();
  });
});

test("file attachment storage limit", () => {
  withDir((dir) => {
    const { db: file, path } = fileDb(dir, "attach");
    file.execute("CREATE TABLE a (id i32 PRIMARY KEY, v text)");
    file.close();
    const host = createDatabase();
    host.attach("f", attachFile(path, { maxStorageBytes: 4n * PAGE }), false);
    const s = host.session();
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)");
    const main = host.storageBytes("main");
    const aux = host.storageBytes("f");
    // A multi-root commit rejected in the file attachment publishes neither database.
    s.execute("BEGIN");
    s.execute(batch(0, 10));
    s.execute("INSERT INTO f.a SELECT g, repeat('y', 1000) FROM generate_series(1, 100) g");
    assert.throws(() => s.execute("COMMIT"), is("54P06"));
    assert.equal(host.storageBytes("main"), main);
    assert.equal(host.storageBytes("f"), aux);
    // queryOutcome closes its cursor, so no reader pin outlives it (the detach below needs none).
    const out = queryOutcome(s, "SELECT (SELECT count(*) FROM t), (SELECT count(*) FROM f.a)");
    assert.equal(out.kind === "query" ? out.rows.map((r) => r.map(render)).join() : "", "0,0");
    // A small write fits.
    s.execute("INSERT INTO f.a VALUES (1, 'z')");
    assert.ok(host.storageBytes("f") <= 4n * PAGE);
    s.close();
    host.detach("f");
    host.close();
  });
});

test("multi-root rejection packs no attachment", () => {
  const db = createDatabase();
  db.attach("aux", attachMemory(), false);
  const s = db.session();
  s.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)");
  s.execute("CREATE TABLE aux.a (id i32 PRIMARY KEY, v text)");
  const main = db.storageBytes("main");
  const aux = db.storageBytes("aux");
  db.setMaxStorageBytes("main", main);
  s.execute("BEGIN");
  s.execute("INSERT INTO aux.a SELECT g, repeat('y', 1000) FROM generate_series(1, 50) g");
  s.execute(batch(0, 50));
  assert.throws(() => s.execute("COMMIT"), is("54P06"));
  // Main is rejected after the attachment's precheck, before any domain packs a page.
  assert.equal(db.storageBytes("aux"), aux);
  assert.equal(db.storageBytes("main"), main);
  assert.equal(s.get("SELECT count(*) AS n FROM aux.a")!.n, 0n);
  s.close();
  db.close();
});

test("pinned reader blocks forced compaction", () => {
  const db = createDatabase();
  const w = db.session();
  w.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)");
  w.execute(batch(0, 100));
  db.setMaxStorageBytes("main", db.storageBytes("main"));
  // A reader pinned at the current version keeps every page a compaction would keep: the delete
  // (admitted over the limit) orphans pages, and the next commit's forced compaction is allowed only
  // once no reader pins a version older than the committed one.
  const r = db.readSession();
  w.execute("DELETE FROM t WHERE id >= 50");
  assert.throws(() => w.execute(batch(50, 50)), is("54P06"));
  r.close();
  w.execute(batch(50, 50));
  w.close();
  db.close();
});
