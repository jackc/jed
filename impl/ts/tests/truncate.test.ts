// In-place trailing-free truncation (spec/fileformat/format.md *Trailing truncation*, storage.md §6).
// A file commit whose top pages are all on the free list lowers the page high-water and, after its
// barrier, shortens the file to one growth step past it. Page counts, file lengths, and the reader
// watermark are host/internal state the corpus cannot see (CLAUDE.md §10). Mirrors
// impl/go/truncate_test.go and impl/rust/tests/truncate.rs.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";
import { createDatabase, Database, intValue, openDatabase, type Value } from "../src/lib.ts";

const PAGE = 4096;
const PROBE = "SELECT id, length(pad), n FROM t WHERE n = 3 ORDER BY id";

const scratch = mkdtempSync(join(tmpdir(), "jed-truncate-"));
after(() => rmSync(scratch, { recursive: true, force: true }));

function createFile(path: string): Database {
  return createDatabase({ path, pageSize: PAGE, skipFsync: true });
}

// growAndShrink grows table t to 2000 wide rows, then deletes all but 100: most pages end up dead.
function growAndShrink(db: Database): void {
  const s = db.session();
  s.execute("CREATE TABLE t (id i64 PRIMARY KEY, pad text, n i64)");
  s.execute("CREATE INDEX t_n ON t (n)");
  s.execute("INSERT INTO t SELECT g, repeat('x', 300), g % 7 FROM generate_series(1, 2000) g");
  s.execute("DELETE FROM t WHERE id > 100");
  s.close();
}

// churn runs n single-row updates of t, each its own durable commit, touching the table and its index.
function churn(db: Database, n: number, tag: string): void {
  const s = db.session();
  for (let k = 0; k < n; k++) {
    s.execute(
      `UPDATE t SET pad = '${tag}${k}' || repeat('y', 200), n = n + 7 WHERE id = ${1 + (k % 100)}`,
    );
  }
  s.close();
}

function rowsAndCost(db: Database, sql: string): { rows: Value[][]; cost: bigint } {
  const s = db.session();
  try {
    const rows = s.query(sql);
    try {
      const collected = [...rows];
      return { rows: collected, cost: rows.cost };
    } finally {
      rows.close();
    }
  } finally {
    s.close();
  }
}

// growthStep is the pages reserve adds growing from pages (pager.md §7): clamped to [16 KiB, 1 MiB].
function growthStep(pages: number): number {
  return Math.min((1024 * 1024) / PAGE, Math.max((16 * 1024) / PAGE, pages));
}

test("trailing truncation shrinks a file after a delete", () => {
  const path = join(scratch, "truncate.jed");
  const db = createFile(path);
  growAndShrink(db);
  const peak = db.pageCount;
  const peakLength = statSync(path).size;
  churn(db, 600, "a");
  const pc = db.pageCount;
  assert.ok(
    pc * 3 <= peak,
    `the dead top of the file is returned: ${peak} pages at peak, ${pc} after`,
  );
  const length = statSync(path).size;
  const limit = (pc + growthStep(pc)) * PAGE;
  assert.ok(
    length <= limit && length < peakLength,
    `file is ${length} bytes after truncation to ${pc} pages (limit ${limit}, peak ${peakLength})`,
  );
  const probe = rowsAndCost(db, PROBE);
  const count = rowsAndCost(db, "SELECT count(*) FROM t");
  db.close();

  // The truncated file reopens to the same high-water, rows, and costs.
  const re = openDatabase(path);
  try {
    assert.equal(re.pageCount, pc);
    assert.deepEqual(rowsAndCost(re, PROBE), probe);
    assert.deepEqual(rowsAndCost(re, "SELECT count(*) FROM t"), count);
  } finally {
    re.close();
  }
});

test("trailing truncation waits for an older reader", () => {
  const path = join(scratch, "truncate-pinned.jed");
  const db = createFile(path);
  try {
    growAndShrink(db);
    // A reader pinned at the post-delete version keeps every page of that version, so nothing is
    // truncated while it is open: the high-water never goes down.
    const reader = db.readSession();
    let high = db.pageCount;
    const s = db.session();
    for (let k = 0; k < 400; k++) {
      s.execute(`UPDATE t SET n = ${k} WHERE id = ${1 + (k % 100)}`);
      const pc = db.pageCount;
      assert.ok(
        pc >= high,
        `commit ${k} lowered the high-water from ${high} to ${pc} under a reader`,
      );
      high = pc;
    }
    s.close();
    reader.close();
    churn(db, 600, "b");
    assert.ok(db.pageCount < high, `truncation resumes: ${db.pageCount} pages, ${high} before`);
    assert.deepEqual(rowsAndCost(db, "SELECT count(*) FROM t").rows, [[intValue(100n)]]);
  } finally {
    db.close();
  }
});
