// Crash-recovery tests driven by the fault-injection seam (spec/design/storage.md §7). These verify
// the §4 commit atomicity at the actual commit points — mid-body, before the body sync, between the
// body and meta syncs, and a torn meta write — which the static torn_meta_slot*.jed goldens (a
// post-hoc byte corruption) cannot reach. The invariant under test: a crash anywhere in a commit
// leaves the file readable as a valid snapshot (the prior one, or — at the last barrier — the new
// one), never corrupt; and the free-list reconstruction (P6.2) stays correct after a recovery. This
// is per-core, not corpus (a crash mid-commit is not SQL-level deterministic, like P5.3 concurrency);
// the cross-core contract is the recovery outcome, asserted identically in Rust and Go (recovery.rs,
// crash_recovery_test.go).

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { close, create, type Engine, execute, open } from "../src/lib.ts";
import type { CommitFault } from "../src/pager.ts";

function tmpDir(): string {
  return mkdtempSync(join(tmpdir(), "jed-crash-"));
}

// idsSorted returns t's ids ascending (the B-tree scan is key-ordered, but sort to be order-robust).
function idsSorted(db: Engine): bigint[] {
  const o = execute(db, "SELECT id FROM t");
  if (o.kind !== "query") throw new Error("expected a query");
  const out = o.rows.map((r) => {
    const v = r[0]!;
    if (v.kind !== "int") throw new Error("non-int id");
    return v.int;
  });
  return out.sort((a, b) => (a < b ? -1 : a > b ? 1 : 0));
}

// seed returns a fresh file-backed t(id i32 PRIMARY KEY) holding rows 1,2 (each INSERT autocommits
// durably) and the prior committed txid.
function seed(path: string): { db: Engine; prior: bigint } {
  const db = create(path);
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY)");
  execute(db, "INSERT INTO t VALUES (1)");
  execute(db, "INSERT INTO t VALUES (2)");
  return { db, prior: db.txid };
}

// insertWithFault arms f, then runs an autocommit INSERT (3) that drives persistImpl into it — which
// must throw. Closes db (a clean close rolls back, no further writes) so the file is left crash-state.
function insertWithFault(db: Engine, f: CommitFault): void {
  if (db.paging === null) throw new Error("expected a file-backed database");
  db.paging.armFault(f);
  assert.throws(
    () => execute(db, "INSERT INTO t VALUES (3)"),
    "expected the injected commit crash",
  );
  close(db);
}

// body_write #1 — a clean crash on the first body-page write, before the body is even synced. The new
// commit's pages are partial/unreferenced and the prior meta is untouched, so the file reopens at the
// prior two-row snapshot.
test("crash mid-body recovers the prior snapshot", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "crash_mid_body.jed");
    const { db, prior } = seed(path);
    insertWithFault(db, { point: "body_write", n: 1 });

    const db2 = open(path);
    assert.equal(db2.txid, prior, "fell back to the prior snapshot");
    assert.deepEqual(idsSorted(db2), [1n, 2n], "the prior snapshot is intact");
    close(db2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// body_write #1 torn — a partial first body-page write. A dirty page is always a freshly allocated
// slot (copy-on-write never overwrites a page the prior meta references — P6.2 torn-safety), so the
// torn page is unreferenced and the prior snapshot reopens intact.
test("a torn body page recovers the prior snapshot", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "torn_body.jed");
    const { db, prior } = seed(path);
    insertWithFault(db, { point: "body_write", n: 1, tearBytes: 64 });

    const db2 = open(path);
    assert.equal(db2.txid, prior, "the torn body page is never referenced");
    assert.deepEqual(idsSorted(db2), [1n, 2n]);
    close(db2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// sync #1 — the body-durability barrier fails. The body is written-through but unsynced and the meta
// is never written, so the prior meta still governs and the prior snapshot reopens.
test("crash before the body sync recovers the prior snapshot", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "crash_body_sync.jed");
    const { db, prior } = seed(path);
    insertWithFault(db, { point: "sync", n: 1 });

    const db2 = open(path);
    assert.equal(db2.txid, prior);
    assert.deepEqual(idsSorted(db2), [1n, 2n]);
    close(db2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// meta_write — the critical between-syncs window (§4): the body is fully written AND synced, then the
// publish (the meta-slot write) crashes. The new body pages are durable but unreferenced; the prior
// meta slot is untouched, so the file reopens at the prior snapshot.
test("crash between the body and meta syncs recovers the prior snapshot", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "crash_between_syncs.jed");
    const { db, prior } = seed(path);
    insertWithFault(db, { point: "meta_write" });

    const db2 = open(path);
    assert.equal(db2.txid, prior, "durable-but-unreferenced body → prior snapshot");
    assert.deepEqual(idsSorted(db2), [1n, 2n]);
    close(db2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// meta_write torn — a partial meta-slot write corrupts its checksum. The loader rejects the torn slot
// (CRC mismatch) and falls back to the other, valid slot — the prior snapshot. This is the
// torn_meta_slot*.jed golden's property, now exercised at the actual publish point. Write only the
// first 20 bytes: the checksum at offset 32 keeps its old value while bytes [0,32) change → mismatch.
test("a torn meta write falls back to the prior snapshot", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "torn_meta.jed");
    const { db, prior } = seed(path);
    insertWithFault(db, { point: "meta_write", tearBytes: 20 });

    const db2 = open(path);
    assert.equal(db2.txid, prior, "torn meta slot rejected → fall back to the other slot");
    assert.deepEqual(idsSorted(db2), [1n, 2n]);
    close(db2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// sync #2 — the meta is written, then its durability barrier fails. Atomicity holds either way: a real
// power loss could keep the meta (→ new) or lose it (→ prior); the seam writes through, so the reopen
// deterministically yields the new snapshot. Both are valid — assert a consistent, fully readable
// snapshot that is exactly one of the two (never a half-published state).
test("crash before the meta sync is atomic (a valid snapshot either way)", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "crash_meta_sync.jed");
    const { db, prior } = seed(path);
    insertWithFault(db, { point: "sync", n: 2 });

    const db2 = open(path);
    if (db2.txid === prior) {
      assert.deepEqual(idsSorted(db2), [1n, 2n], "prior snapshot (meta lost)");
    } else {
      assert.equal(db2.txid, prior + 1n, "new snapshot (meta survived)");
      assert.deepEqual(idsSorted(db2), [1n, 2n, 3n], "new snapshot is fully consistent");
    }
    close(db2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// After a crash-to-prior recovery the file is fully functional: the free-list reconstructs correctly
// on the reopen (P6.2), so subsequent commits reuse dead pages, persist durably, and round-trip — and
// the file does not corrupt across the crash → reopen → churn → reopen cycle.
test("recovery then free-list reuse stays consistent", () => {
  const dir = tmpDir();
  try {
    const path = join(dir, "recovery_then_reuse.jed");
    const { db, prior } = seed(path);

    // Crash between the syncs → reopen at the prior two-row snapshot.
    insertWithFault(db, { point: "meta_write" });
    let db2 = open(path);
    assert.equal(db2.txid, prior);
    assert.deepEqual(idsSorted(db2), [1n, 2n]);

    // Churn through several commits (frees pages a prior root abandoned, then reuses them).
    execute(db2, "INSERT INTO t VALUES (3)");
    execute(db2, "INSERT INTO t VALUES (4)");
    execute(db2, "DELETE FROM t WHERE id = 1");
    execute(db2, "INSERT INTO t VALUES (5)");
    const pageCountAfter = db2.pageCount;
    close(db2);

    db2 = open(path);
    assert.deepEqual(
      idsSorted(db2),
      [2n, 3n, 4n, 5n],
      "post-recovery commits are durable and correct",
    );

    // A second churn round reuses the reconstructed free-list rather than growing the file unbounded.
    execute(db2, "DELETE FROM t WHERE id = 2");
    execute(db2, "INSERT INTO t VALUES (6)");
    close(db2);
    db2 = open(path);
    assert.deepEqual(idsSorted(db2), [3n, 4n, 5n, 6n]);
    assert.ok(
      db2.pageCount <= pageCountAfter + 4,
      `free-list reuse keeps the file bounded after recovery (was ${pageCountAfter}, now ${db2.pageCount})`,
    );
    close(db2);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
