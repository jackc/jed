// Host-API surface of the committed-storage limit (spec/design/memory.md §8, Q4a): the create and
// attach options, the runtime setter and the storageBytes gauge, the 0A000/42704 rejections, the
// multi-root precheck, and the reader watermark's hold on forced compaction. Trip points themselves are
// pinned in spec/conformance/suites/resource/storage_memory.test. Mirrors
// impl/rust/tests/storage_memory.rs.

import assert from "node:assert/strict";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { attachFile, attachMemory, createDatabase, EngineError } from "../src/tooling.ts";

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

test("storage limit rejects file backings", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-storage-memory-"));
  const path = join(dir, "create.jed");
  try {
    assert.throws(() => createDatabase({ path, maxStorageBytes: PAGE }), is("0A000"));
    assert.ok(!existsSync(path), "a rejected create makes no file");

    const db = createDatabase({ path, skipFsync: true });
    assert.throws(() => db.setMaxStorageBytes("main", PAGE), is("0A000"));
    db.setMaxStorageBytes("main", 0n);
    assert.ok(db.storageBytes("main") > 0n);
    db.close();

    const host = createDatabase();
    assert.throws(
      () => host.attach("f", { ...attachFile(path), maxStorageBytes: PAGE }, false),
      is("0A000"),
    );
    host.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
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
