// The storePaging-at-creation contract (bplus-reshape.md B4): a table CREATEd in this session (never
// loaded from a file) binds the domain pager at creation, so the post-commit residency flip demotes
// its committed leaves — an in-memory database (which never reopens) must not keep every table
// fully-resident decoded for the handle's lifetime, and a file-backed database must take the same
// shape in its creating session as after a reopen. Per-core because it asserts internal residency
// forms the corpus cannot express. Mirrors the Go TestInSessionTableJoinsResidencyFlip and the Rust
// residency_flip_tests.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { createDatabase, type Database } from "../src/tooling.ts";
import type { PNode } from "../src/pmap.ts";
import type { Snapshot } from "../src/executor.ts";
import { memDb } from "./mem_db.ts";

// committedOf reaches the handle's committed snapshot — a white-box cast (the shared core is
// internal, no public surface).
function committedOf(db: Database): Snapshot {
  return (db as unknown as { core: { committed: Snapshot } }).core.committed;
}

// countLeafForms walks a committed table tree and tallies leaf residency forms: Decoded (vals
// resident), Packed (block-backed), and OnDisk (demoted references).
function countLeafForms(root: PNode): { decoded: number; packed: number; ondisk: number } {
  const acc = { decoded: 0, packed: 0, ondisk: 0 };
  const walk = (n: PNode): void => {
    if (n.children.length === 0) {
      if (n.packed !== undefined) acc.packed++;
      else acc.decoded++;
      return;
    }
    for (const c of n.children) {
      if (c.node === null) acc.ondisk++;
      else walk(c.node);
    }
  };
  walk(root);
  return acc;
}

function run(db: Database): void {
  db.executeScript("CREATE TABLE t (k i32 PRIMARY KEY, v i32)");
  db.executeScript("CREATE INDEX t_v ON t (v)");
  // 200 rows at page size 256 → a multi-leaf tree; autocommit runs the flip on every commit.
  for (let k = 0; k < 200; k++) db.executeScript(`INSERT INTO t VALUES (${k}, ${k * 2})`);
  const snap = committedOf(db);
  const st = snap.stores.get("t")!;
  assert.ok(
    st.isFileBacked(),
    "an in-session-created table store should bind the domain pager at creation",
  );
  assert.ok(
    snap.indexStores.get("t_v")!.isFileBacked(),
    "an in-session-created index store should bind the domain pager at creation",
  );
  const root = st.treeRoot();
  assert.ok(root !== null, "t is non-empty");
  const { decoded, packed, ondisk } = countLeafForms(root);
  // The root leaf stays resident by the PMap convention; every other committed leaf must have
  // demoted. A multi-leaf tree therefore has OnDisk children and no Decoded leaf at all (the root is
  // interior); nothing should be resident-Packed right after a commit (packed forms arise on fault,
  // and the flip demoted the just-written Decoded forms).
  assert.ok(
    ondisk > 0,
    `expected a multi-leaf demoted tree, got ${JSON.stringify({ decoded, packed, ondisk })}`,
  );
  assert.equal(
    decoded,
    0,
    `committed leaves should demote after the flip (packed=${packed} ondisk=${ondisk})`,
  );
  // Reads fault the demoted leaves back through the pool and still see every row.
  const all = [...db.query("SELECT count(*), sum(v) FROM t")];
  assert.equal(all.length, 1);
  assert.deepEqual(
    all[0]!.map((v) => (v.kind === "int" ? v.int : null)),
    [200n, 39800n],
  );
}

test("in-memory: an in-session-created table joins the residency flip", () => {
  const db = memDb(256);
  try {
    run(db);
  } finally {
    db.close();
  }
});

test("file create-session: an in-session-created table joins the residency flip", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-flip-"));
  try {
    const db = createDatabase({ path: join(dir, "flip.jed"), pageSize: 256, skipFsync: true });
    try {
      run(db);
    } finally {
      db.close();
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
