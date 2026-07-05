// Phase 7: the formal host API (spec/design/api.md) — open/create/commit/close a database file,
// prepare/execute/query, the Rows cursor, and the structured-error surface. Files are written
// under a fresh mkdtemp dir, never the repo tree.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  begin,
  close,
  commit,
  create,
  Engine,
  EngineError,
  execute,
  executePrepared,
  intValue,
  open,
  prepare,
  query,
  queryPrepared,
  rollback,
  update,
  view,
} from "../src/tooling.ts";
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
    execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
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
    execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
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
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
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
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32)");
  const insert = prepare(db, "INSERT INTO t VALUES ($1, $2)");
  executePrepared(db, insert, [intValue(1n), intValue(100n)]);
  executePrepared(db, insert, [intValue(2n), intValue(200n)]);

  const sel = prepare(db, "SELECT id, v FROM t WHERE v = $1");
  const r = queryPrepared(db, sel, [intValue(200n)]);
  assert.deepStrictEqual(r.columnNames, ["id", "v"]);
  const collected: Value[][] = [];
  for (const row of r) collected.push(row);
  assert.equal(collected.length, 1);
  assert.equal(collected[0]![0]!.kind === "int" ? collected[0]![0]!.int : -1n, 2n);
  assert.ok(r.cost >= 0n);
});

test("one-shot query iterates rows", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  execute(db, "INSERT INTO t VALUES (1), (2), (3)");
  const ids: bigint[] = [];
  for (const row of query(db, "SELECT id FROM t")) {
    if (row[0]!.kind === "int") ids.push(row[0]!.int);
  }
  assert.deepStrictEqual(ids, [1n, 2n, 3n]);
});

test("query on a non-query statement is total (no rows, statement still runs)", () => {
  // `query` is the one total seam (spec/design/api.md §11): a non-query statement is observably a Rows
  // with no output columns — NOT a "use execute" throw (the removed effect-then-error surprise). The
  // DDL still takes effect, and a write exposes its command tag via rowsAffected.
  const db = new Engine();
  const ddl = query(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  assert.equal(ddl.columnNames.length, 0);
  assert.deepStrictEqual([...ddl], []);
  assert.equal(ddl.rowsAffected, null); // DDL carries no row count
  ddl.close();
  const ins = query(db, "INSERT INTO t VALUES (1), (2)");
  assert.deepStrictEqual([...ins], []);
  assert.equal(ins.rowsAffected, 2);
  ins.close();
});

test("errors surface with SQLSTATE", () => {
  const db = new Engine();
  let code = "";
  try {
    prepare(db, "SELCT 1");
  } catch (e) {
    if (e instanceof EngineError) code = e.code();
  }
  assert.equal(code, "42601");
});

test("commit on in-memory db is a no-op success", () => {
  const db = new Engine();
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  commit(db); // no path -> no-op, not an error
  assert.equal(db.txid, 0n);
  assert.equal(db.path, null);
});

test("rowsAffected reports DML counts", () => {
  // The affected-row count (api.md §4): INSERT/UPDATE/DELETE without RETURNING report
  // how many rows they touched (PostgreSQL's command-tag count); a DML statement that
  // matched nothing reports 0; DDL and transaction control report null; DML with
  // RETURNING is a query outcome (its row count is the result's length).
  const db = new Engine();
  const affected = (sql: string): number | null => {
    const out = execute(db, sql);
    assert.equal(out.kind, "statement", sql);
    return out.kind === "statement" ? out.rowsAffected : null;
  };

  assert.equal(affected("CREATE TABLE t (id i32 PRIMARY KEY, v i32)"), null);
  assert.equal(affected("INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)"), 3);
  assert.equal(affected("UPDATE t SET v = v + 1 WHERE id <= 2"), 2);
  assert.equal(affected("DELETE FROM t WHERE id = 3"), 1);
  assert.equal(affected("DELETE FROM t WHERE id = 99"), 0);
  assert.equal(affected("BEGIN"), null);
  assert.equal(affected("COMMIT"), null);

  // INSERT ... SELECT counts the inserted rows; DML with RETURNING is a Query.
  execute(db, "CREATE TABLE dst (id i32 PRIMARY KEY)");
  assert.equal(affected("INSERT INTO dst SELECT id FROM t"), 2);
  const out = execute(db, "DELETE FROM dst RETURNING id");
  assert.equal(out.kind, "query");
  if (out.kind === "query") assert.equal(out.rows.length, 2);
});

test("open read-only blocks writes and never touches the file", () => {
  // Read-only open (api.md §2.1): the handle behaves like PostgreSQL hot standby — every
  // transaction defaults to READ ONLY, an explicit READ WRITE request and any write are
  // 25006, and the file bytes are never touched.
  const dir = tmpDir();
  try {
    const path = join(dir, "readonly.jed");
    let db = create(path);
    execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
    execute(db, "INSERT INTO t VALUES (1)");
    close(db);
    const before = readFileSync(path);

    db = open(path, { readOnly: true });
    assert.equal(db.readOnly, true);
    const wantCode = (fn: () => void, code: string) => {
      try {
        fn();
      } catch (e) {
        if (e instanceof EngineError) {
          assert.equal(e.code(), code);
          return;
        }
        throw e;
      }
      assert.fail(`expected ${code}`);
    };

    // Reads work — bare and inside an explicit block (plain BEGIN defaults to READ ONLY here).
    const out = execute(db, "SELECT id FROM t");
    assert.equal(out.kind === "query" && out.rows.length, 1);
    execute(db, "BEGIN");
    execute(db, "SELECT id FROM t");
    execute(db, "COMMIT");

    // Autocommit writes are 25006 (the implicit transaction is read-only)...
    wantCode(() => execute(db, "INSERT INTO t VALUES (2)"), "25006");
    // ...as are writes inside a block (which then poisons, like any in-block error)...
    execute(db, "BEGIN");
    wantCode(() => execute(db, "DELETE FROM t"), "25006");
    wantCode(() => execute(db, "SELECT id FROM t"), "25P02");
    execute(db, "ROLLBACK");
    // ...and an explicit READ WRITE request, via SQL or the host API.
    wantCode(() => execute(db, "BEGIN READ WRITE"), "25006");
    wantCode(() => begin(db, true), "25006");
    view(db, (tx) => tx.query("SELECT id FROM t"));
    wantCode(() => update(db, (tx) => tx.execute("DELETE FROM t")), "25006");
    close(db);

    // The file is byte-identical after the whole read-only session.
    assert.deepStrictEqual(readFileSync(path), before);

    // A normal reopen is writable again.
    db = open(path);
    assert.equal(db.readOnly, false);
    execute(db, "INSERT INTO t VALUES (2)");
    close(db);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
