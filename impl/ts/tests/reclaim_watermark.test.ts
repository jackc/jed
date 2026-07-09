// Deterministic regression test for the reader-liveness watermark gating within-session free-list reuse
// (transactions.md §8) — the concurrency case the corpus cannot express (CLAUDE.md §10). A file-backed
// reader that pins the committed snapshot in the persist→publish window (the "fallback reader") must never
// observe rows from a later commit, even though continuous within-session reclamation is recycling pages.
// It reproduced a snapshot-isolation violation before the free-list generation gate landed (a pinned
// reader saw newer rows because its pages were reclaimed and overwritten). JS is single-threaded, so the
// afterPersistHook seam lets us pin the fallback reader synchronously — no thread coordination needed.
// Mirrors the Go TestFallbackReaderSnapshotIsolationUnderReclamation and the Rust equivalent.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { createDatabase, openDatabase, type Session } from "../src/lib.ts";
import { setAfterPersistHook } from "../src/shared.ts";

function countSession(s: Session): bigint {
  const rows = [...s.query("SELECT count(*) FROM t")];
  const v = rows[0][0];
  return v.kind === "int" ? v.int : -1n;
}

test("fallback reader stays snapshot-isolated under within-session reclamation (§8)", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-reclaim-"));
  const path = join(dir, "fallback.jed");
  try {
    {
      const db = createDatabase({ path, pageSize: 256, skipFsync: true });
      db.execute("CREATE TABLE t (id i64 PRIMARY KEY)");
      // A multi-leaf tree so within-session compaction runs and a free-list accumulates.
      for (let i = 1; i <= 120; i++) db.execute(`INSERT INTO t VALUES (${i})`);
      db.close();
    }
    const db = openDatabase(path, { cacheBytes: 4 * 256, skipFsync: true });
    try {
      let reader: Session | null = null;
      let pinnedCount = -1n;
      // The hook fires in the persist→publish window; act ONLY on a commit that compacted (advanced the
      // free-list generation past the still-published version), which just placed a page reachable at the
      // fallback version into the reusable free-list. Pin a reader at that fallback version synchronously.
      setAfterPersistHook((committedTxid, freeGenTxid) => {
        if (reader !== null || freeGenTxid <= committedTxid) return;
        reader = db.readSession(); // pins the PRIOR published (fallback) version
        pinnedCount = countSession(reader);
      });

      // Drive commits until one compacts and fires the hook (pinning the fallback reader mid-commit).
      let i = 121;
      while (reader === null && i <= 4000) {
        db.execute(`INSERT INTO t VALUES (${i})`);
        i++;
      }
      setAfterPersistHook(null);
      assert.ok(
        reader !== null,
        "no compacting commit occurred — test did not exercise the reuse path",
      );

      // Hammer reuse-commits while the reader is pinned at the fallback version: on the buggy path these
      // recycle a page the reader still references and overwrite it; the gate must defer that reuse.
      for (let j = 4001; j <= 4200; j++) db.execute(`INSERT INTO t VALUES (${j})`);

      const pinned: Session = reader;
      const got = countSession(pinned);
      pinned.close();
      assert.equal(
        got,
        pinnedCount,
        `SNAPSHOT ISOLATION VIOLATED: fallback reader pinned ${pinnedCount} but now sees ${got} (its pages were reclaimed and overwritten)`,
      );
    } finally {
      setAfterPersistHook(null);
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
