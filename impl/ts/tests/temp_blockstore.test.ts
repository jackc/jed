// Phase B — session-local temp tables ride a per-domain MemoryBlockStore pager (spec/design/
// temp-tables.md §6, bplus-reshape.md), instead of a fully-resident decoded tree. Per-core tests for
// what the corpus cannot express: the internal page-based footprint / bound, the compact (packed)
// residency, and the zero-main-file-write invariant. The SQL-visible temp behavior (rows, errors,
// 54P03) is the corpus's job (ddl/temp_table.test, resource/temp_budget.test) — these assert the storage
// internals.

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
  type Session,
  type Value,
} from "../src/tooling.ts";
import { memDb } from "./mem_db.ts";

// tempStorageOf reaches the session's private temp domain (the Go test's sess.engine.tempStorage) — a
// white-box cast, since the domain is an internal storage engine with no public surface.
function tempStorageOf(sess: Session): { pageCount: number } | null {
  return (sess as unknown as { engine: { tempStorage: { pageCount: number } | null } }).engine
    .tempStorage;
}

function rowsOf(sess: Session, sql: string): Value[][] {
  const o = sess.execute(sql);
  if (o.kind !== "query") throw new Error("expected a query");
  return o.rows;
}

function textAt(rows: Value[][], i: number): string {
  const v = rows[i]![0]!;
  if (v.kind !== "text") throw new Error("expected a text value");
  return v.text;
}

// TestSessionLocalTempRunsThroughBlockStore proves a session-local temp table is demand-paged over its
// own in-RAM MemoryBlockStore: rows read back correctly (faulting demoted leaves through the temp pool),
// and heavy churn stays bounded (within-session compaction reclaims copy-on-write orphans — no leak).
test("session-local temp runs through its MemoryBlockStore and churn stays bounded", () => {
  const db = memDb(256);
  const sess = db.session();
  sess.execute("CREATE TEMP TABLE lt (id i32 PRIMARY KEY, pad text)");
  const base = "x".repeat(40);
  for (let i = 1; i <= 60; i++) {
    // 60 rows at page 256 → a multi-level tree with demoted leaves.
    sess.execute(`INSERT INTO lt VALUES (${i}, 'r${String(i).padStart(2, "0")}-${base}')`);
  }
  const temp = tempStorageOf(sess);
  assert.ok(temp !== null, "session-local temp DDL should have created a temp storage domain");

  // Reads fault demoted leaves back through the temp pool.
  const got = rowsOf(sess, "SELECT pad FROM lt WHERE id = 42");
  assert.equal(got.length, 1);
  assert.equal(textAt(got, 0), `r42-${base}`);
  assert.equal(rowsOf(sess, "SELECT id FROM lt").length, 60);

  // Churn one row 400×; the high-water plateaus (compaction), it does not grow ~linearly.
  const pad = "y".repeat(40);
  for (let k = 0; k < 400; k++) {
    sess.execute(`UPDATE lt SET pad = 'u${k}-${pad}' WHERE id = 30`);
  }
  const pc = tempStorageOf(sess)!.pageCount;
  assert.ok(pc <= 200, `temp churn not bounded by compaction: pageCount=${pc} after 400 updates`);

  const after = rowsOf(sess, "SELECT pad FROM lt WHERE id = 30");
  assert.equal(after.length, 1);
  assert.equal(textAt(after, 0), `u399-${pad}`);
  assert.equal(rowsOf(sess, "SELECT id FROM lt").length, 60);
  db.close();
});

// TestSessionLocalTempPageBudgetBoundsMultiLeaf is the bug the page-based budget (Design decision 3)
// closes: once temp is paged, its leaves demote to OnDisk, so a record-byte walk sees only the one leaf a
// write touches and undercounts a multi-leaf temp table — the §13 bound would never fire. The page-based
// measure (committed pageCount × page_size) counts every allocated page, so a growing temp table hits
// 54P03 deterministically.
test("a multi-leaf temp table past its page budget aborts 54P03", () => {
  const db = memDb(256);
  // ~20 pages of budget: a single leaf (≤ ~240 record bytes) is far under it, so a record-byte measure
  // would never abort; the page footprint crosses it as the tree grows past ~20 pages.
  const sess = db.session({ tempBuffers: 20 * 256 });
  sess.execute("CREATE TEMP TABLE lt (id i32 PRIMARY KEY, pad text)");
  const pad = "z".repeat(40);
  let aborted = false;
  for (let i = 1; i <= 400 && !aborted; i++) {
    try {
      sess.execute(`INSERT INTO lt VALUES (${i}, 'r-${pad}')`);
    } catch (e) {
      if (!(e instanceof EngineError) || e.code() !== "54P03") {
        throw new Error(`insert ${i}: want 54P03, got ${e}`);
      }
      aborted = true;
    }
  }
  assert.ok(
    aborted,
    "a multi-leaf temp table past its page budget should abort 54P03; it never did (undercount bug)",
  );
  db.close();
});

// TestSessionLocalTempZeroFileWrites confirms the invariant that survives the flip: session-local temp
// writes touch only the temp MemoryBlockStore, never the main database file (temp-tables.md §2, D1). The
// file's committed version and page high-water are unchanged across a burst of temp DDL/DML. Uses the
// bare-Engine autocommit path (tooling create/execute), which — like the Go test — correctly skips the
// main persist for a pure-temp commit.
test("session-local temp makes zero file writes", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-temp-"));
  try {
    const path = join(dir, "ztemp.jed");
    const db: Engine = create(path, { pageSize: 256 });
    execute(db, "CREATE TABLE p (id i32 PRIMARY KEY)");
    execute(db, "INSERT INTO p VALUES (1)");
    const baseTxid = db.txid;
    const basePages = db.pageCount;

    execute(db, "CREATE TEMP TABLE lt (id i32 PRIMARY KEY, pad text)");
    const pad = "q".repeat(40);
    for (let i = 1; i <= 40; i++) {
      execute(db, `INSERT INTO lt VALUES (${i}, '${pad}')`);
    }
    for (let k = 0; k < 40; k++) {
      execute(db, `UPDATE lt SET pad = 'u${k}' WHERE id = 20`);
    }
    assert.equal(db.txid, baseTxid, "session-local temp writes advanced the file txid");
    assert.equal(db.pageCount, basePages, "session-local temp writes grew the file high-water");

    // The temp data is nonetheless present and correct (it lives in the temp store).
    const o = execute(db, "SELECT id FROM lt");
    assert.equal(o.kind, "query");
    if (o.kind === "query") assert.equal(o.rows.length, 40);
    close(db);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
