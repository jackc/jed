// A dirty leaf that keeps its page's rows undecoded, sizes itself from the page and is encoded as a
// patch of the page (pmap.ts setLeafLazy; format.ts encodeLazyLeaf) must produce the rows, query
// results and file bytes of the eager paths. The workload runs with the size-fact and encoder
// cross-checks on. Mirrors impl/go/leaf_lazy_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { setLeafPatchCheck } from "../src/format.ts";
import { leafLazyOn, setLeafLazy, setLeafSizesCheck } from "../src/pmap.ts";
import type { Snapshot } from "../src/snapshot.ts";
import { createDatabase, openDatabase, verifyLivePages } from "../src/tooling.ts";
import { query } from "./util.ts";

// lazyWorkload runs a write workload against a file-backed database, committing between statements
// so the leaves it touches are clean packed pages, and returns a transcript of every query result
// and the file's bytes after a final reopen.
function lazyWorkload(dir: string, lazy: boolean): { rows: string; file: Uint8Array } {
  setLeafLazy(lazy);
  const path = join(dir, lazy ? "lazy.jed" : "eager.jed");
  let db = createDatabase({ path, pageSize: 4096, skipFsync: true });
  let sess = db.session();
  const out: string[] = [];
  const show = (sql: string): void => {
    out.push(sql);
    for (const row of query(sess, sql)) out.push("  " + row.join("|"));
  };
  const exec = (sql: string): void => {
    sess.execute(sql);
  };
  const reopen = (): void => {
    sess.close();
    db.close();
    db = openDatabase(path, { skipFsync: true });
    sess = db.session();
  };
  // Integers, a text with few distinct values, a text with unique values, an integer array, a
  // boolean, NULLs.
  exec(`CREATE TABLE w (id bigint PRIMARY KEY, a bigint, b integer, c text, d text, e bigint[],
    f bigint, g integer, h text, k bigint, m boolean)`);
  exec("CREATE INDEX w_c ON w (c)");
  exec(`INSERT INTO w SELECT x, x * 7, CASE WHEN x % 11 = 0 THEN NULL ELSE x % 100 END,
    'status-' || (x % 5), 'row-' || x, ARRAY[x, x + 1, x % 9], 1000000 + x, x % 3,
    CASE WHEN x % 4 = 0 THEN NULL ELSE 'h' || (x % 2) END, x * 1000, x % 2 = 0
    FROM generate_series(1, 4000) AS g(x)`);
  reopen();

  // Single-row statements, each its own commit: one column of one row, of every kind.
  for (let i = 0; i < 120; i++) {
    const id = 1 + ((i * 331) % 4000);
    switch (i % 8) {
      case 0:
        exec(`UPDATE w SET a = a + 1 WHERE id = ${id}`);
        break;
      case 1:
        exec(`UPDATE w SET c = 'changed-' || (id % 3) WHERE id = ${id}`);
        break;
      case 2:
        exec(`UPDATE w SET d = 'second-' || id WHERE id = ${id}`);
        break;
      case 3:
        exec(`UPDATE w SET e = ARRAY[id, id * 1000000007] WHERE id = ${id}`);
        break;
      case 4:
        exec(`UPDATE w SET h = NULL, b = NULL WHERE id = ${id}`);
        break;
      case 5:
        exec(`UPDATE w SET g = g WHERE id = ${id}`); // every column unchanged
        break;
      case 6:
        exec(`UPDATE w SET k = 9000000000000 + id WHERE id = ${id}`);
        break;
      case 7:
        exec(`UPDATE w SET e = NULL, c = NULL WHERE id = ${id}`);
        break;
    }
  }
  show("SELECT count(*), sum(a), sum(b), sum(k), count(c), count(h) FROM w");

  // Several rows of one leaf in one statement, then the same leaves again.
  exec("UPDATE w SET a = a - 3 WHERE id BETWEEN 2000 AND 2100");
  exec("UPDATE w SET c = 'bulk' WHERE id BETWEEN 2050 AND 2150");
  exec("UPDATE w SET f = f WHERE id BETWEEN 2000 AND 2400");
  exec("UPDATE w SET d = d || '!' WHERE id % 50 = 0");
  exec("UPDATE w SET m = NOT m WHERE id % 97 = 0");
  exec("UPDATE w SET m = NULL WHERE id = 2222");

  // Deletes and inserts on leaves patched above.
  for (let i = 0; i < 40; i++) {
    const id = 1 + ((i * 331) % 4000);
    exec(`DELETE FROM w WHERE id = ${id + 1}`);
    exec(
      `INSERT INTO w VALUES (${100000 + id}, 1, NULL, 'status-1', 'new', '{}', 5, 6, 'h0', 7, true)`,
    );
  }
  exec(
    "INSERT INTO w VALUES (2, 14, 2, 'status-2', 'back', ARRAY[2], 5, 6, NULL, 7, false) ON CONFLICT (id) DO NOTHING",
  );
  show("SELECT id, a, c, d, e, h FROM w WHERE id BETWEEN 2040 AND 2060 ORDER BY id");

  // Rows that grow: enough to split a leaf, then past the inline limit (a compressed or external
  // value, which the patch encoder leaves to the full one).
  for (let i = 0; i < 30; i++) {
    exec(`UPDATE w SET d = repeat('x', 300) || id WHERE id = ${3000 + i * 7}`);
  }
  exec("UPDATE w SET d = repeat('long value ', 900) WHERE id = 3500");
  exec("UPDATE w SET a = 0 WHERE id = 3501");
  exec("UPDATE w SET d = 'short again' WHERE id = 3500");
  exec("UPDATE w SET a = 1 WHERE id = 3502");
  for (let i = 0; i < 12; i++) {
    const id = 3600 + i * 29;
    exec(`UPDATE w SET h = repeat('large-${i} ', 1200) WHERE id = ${id}`);
    exec(`UPDATE w SET a = a + 1 WHERE id = ${id + 1}`);
    exec(`UPDATE w SET k = k + 1 WHERE id = ${id}`);
    exec(`UPDATE w SET c = 'beside' WHERE id = ${id - 1}`);
    if (i % 3 === 0) {
      exec(`DELETE FROM w WHERE id = ${id + 2}`);
      exec(`UPDATE w SET h = 'small' WHERE id = ${id}`);
    }
  }

  // A transaction of several statements: later statements find the lazy leaves of earlier ones, and
  // reads inside it see them.
  exec("BEGIN");
  exec("UPDATE w SET a = -1 WHERE id = 1200");
  exec("UPDATE w SET c = 'txn' WHERE id = 1201");
  show("SELECT id, a, b, c, d, e, f, g, h, k FROM w WHERE id BETWEEN 1195 AND 1205 ORDER BY id");
  exec("DELETE FROM w WHERE id = 1202");
  exec("UPDATE w SET k = 1 WHERE id = 1203");
  show("SELECT count(*), sum(a), sum(k) FROM w WHERE id BETWEEN 1100 AND 1300");
  show("SELECT id, c FROM w WHERE c = 'txn'");
  exec("COMMIT");

  // One leaf edited by many statements of a transaction: columns measured one after another as they
  // change, a row replaced twice, a savepoint (a later statement then copies the lazy leaf), a
  // delete and an insert among the replacements, and a partial rollback.
  exec("BEGIN");
  exec("UPDATE w SET b = 77 WHERE id = 1400");
  exec("UPDATE w SET k = 5 WHERE id = 1401");
  exec("UPDATE w SET e = ARRAY[5, 6] WHERE id = 1402");
  exec("UPDATE w SET c = 'zz' WHERE id = 1402");
  exec("UPDATE w SET c = 'status-1', a = 3 WHERE id = 1402");
  exec("UPDATE w SET k = NULL WHERE id BETWEEN 1380 AND 1420");
  exec("UPDATE w SET k = 12 WHERE id = 1390");
  exec("SAVEPOINT s1");
  exec("UPDATE w SET d = 'sp' WHERE id = 1403");
  show("SELECT id, a, b, c, d, e, k FROM w WHERE id BETWEEN 1398 AND 1408 ORDER BY id");
  exec("DELETE FROM w WHERE id = 1404");
  exec("UPDATE w SET a = 9 WHERE id = 1405");
  exec("INSERT INTO w VALUES (1404, 1, 2, 'status-9', 'again', ARRAY[4], 5, 6, 'h1', 7, NULL)");
  exec("UPDATE w SET a = 10, c = 'status-9' WHERE id = 1407");
  exec("SAVEPOINT s2");
  exec("UPDATE w SET g = 5 WHERE id = 1408");
  exec("ROLLBACK TO SAVEPOINT s2");
  exec("UPDATE w SET h = 'after' WHERE id = 1406");
  show("SELECT id, a, b, c, d, e, k FROM w WHERE id BETWEEN 1398 AND 1408 ORDER BY id");
  exec("COMMIT");
  exec("BEGIN");
  exec("UPDATE w SET f = 1 WHERE id = 1500");
  exec("SAVEPOINT s1");
  exec("UPDATE w SET f = 2, d = 'x' WHERE id = 1501");
  exec("UPDATE w SET f = 3 WHERE id = 1500");
  exec("ROLLBACK TO SAVEPOINT s1");
  exec("UPDATE w SET g = 9 WHERE id = 1502");
  exec("COMMIT");

  // The same, rolled back.
  exec("BEGIN");
  exec("UPDATE w SET a = -2, d = 'gone' WHERE id BETWEEN 700 AND 760");
  exec("DELETE FROM w WHERE id BETWEEN 720 AND 740");
  exec("UPDATE w SET e = ARRAY[1, 2, 3] WHERE id = 750");
  show("SELECT count(*), sum(a) FROM w WHERE id BETWEEN 690 AND 770");
  exec("ROLLBACK");
  show("SELECT count(*), sum(a) FROM w WHERE id BETWEEN 690 AND 770");
  reopen();

  // Range deletes that merge leaves, some of them patched above.
  exec("DELETE FROM w WHERE id BETWEEN 500 AND 1150");
  exec("DELETE FROM w WHERE id % 3 = 0 AND id < 3000");
  exec("UPDATE w SET b = b + 1 WHERE id % 7 = 0");
  exec("UPDATE w SET a = id");
  for (let i = 0; i < 20; i++) {
    exec(`UPDATE w SET g = ${i} WHERE id = ${1300 + i * 13}`);
  }
  show("SELECT count(*), sum(a), sum(b), sum(g) FROM w");

  // One transaction over leaves a range delete merges: updates that widen a column and split them,
  // then replacements and inserts in the decoded leaves those merges and splits built (their unique
  // text column d is kept without a map until then; c has few distinct values).
  exec("BEGIN");
  exec("DELETE FROM w WHERE id BETWEEN 3000 AND 3400");
  exec("UPDATE w SET k = 9000000000000000 + id WHERE id % 40 = 0");
  exec("UPDATE w SET d = 'moved-' || id WHERE id BETWEEN 3390 AND 3460");
  exec(`INSERT INTO w SELECT x, 1, 2, 'status-1', 'new-' || x, ARRAY[x], 5, 6, 'h0', 7, true
    FROM generate_series(3000, 3100) AS g(x)`);
  exec("UPDATE w SET d = 'same' WHERE id BETWEEN 3420 AND 3440");
  exec("DELETE FROM w WHERE id BETWEEN 3050 AND 3700");
  show("SELECT count(*), sum(a), sum(k), count(DISTINCT d) FROM w");
  exec("COMMIT");

  reopen();
  show("SELECT * FROM w ORDER BY id");
  sess.close();
  db.close();
  checkPackedSizes(path, "w");
  return { rows: out.join("\n"), file: readFileSync(path) };
}

// checkPackedSizes compares, for every leaf of a committed table, the size facts read from the page
// (measured in full, and as the page shows them) with a measurement of the decoded rows.
function checkPackedSizes(path: string, table: string): void {
  const db = openDatabase(path, { skipFsync: true });
  try {
    const committed = (db as unknown as { core: { committed: Snapshot } }).core.committed;
    const leaves = committed.store(table).checkPackedSizes();
    assert.ok(leaves > 0, "no packed leaf was checked");
  } finally {
    db.close();
  }
}

// rootWorkload writes tables that stay a single root leaf, some rows holding values stored out of
// line or compressed. A persisted root leaf is re-faulted for lazy edits only when that cannot change
// which overflow chains the next commit writes (storage.ts refaultRootSteps).
function rootWorkload(dir: string, lazy: boolean): { rows: string; file: Uint8Array } {
  setLeafLazy(lazy);
  const path = join(dir, lazy ? "root-lazy.jed" : "root-eager.jed");
  let db = createDatabase({ path, pageSize: 512, skipFsync: true });
  let sess = db.session();
  const out: string[] = [];
  const show = (sql: string): void => {
    out.push(sql);
    for (const row of query(sess, sql)) out.push("  " + row.join("|"));
  };
  const exec = (sql: string): void => {
    sess.execute(sql);
  };
  const reopen = (): void => {
    sess.close();
    db.close();
    db = openDatabase(path, { skipFsync: true });
    sess = db.session();
  };
  // Incompressible text: stored in an overflow chain at this page size.
  const big = (seed: number, len: number): string => {
    let x = seed;
    let s = "";
    while (s.length < len) {
      x = (x * 1103515245 + 12345) & 0x7fffffff;
      s += String.fromCharCode(97 + (x % 26));
    }
    return s;
  };
  exec("CREATE TABLE r (id integer PRIMARY KEY, n bigint, body text, tag text)");
  exec("CREATE TABLE s (id integer PRIMARY KEY, n bigint, t text)");
  // A root leaf whose resident rows include an external value, then writes beside it.
  exec(`INSERT INTO r VALUES (1, 10, '${big(1, 1500)}', 'a')`);
  exec("INSERT INTO r VALUES (2, 20, 'small', 'a')");
  exec("UPDATE r SET n = n + 1 WHERE id = 2");
  exec("UPDATE r SET tag = 'b' WHERE id = 1");
  exec(`INSERT INTO r VALUES (3, 30, repeat('compressible ', 60), 'c')`);
  exec("UPDATE r SET n = n + 1 WHERE id = 3");
  exec("UPDATE r SET n = 0");
  show("SELECT id, n, length(body), tag FROM r ORDER BY id");
  // A root leaf of inline rows only: re-faulted and edited lazily between commits.
  for (let i = 1; i <= 12; i++) exec(`INSERT INTO s VALUES (${i}, ${i * 100}, 'v${i % 3}')`);
  for (let i = 1; i <= 12; i++) exec(`UPDATE s SET n = n + ${i} WHERE id = ${((i * 5) % 12) + 1}`);
  exec("UPDATE s SET t = 'changed' WHERE id = 4");
  exec("DELETE FROM s WHERE id = 5");
  exec("INSERT INTO s VALUES (40, 1, NULL)");
  exec("BEGIN");
  exec("UPDATE s SET n = -1 WHERE id = 6");
  exec("UPDATE s SET t = NULL WHERE id = 7");
  show("SELECT * FROM s ORDER BY id");
  exec("COMMIT");
  exec("UPDATE s SET n = n * 2 WHERE id = 6");
  reopen();
  // The same after a reopen: the root is a clean page with chains.
  exec("UPDATE r SET n = 5 WHERE id = 2");
  exec("UPDATE r SET n = 6 WHERE id = 1");
  exec(`UPDATE r SET body = '${big(2, 900)}' WHERE id = 2`);
  exec("UPDATE r SET tag = 'd' WHERE id = 3");
  exec("DELETE FROM r WHERE id = 1");
  exec(`INSERT INTO r VALUES (4, 40, '${big(3, 1200)}', 'e')`);
  exec("UPDATE r SET n = n + 1");
  exec("UPDATE s SET n = n + 1");
  show("SELECT id, n, length(body), tag FROM r ORDER BY id");
  reopen();
  show("SELECT id, n, body, tag FROM r ORDER BY id");
  show("SELECT * FROM s ORDER BY id");
  sess.close();
  db.close();
  return { rows: out.join("\n"), file: readFileSync(path) };
}

test("lazy root leaves match eager root leaves", () => {
  const was = leafLazyOn();
  const dir = mkdtempSync(join(tmpdir(), "jed-leaf-lazy-root-"));
  setLeafSizesCheck(true);
  setLeafPatchCheck(true);
  verifyLivePages.enabled = true;
  try {
    const lazy = rootWorkload(dir, true);
    const eager = rootWorkload(dir, false);
    assert.equal(lazy.rows, eager.rows, "query results differ between lazy and eager leaves");
    assert.ok(
      Buffer.compare(lazy.file, eager.file) === 0,
      `file bytes differ between lazy and eager leaves (${lazy.file.length} and ${eager.file.length} bytes)`,
    );
  } finally {
    setLeafLazy(was);
    setLeafSizesCheck(false);
    setLeafPatchCheck(false);
    verifyLivePages.enabled = false;
    rmSync(dir, { recursive: true, force: true });
  }
});

test("lazy leaves match eager leaves", () => {
  const was = leafLazyOn();
  const dir = mkdtempSync(join(tmpdir(), "jed-leaf-lazy-"));
  setLeafSizesCheck(true);
  setLeafPatchCheck(true);
  verifyLivePages.enabled = true;
  try {
    const lazy = lazyWorkload(dir, true);
    const eager = lazyWorkload(dir, false);
    assert.equal(lazy.rows, eager.rows, "query results differ between lazy and eager leaves");
    assert.ok(
      Buffer.compare(lazy.file, eager.file) === 0,
      `file bytes differ between lazy and eager leaves (${lazy.file.length} and ${eager.file.length} bytes)`,
    );
  } finally {
    setLeafLazy(was);
    setLeafSizesCheck(false);
    setLeafPatchCheck(false);
    verifyLivePages.enabled = false;
    rmSync(dir, { recursive: true, force: true });
  }
});
