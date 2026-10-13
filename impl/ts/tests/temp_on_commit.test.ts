// CREATE TEMP TABLE … ON COMMIT (spec/design/temp-tables.md §5.1). The SQL-visible behavior — what a
// commit empties or drops, 42P16, the grammar — is the corpus's job (ddl/temp_on_commit.test). These
// cover what the corpus cannot express: the host transaction and prepared-statement surfaces, the
// zero-main-file-write invariant, and the storage footprint under max_temp_storage_bytes. Mirrors the
// Go temp_on_commit_test.go and the Rust temp_on_commit tests.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import {
  close,
  create,
  type Engine,
  EngineError,
  execute,
  queryOutcome,
  type Session,
} from "../src/tooling.ts";
import { memDb } from "./mem_db.ts";

function rowCount(sess: Session, sql: string): number {
  const o = queryOutcome(sess, sql);
  if (o.kind !== "query") throw new Error("expected a query");
  return o.rows.length;
}

function codeOf(fn: () => unknown): string {
  try {
    fn();
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error("expected an error");
}

// A Session.update closure commits through the same path as SQL COMMIT, and a prepared SELECT over an
// ON COMMIT DROP table re-resolves after the commit drops it (temp relations are never plan-cached).
test("ON COMMIT actions run through the host transaction and prepared surfaces", () => {
  const db = memDb();
  const sess = db.session();
  sess.execute("CREATE TEMP TABLE d (id i32 PRIMARY KEY) ON COMMIT DELETE ROWS");
  sess.update((tx) => {
    tx.execute("INSERT INTO d VALUES (1), (2)");
    assert.equal([...tx.query("SELECT id FROM d")].length, 2);
    tx.execute("CREATE TEMP TABLE x (a i32) ON COMMIT DROP");
  });
  assert.equal(rowCount(sess, "SELECT id FROM d"), 0);
  assert.equal(
    codeOf(() => queryOutcome(sess, "SELECT a FROM x")),
    "42P01",
  );

  sess.execute("BEGIN");
  sess.execute("CREATE TEMP TABLE y (a i32) ON COMMIT DROP");
  sess.execute("INSERT INTO y VALUES (7)");
  const sel = sess.prepareStatement("SELECT a FROM y");
  assert.equal([...sess.queryPrepared(sel)].length, 1);
  sess.execute("COMMIT");
  assert.equal(
    codeOf(() => [...sess.queryPrepared(sel)]),
    "42P01",
  );
  db.close();
});

// The commit-time truncate and drop touch only the temp domain, so a transaction whose only effects are
// temp writes and their ON COMMIT actions leaves the main file's version and page high-water unchanged
// (temp-tables.md §2 D1, transactions.md §2.1).
test("ON COMMIT actions make zero file writes", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-oncommit-"));
  try {
    const db: Engine = create(join(dir, "oncommit.jed"), { pageSize: 256 });
    execute(db, "CREATE TABLE p (id i32 PRIMARY KEY)");
    const baseTxid = db.txid;
    const basePages = db.pageCount;

    execute(db, "CREATE TEMP TABLE d (id i32 PRIMARY KEY, pad text) ON COMMIT DELETE ROWS");
    const pad = "q".repeat(40);
    for (let i = 1; i <= 20; i++) execute(db, `INSERT INTO d VALUES (${i}, '${pad}')`);
    execute(db, "CREATE TEMP TABLE x (a serial PRIMARY KEY) ON COMMIT DROP");
    for (const sql of [
      "BEGIN",
      "CREATE TEMP TABLE y (a i32) ON COMMIT DROP",
      "INSERT INTO y VALUES (1)",
      "INSERT INTO d VALUES (1, 'x')",
      "COMMIT",
    ]) {
      execute(db, sql);
    }
    assert.equal(db.txid, baseTxid, "ON COMMIT actions advanced the file txid");
    assert.equal(db.pageCount, basePages, "ON COMMIT actions grew the file high-water");
    const o = execute(db, "SELECT id FROM d");
    assert.equal(o.kind, "query");
    if (o.kind === "query") assert.equal(o.rows.length, 0);
    close(db);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// Rows written to an ON COMMIT DELETE ROWS table count against max_temp_storage_bytes only until their
// commit empties the table; compaction reclaims the freed pages, so many autocommit inserts never reach
// the ceiling the same inserts into an ordinary temp table cross (temp_blockstore.test.ts).
test("an ON COMMIT DELETE ROWS table stays under its temp storage budget", () => {
  const db = memDb(256);
  const sess = db.session({ maxTempStorageBytes: 20 * 256 });
  sess.execute("CREATE TEMP TABLE d (id i32 PRIMARY KEY, pad text) ON COMMIT DELETE ROWS");
  const pad = "z".repeat(40);
  for (let i = 1; i <= 400; i++) sess.execute(`INSERT INTO d VALUES (${i}, 'r-${pad}')`);
  assert.equal(rowCount(sess, "SELECT id FROM d"), 0);
  db.close();
});
