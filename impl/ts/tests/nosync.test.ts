// fsync=off (api.md §2.1) is a DEV/TESTING durability knob: a commit writes the identical bytes in the
// same order but skips the fdatasync barrier. It must be byte/result-NEUTRAL — a database built with it
// holds the exact same on-disk image and reads back identically; only the flush-to-platter is skipped
// (so the data survives a process crash but not an OS crash). The conformance disk harness runs with it
// to cut the fsync-per-commit cost. The corpus cannot express fsync timing or file-byte identity, so
// this is a per-core unit test (CLAUDE.md §10). Mirrors impl/go/nosync_test.go and
// impl/rust/tests/nosync.rs.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { close, create, type Engine, execute } from "../src/tooling.ts";
import { open } from "../src/tooling.ts";

// buildSampleDb creates a file database at path with the given noSync setting, runs a fixed
// deterministic workload (DDL + inserts + an update + a delete, autocommitted across many commits), and
// closes it. Deterministic (no clock/entropy), so two runs differing only in noSync must produce
// byte-identical files.
function buildSampleDb(path: string, noSync: boolean): void {
  const db = create(path, { noSync });
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, v i32, s text)");
  for (let i = 1; i <= 50; i++) {
    execute(db, `INSERT INTO t VALUES (${i}, ${i * 10}, 'row-${i}')`);
  }
  execute(db, "UPDATE t SET v = v + 1 WHERE id % 2 = 0");
  execute(db, "DELETE FROM t WHERE id > 40");
  close(db);
}

// intAt reads a row cell as a bigint, asserting it is an int value.
function intAt(
  rows: readonly (readonly { kind: string; int?: bigint }[])[],
  row: number,
  col: number,
): bigint {
  const v = rows[row]![col]!;
  assert.equal(v.kind, "int");
  return v.kind === "int" ? v.int! : -1n;
}

test("fsync=off round-trips (a same-process reopen sees the committed state)", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-nosync-"));
  try {
    const path = join(dir, "nosync.jed");
    buildSampleDb(path, true);
    // The OS page cache holds the un-synced writes, so a same-process reopen sees the committed state —
    // fsync=off forfeits durability only across an OS crash, not a clean close + reopen.
    const db: Engine = open(path, { noFsync: true });
    const o = execute(db, "SELECT id, v FROM t ORDER BY id");
    assert.equal(o.kind, "query");
    if (o.kind !== "query") throw new Error("expected a query");
    assert.equal(o.rows.length, 40); // 50 inserted, ids 41..50 deleted
    // id=1 is odd → v=10; id=2 is even → v = 20 + 1 = 21 after the UPDATE.
    assert.equal(intAt(o.rows, 0, 0), 1n);
    assert.equal(intAt(o.rows, 0, 1), 10n);
    assert.equal(intAt(o.rows, 1, 0), 2n);
    assert.equal(intAt(o.rows, 1, 1), 21n);
    close(db);
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("fsync=off is byte-identical to fsync=on", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-nosync-"));
  try {
    const on = join(dir, "on.jed");
    const off = join(dir, "off.jed");
    buildSampleDb(on, false); // fsync on (the default)
    buildSampleDb(off, true); // fsync off
    // fsync=off changes only *when* bytes are flushed, never *which* bytes: byte-identical files (so no
    // golden churn, no format bump, cross-core byte-identity preserved).
    assert.deepEqual(new Uint8Array(readFileSync(on)), new Uint8Array(readFileSync(off)));
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});
