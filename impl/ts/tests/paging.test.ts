// Demand paging (P6.4b, spec/design/pager.md §1/§4): a file-backed database with many leaf pages,
// reopened with a tiny buffer-pool budget, still scans and mutates correctly while keeping only a
// bounded number of leaves resident — the residency win, exercised end to end. Files are written under
// a fresh mkdtemp dir, never the repo tree.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { close, create, execute } from "../src/lib.ts";
import { openWithCapacity, residentLeaves } from "../src/file.ts";
import type { Value } from "../src/lib.ts";

function intOf(v: Value): bigint {
  if (v.kind !== "int") throw new Error("expected an int value");
  return v.int;
}

test("demand paging: scans + mutates correctly with bounded residency", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-paging-"));
  try {
    const path = join(dir, "paging.jed");
    const n = 600;
    const CAP = 3;

    // Build a multi-level tree at a small page size, so a few hundred rows span many pages.
    const db = create(path, { pageSize: 256 });
    execute(db, "CREATE TABLE t (k int32 PRIMARY KEY, v int32)");
    execute(db, "BEGIN"); // one commit, not 600
    for (let k = 0; k < n; k++) execute(db, `INSERT INTO t VALUES (${k}, ${k * 2})`);
    execute(db, "COMMIT");
    close(db);

    // Reopen demand-paged with a 3-leaf budget.
    let db2 = openWithCapacity(path, CAP);
    // A PK table's skeleton load faults no leaves (it reads them only to count rows, uncached), so the
    // pool starts empty — and the file holds many pages.
    assert.equal(residentLeaves(db2), 0, "skeleton load caches no leaf");
    assert.ok(db2.pageCount > CAP * 5, `file has many more pages (${db2.pageCount}) than the budget`);

    // A full scan faults every leaf through the bounded pool: results exact, residency bounded.
    const rows = db2.rowsInKeyOrder("t");
    assert.equal(rows.length, n);
    for (let i = 0; i < rows.length; i++) {
      assert.equal(intOf(rows[i]![0]!), BigInt(i));
      assert.equal(intOf(rows[i]![1]!), BigInt(i * 2));
    }
    assert.ok(residentLeaves(db2) <= CAP, `resident leaves ${residentLeaves(db2)} exceed budget ${CAP}`);
    close(db2);

    // Mutate through the pool (each statement faults the leaf it touches), reopen, verify.
    db2 = openWithCapacity(path, CAP);
    execute(db2, "DELETE FROM t WHERE k = 100");
    execute(db2, "UPDATE t SET v = 999 WHERE k = 200");
    execute(db2, "INSERT INTO t VALUES (600, 1200)");
    assert.ok(residentLeaves(db2) <= CAP, "mutations keep residency bounded");
    close(db2); // autocommit already persisted each statement

    db2 = openWithCapacity(path, CAP);
    const after = db2.rowsInKeyOrder("t");
    assert.equal(after.length, n, "one deleted, one inserted");
    for (const r of after) {
      const k = intOf(r[0]!);
      assert.notEqual(k, 100n, "k=100 was deleted");
      if (k === 200n) assert.equal(intOf(r[1]!), 999n, "k=200 was updated");
      if (k === 600n) assert.equal(intOf(r[1]!), 1200n, "k=600 was inserted");
    }
    close(db2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
