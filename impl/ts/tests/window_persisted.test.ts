// A windowed aggregate whose ARGUMENT column is referenced ONLY inside the OVER call must still read
// that column from a persisted (lazily-faulted) leaf. The touched-set collector has to descend into
// each window function's args / FILTER (spec/design/window.md §5.2; large-values.md §14) — otherwise
// the lazy/masked scan leaves the operand column unfetched and the aggregate folds NULL. This is the
// on-disk read-path regression the in-memory conformance corpus cannot express (CLAUDE.md §10): it
// only surfaced through the window_running_sum benchmark, which reads a committed file. Mirrors
// impl/rust/tests/window_persisted.rs and impl/go/window_persisted_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { type Database, createDatabase, openDatabase } from "../src/tooling.ts";
import { render } from "../src/value.ts";
import type { Handle } from "./util.ts";

const PAGE_SIZE = 256;

function rowsOf(db: Handle, sql: string) {
  const o = db.execute(sql);
  if (o.kind !== "query") throw new Error("expected a query");
  return o.rows;
}

// A small-page file table whose `amount` column is not referenced outside the window functions under
// test, committed + reopened so its rows fault in lazily. 40 rows over 256-byte pages span several
// leaves, so the masked scan is genuinely exercised.
function seedPersistedWindow(path: string): Database {
  let db = createDatabase(path, { pageSize: PAGE_SIZE });
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, grp i32, amount i32)");
  for (let i = 1; i <= 40; i++) {
    db.execute(`INSERT INTO t VALUES (${i}, ${i % 3}, ${i * 10})`);
  }
  db.close();
  db = openDatabase(path);
  return db;
}

test("window: running aggregate over a persisted operand column", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-window-"));
  const path = join(dir, "running.jed");
  try {
    const db = seedPersistedWindow(path);
    // Running SUM over the default frame — amount enters the touched set ONLY through the window arg.
    const rows = rowsOf(
      db,
      "SELECT id, sum(amount) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id",
    );
    assert.equal(rows.length, 40);
    let want = 0;
    for (let i = 0; i < rows.length; i++) {
      want += (i + 1) * 10;
      assert.equal(
        render(rows[i]![1]),
        String(want),
        `row ${i + 1} running sum — operand column read as NULL?`,
      );
    }
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("window: FILTER and offset over a persisted operand column", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-window-"));
  const path = join(dir, "filter.jed");
  try {
    const db = seedPersistedWindow(path);

    // A bounded moving MAX (frame path) plus an offset function whose value is a persisted column.
    const rows = rowsOf(
      db,
      "SELECT id, max(amount) OVER (ORDER BY id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW), lag(amount) OVER (ORDER BY id) FROM t ORDER BY id LIMIT 3",
    );
    const wantMax = ["10", "20", "30"];
    const wantLag = ["NULL", "10", "20"];
    for (let i = 0; i < rows.length; i++) {
      assert.equal(render(rows[i]![1]), wantMax[i], `row ${i + 1} moving max`);
      assert.equal(render(rows[i]![2]), wantLag[i], `row ${i + 1} lag`);
    }

    // FILTER routes its predicate column through spec.filter; a running SUM of amount for ids whose
    // amount is a multiple of 20 (amounts 10,20,30,40 → only 20 and 40 pass → running NULL,20,20,60).
    const f = rowsOf(
      db,
      "SELECT id, sum(amount) FILTER (WHERE amount % 20 = 0) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) FROM t ORDER BY id LIMIT 4",
    );
    const wantF = ["NULL", "20", "20", "60"];
    for (let i = 0; i < f.length; i++) {
      assert.equal(render(f[i]![1]), wantF[i], `filter row ${i + 1}`);
    }
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
