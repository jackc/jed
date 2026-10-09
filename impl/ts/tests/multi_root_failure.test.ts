// A multi-root commit that fails after an in-memory attachment was staged (attached-databases.md §5)
// must leave that attachment's storage matching its still-published root. Before staging was split
// from adoption, the attachment adopted its page accounting and compacted relative to the UNPUBLISHED
// root, so its free list held pages the published root still referenced; the next commit overwrote
// them under every reader of the old root (silent corruption or XX001). Fault injection and storage
// internals are out of the corpus's reach (CLAUDE.md §10). Mirrors the Rust
// shared::multi_root_failure_tests and impl/go/multi_root_failure_test.go.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import type { Engine } from "../src/executor.ts";
import {
  attachMemory,
  createDatabase,
  type Database,
  EngineError,
  type Session,
} from "../src/lib.ts";
import type { CommitFault } from "../src/pager.ts";

const ROWS = 200n;

// core reaches the private shared core (the analogue of the Go test's db.core).
function core(db: Database): { storage: Engine; attachments: Map<string, { storage: Engine }> } {
  return (
    db as unknown as {
      core: { storage: Engine; attachments: Map<string, { storage: Engine }> };
    }
  ).core;
}

// attachmentRows reads every [id, v] of a.t in key order through a fresh session.
function attachmentRows(db: Database): [bigint, bigint][] {
  const s = db.session();
  const rows = s.query("SELECT id, v FROM a.t ORDER BY id");
  const out: [bigint, bigint][] = [];
  for (const r of rows) {
    const id = r[0]!;
    const v = r[1]!;
    assert.ok(id.kind === "int" && v.kind === "int");
    out.push([id.int, v.int]);
  }
  rows.close();
  s.close();
  return out;
}

function expected(v: (id: bigint) => bigint): [bigint, bigint][] {
  const out: [bigint, bigint][] = [];
  for (let id = 1n; id <= ROWS; id++) out.push([id, v(id)]);
  return out;
}

// seedAttachment seeds a.t with ROWS rows (v = 0) in one commit — a multi-leaf tree at page size 256.
function seedAttachment(s: Session): void {
  s.execute("CREATE TABLE a.t (id i64 PRIMARY KEY, v i64)");
  s.execute(`INSERT INTO a.t SELECT g, 0 FROM generate_series(1, ${ROWS}) AS g`);
}

// dirtyAttachment rewrites every leaf of a.t and doubles it, so the failing commit's high-water passes
// twice the live count and its post-commit compaction is due.
function dirtyAttachment(s: Session): void {
  s.execute("UPDATE a.t SET v = 1");
  s.execute(`INSERT INTO a.t SELECT g, 1 FROM generate_series(${ROWS + 1n}, ${3n * ROWS}) AS g`);
}

function armFault(storage: Engine, fault: CommitFault): void {
  storage.paging!.armFault(fault);
}

function assertCode(f: () => void, code: string): void {
  assert.throws(f, (e: unknown) => e instanceof EngineError && e.code() === code);
}

test("failed main persist leaves an in-memory attachment intact", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-multi-root-"));
  try {
    const db = createDatabase({ path: join(dir, "main.jed"), pageSize: 256, skipFsync: true });
    db.attach("a", attachMemory(), false);
    const s = db.session();
    s.execute("CREATE TABLE t (id i64 PRIMARY KEY)");
    seedAttachment(s);

    // Main's durable commit fails at its barrier after the attachment was dirtied in the same tx.
    s.begin(true);
    dirtyAttachment(s);
    s.execute("INSERT INTO t VALUES (1)");
    armFault(core(db).storage, { point: "sync", n: 1 });
    assertCode(() => s.commit(), "58030");

    // Nothing published: a fresh session sees the attachment's prior root.
    assert.deepEqual(
      attachmentRows(db),
      expected(() => 0n),
    );
    // Main now refuses writes until reopened, but each attempt still stages attachment pages before
    // main's persist fails. They must never land on pages the published root uses.
    for (let k = 0; k < 4; k++) {
      assertCode(() => s.execute(`UPDATE a.t SET v = ${10 + k} WHERE id % 7 = ${k}`), "58030");
      assert.deepEqual(
        attachmentRows(db),
        expected(() => 0n),
      );
    }
    s.close();
    db.close();
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

test("failed later attachment leaves an earlier in-memory attachment intact", () => {
  const db = createDatabase({ pageSize: 256 });
  db.attach("a", attachMemory(), false);
  db.attach("b", attachMemory(), false);
  const s = db.session();
  s.execute("CREATE TABLE t (id i64 PRIMARY KEY)");
  s.execute("CREATE TABLE b.t (id i64 PRIMARY KEY)");
  seedAttachment(s);

  // a stages before b (in-memory attachments commit in name order); b's pack fails.
  s.begin(true);
  dirtyAttachment(s);
  s.execute("INSERT INTO b.t VALUES (1)");
  s.execute("INSERT INTO t VALUES (1)");
  armFault(core(db).attachments.get("b")!.storage, { point: "body_write", n: 1 });
  assertCode(() => s.commit(), "58030");
  assert.deepEqual(
    attachmentRows(db),
    expected(() => 0n),
  );

  // Main and a stay writable. Each later commit rewrites one leaf path and reuses a's free list; the
  // leaves it does not touch are still the published root's and must survive.
  for (let k = 0n; k < 6n; k++) {
    s.execute(`UPDATE a.t SET v = 7 WHERE id = ${1n + 37n * k}`);
    s.execute(`INSERT INTO t VALUES (${k + 2n})`);
  }
  const updated = (id: bigint): boolean => (id - 1n) % 37n === 0n && id <= 1n + 37n * 5n;
  assert.deepEqual(
    attachmentRows(db),
    expected((id) => (updated(id) ? 7n : 0n)),
  );
  const fresh = db.session();
  const n = fresh.query("SELECT count(*) FROM t");
  const counts = [...n].map((r) => r[0]!);
  n.close();
  assert.deepEqual(counts, [{ kind: "int", int: 6n }]);
});
