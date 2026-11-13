// Under incremental reclamation (incr_reclaim.ts, spec/design/incremental-reclaim.md §3.3) a pinned
// reader holds back only the generations listed after its version. Its snapshot stays intact while the
// writer churns, the writer still reuses pages listed before the pin, and once the reader closes the
// next commits merge the held generations and stop growing the file. The reader watermark and the
// in-memory generations are host-API state the corpus cannot express (CLAUDE.md §10). Mirrors the Go
// TestIncrReclaimReaderHoldsOnlyNewerGenerations.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import type { Engine } from "../src/executor.ts";
import { incrLeaked } from "../src/incr_reclaim.ts";
import { createDatabase, type Database, type Session } from "../src/lib.ts";

// countSession reads the row count through s, closing the cursor so it releases its own pin.
function countSession(s: Session): bigint {
  const cursor = s.query("SELECT count(*) FROM t");
  try {
    const v = [...cursor][0][0];
    return v.kind === "int" ? v.int : -1n;
  } finally {
    cursor.close();
  }
}

function storageOf(db: Database): Engine {
  return (db as unknown as { core: { storage: Engine } }).core.storage;
}

test("a pinned reader holds back only the newer free-list generations", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-incr-"));
  try {
    const db = createDatabase({ path: join(dir, "incr.jed"), pageSize: 256, skipFsync: true });
    try {
      db.execute("CREATE TABLE t (id i64 PRIMARY KEY, v text)");
      db.execute("INSERT INTO t SELECT g, repeat('a', 30) FROM generate_series(1, 200) g");
      for (let i = 0; i < 50; i++) {
        db.execute(`UPDATE t SET v = repeat('b', 30) WHERE id = ${1 + (i % 200)}`);
      }
      const steady = db.pageCount;
      const rd = db.readSession();
      const pinned = countSession(rd);
      for (let i = 0; i < 300; i++) {
        db.execute(`UPDATE t SET v = repeat('c', 30) WHERE id = ${1 + (i % 200)}`);
        // A no-op delete: an empty commit.
        if (i % 50 === 0) db.execute(`DELETE FROM t WHERE id = ${1000 + i}`);
      }
      assert.ok(
        storageOf(db).incr!.gens.length > 0,
        "a pinned reader left no newer generation unmerged",
      );
      assert.equal(countSession(rd), pinned, "the pinned reader's snapshot changed");
      rd.close();
      const grown = db.pageCount;
      assert.ok(
        grown > steady,
        `the file did not grow under the pin (${steady} → ${grown}): the test does not exercise the gate`,
      );
      for (let i = 0; i < 300; i++) {
        db.execute(`UPDATE t SET v = repeat('d', 30) WHERE id = ${1 + (i % 200)}`);
      }
      const st = storageOf(db);
      assert.ok(
        st.incr!.gens.length <= 1,
        `${st.incr!.gens.length} generations remain unmerged with no reader`,
      );
      assert.equal(incrLeaked(st), 0, "dead pages left unlisted");
      assert.ok(
        db.pageCount <= grown,
        `the file kept growing after the reader closed: ${grown} → ${db.pageCount}`,
      );
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
