// Phase 7: the formal host API (spec/design/api.md) — open/create/commit/close a database file,
// prepare/execute/query, the Rows cursor, and the structured-error surface. Files are written
// under a fresh mkdtemp dir, never the repo tree.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  close,
  commit,
  create,
  Database,
  EngineError,
  execute,
  intValue,
  open,
  prepare,
  query,
  rollback,
} from "../src/lib.ts";
import type { Value } from "../src/lib.ts";

function tmpDir(): string {
  return mkdtempSync(join(tmpdir(), "jed-"));
}

test("create → commit → reopen round-trips", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "round_trip.jed");
    const db = create(path);
    assert.equal(db.txid, 1n); // the initial empty image is committed at create
    execute(db, "CREATE TABLE t (id int32 PRIMARY KEY, v int32)");
    execute(db, "INSERT INTO t VALUES (1, 10), (2, 20)");
    commit(db);
    const afterCommit = db.txid;
    close(db);

    const db2 = open(path);
    assert.equal(db2.txid, afterCommit);
    const o = execute(db2, "SELECT id, v FROM t");
    assert.equal(o.kind, "query");
    if (o.kind === "query") assert.equal(o.rows.length, 2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("open missing file is 58P01", () => {
  const dir = tmpDir();
  try {
    let code = "";
    try {
      open(join(dir, "nope.jed"));
    } catch (e) {
      if (e instanceof EngineError) code = e.code();
    }
    assert.equal(code, "58P01");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("create over existing file is 58P02", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "here.jed");
    create(path);
    let code = "";
    try {
      create(path);
    } catch (e) {
      if (e instanceof EngineError) code = e.code();
    }
    assert.equal(code, "58P02");
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("create with custom page size round-trips", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "page256.jed");
    const db = create(path, { pageSize: 256 });
    assert.equal(db.pageSize, 256);
    close(db);
    const bytes = readFileSync(path);
    const dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    assert.equal(dv.getUint32(8, false), 256);
    assert.equal(open(path).pageSize, 256);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("autocommit persists each write across close", () => {
  // jed autocommits (spec/design/transactions.md §4.1): a write is durable as soon as it
  // succeeds, so it survives a close with no explicit commit — the opposite of the original
  // "no autocommit" model this test used to assert.
  const dir = tmpDir();
  try {
    const path = join(dir, "autocommit.jed");
    const db = create(path);
    execute(db, "CREATE TABLE t (id int32 PRIMARY KEY)");
    execute(db, "INSERT INTO t VALUES (1)"); // autocommitted, no explicit commit
    close(db);

    const db2 = open(path);
    const o = execute(db2, "SELECT id FROM t");
    assert.equal(o.kind, "query");
    if (o.kind === "query") {
      assert.equal(o.rows.length, 1);
      assert.deepEqual(o.rows[0][0], intValue(1n));
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("commit and rollback are no-ops under autocommit", () => {
  // With no explicit transaction open, both are lenient no-op successes (transactions.md §4.2).
  const db = new Database();
  execute(db, "CREATE TABLE t (id int32 PRIMARY KEY)");
  execute(db, "INSERT INTO t VALUES (1)");
  commit(db);
  rollback(db); // does NOT undo the autocommitted insert
  const o = execute(db, "SELECT id FROM t");
  assert.equal(o.kind, "query");
  if (o.kind === "query") {
    assert.equal(o.rows.length, 1);
    assert.deepEqual(o.rows[0][0], intValue(1n));
  }
});

test("prepare → execute → query with params, iterating rows", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id int32 PRIMARY KEY, v int32)");
  const insert = prepare(db, "INSERT INTO t VALUES ($1, $2)");
  insert.execute([intValue(1n), intValue(100n)]);
  insert.execute([intValue(2n), intValue(200n)]);

  const sel = prepare(db, "SELECT id, v FROM t WHERE v = $1");
  const r = sel.query([intValue(200n)]);
  assert.deepStrictEqual(r.columnNames, ["id", "v"]);
  const collected: Value[][] = [];
  for (const row of r) collected.push(row);
  assert.equal(collected.length, 1);
  assert.equal(collected[0]![0]!.kind === "int" ? collected[0]![0]!.int : -1n, 2n);
  assert.ok(r.cost >= 0n);
});

test("one-shot query iterates rows", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id int32 PRIMARY KEY)");
  execute(db, "INSERT INTO t VALUES (1), (2), (3)");
  const ids: bigint[] = [];
  for (const row of query(db, "SELECT id FROM t")) {
    if (row[0]!.kind === "int") ids.push(row[0]!.int);
  }
  assert.deepStrictEqual(ids, [1n, 2n, 3n]);
});

test("query on a non-query statement throws", () => {
  const db = new Database();
  assert.throws(() => query(db, "CREATE TABLE t (id int32 PRIMARY KEY)"));
});

test("errors surface with SQLSTATE", () => {
  const db = new Database();
  let code = "";
  try {
    prepare(db, "SELCT 1");
  } catch (e) {
    if (e instanceof EngineError) code = e.code();
  }
  assert.equal(code, "42601");
});

test("commit on in-memory db is a no-op success", () => {
  const db = new Database();
  execute(db, "CREATE TABLE t (id int32 PRIMARY KEY)");
  commit(db); // no path -> no-op, not an error
  assert.equal(db.txid, 0n);
  assert.equal(db.path, null);
});

test("tableNames lists tables sorted by lowercased name, excluding indexes", () => {
  // The catalog-read surface (api.md §6): canonical names, sorted ascending by
  // lowercased name; secondary indexes are relations but not tables.
  const db = new Database();
  assert.deepStrictEqual(db.tableNames(), []);
  execute(db, "CREATE TABLE Zed (id int32 PRIMARY KEY, v int32)");
  execute(db, "CREATE TABLE apple (id int32 PRIMARY KEY)");
  execute(db, "CREATE INDEX zed_v_idx ON Zed (v)");
  // Sorted by LOWERCASED name (apple < zed), returning the canonical spelling (`Zed`).
  assert.deepStrictEqual(db.tableNames(), ["apple", "Zed"]);
  // The visible snapshot includes an open transaction's working set.
  execute(db, "BEGIN");
  execute(db, "CREATE TABLE mid (id int32 PRIMARY KEY)");
  assert.deepStrictEqual(db.tableNames(), ["apple", "mid", "Zed"]);
  execute(db, "ROLLBACK");
  assert.deepStrictEqual(db.tableNames(), ["apple", "Zed"]);
});
