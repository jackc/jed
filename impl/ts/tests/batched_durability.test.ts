// Batched durability under synchronous = off (spec/design/validated-cow.md §4): host-API and byte-level
// assertions the corpus cannot express — barrier counts, meta-slot placement, the batch cap, close's
// flush, the floor-only reuse rule, and the page-service mirror of the batch state. Crash recovery
// under relaxed mode is the simulator's (spec/design/simulation-testing.md, the relaxed profile).
// TypeScript always runs the cap flush inline, so the background-flush byte-identity test of the Go
// and Rust cores becomes a run-to-run byte-identity check here.

import assert from "node:assert/strict";
import { mkdtempSync, readFileSync, rmSync, statSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { after, test } from "node:test";

import { type BlockStore, ClosedBlockStore } from "../src/blockstore.ts";
import { manifestInlineCapacity } from "../src/commit_manifest.ts";
import { engineError } from "../src/errors.ts";
import type { Engine } from "../src/executor.ts";
import { createDatabase, openDatabase } from "../src/file.ts";
import type { SharedPaging } from "../src/paging.ts";
import { ServiceState } from "../src/pageservice/shared_state.ts";
import { Database } from "../src/shared.ts";
import { errCode } from "./util.ts";

const dir = mkdtempSync(join(tmpdir(), "jed-relaxed-"));
after(() => rmSync(dir, { recursive: true, force: true }));

// Op is one recorded store operation.
type Op =
  | { kind: "write"; offset: number; bytes: Uint8Array }
  | { kind: "size"; n: number }
  | {
      kind: "sync";
    };

// RecordingStore wraps a store, counting barriers and recording writes and size changes. failSync
// makes every barrier fail, as a dying device would.
class RecordingStore implements BlockStore {
  readonly base: BlockStore;
  ops: Op[] = [];
  syncs = 0;
  failSync = false;
  constructor(base: BlockStore) {
    this.base = base;
  }
  readAt(offset: number, len: number): Uint8Array {
    return this.base.readAt(offset, len);
  }
  writeAt(offset: number, bytes: Uint8Array): void {
    this.ops.push({ kind: "write", offset, bytes: bytes.slice() });
    this.base.writeAt(offset, bytes);
  }
  sync(): void {
    if (this.failSync) throw engineError("io_error", "injected sync failure");
    this.syncs++;
    this.ops.push({ kind: "sync" });
    this.base.sync();
  }
  size(): number {
    return this.base.size();
  }
  setSize(bytes: number): void {
    this.ops.push({ kind: "size", n: bytes });
    this.base.setSize(bytes);
  }
  close(): void {
    this.base.close();
  }
}

function storage(db: Database): Engine {
  return (db as unknown as { core: { storage: Engine } }).core.storage;
}

// record wraps db's main store in a RecordingStore.
function record(db: Database): RecordingStore {
  const paging: SharedPaging = storage(db).paging!;
  const rec = new RecordingStore(paging.swapStore(new ClosedBlockStore()));
  paging.swapStore(rec);
  return rec;
}

function relaxedDb(name: string, pageSize?: number): { db: Database; path: string } {
  const path = join(dir, name);
  const db = createDatabase({ path, pageSize, synchronous: "off", locking: "none" });
  return { db, path };
}

function metaTxidAt(path: string, slot: number): bigint {
  const img = readFileSync(path);
  const dv = new DataView(img.buffer, img.byteOffset, img.byteLength);
  const ps = dv.getUint32(8, false);
  return dv.getBigUint64(slot * ps + 12, false);
}

test("relaxed commits defer the barrier to close", () => {
  const { db, path } = relaxedDb("defer.jed");
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)");
  const rec = record(db);
  for (let i = 1; i <= 10; i++) db.execute(`INSERT INTO t VALUES (${i}, 'x')`);
  assert.equal(rec.syncs, 0, "relaxed commits issued barriers below the batch cap");
  // Every pending commit writes the slot that does not hold the floor (create's txid 1 in slot 1).
  assert.equal(metaTxidAt(path, 0), db.version);
  assert.equal(metaTxidAt(path, 1), 1n);
  db.close();
  assert.equal(rec.syncs, 1, "close issues the one flush");
  const reopened = openDatabase(path, { locking: "none" });
  try {
    assert.equal(reopened.get("SELECT count(*) AS n FROM t")?.n, 10n);
  } finally {
    reopened.close();
  }
});

test("the batch cap flushes inline, and relaxed runs are byte-identical", () => {
  const images: Uint8Array[] = [];
  const barriers: number[] = [];
  for (const run of [0, 1]) {
    const { db, path } = relaxedDb(`cap${run}.jed`, 256);
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)");
    db.execute("CREATE INDEX t_v ON t (v)");
    const rec = record(db);
    for (let i = 1; i <= 120; i++) {
      db.execute(`INSERT INTO t VALUES (${i}, repeat('v', ${i % 40}))`);
      if (i % 7 === 0) db.execute(`DELETE FROM t WHERE id = ${i - 3}`);
    }
    barriers.push(rec.syncs);
    db.close();
    images.push(readFileSync(path));
  }
  assert.ok(barriers[0]! > 0, "the batch cap never flushed");
  assert.equal(barriers[0], barriers[1]);
  assert.deepEqual(images[0], images[1]);
});

// A commit inside a batch may reuse only pages free at the floor, never one freed during the batch, and
// no free list may hold a pending page (validated-cow.md §4 rules 3 and 4). The file must also stop
// growing: each batch's orphans become reusable once a flush moves the floor past them.
test("a relaxed batch reuses only the floor's free pages", () => {
  const { db } = relaxedDb("reuse.jed", 256);
  try {
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)");
    for (let i = 1; i <= 40; i++) db.execute(`INSERT INTO t VALUES (${i}, repeat('v', 30))`);
    const st = storage(db);
    const state = () => ({
      pending: new Set(st.paging!.pendingEntries().map((e) => e.index)),
      free: st.freePages.slice(),
      floorFree: st.floorFree,
      pageCount: st.pageCount,
    });
    let midway = 0;
    for (let i = 0; i < 400; i++) {
      const before = state();
      const letter = String.fromCharCode(97 + (i % 26));
      db.execute(`UPDATE t SET v = repeat('${letter}', 30) WHERE id = ${1 + (i % 40)}`);
      const after = state();
      for (const f of after.free) {
        assert.ok(!after.pending.has(f), `update ${i}: the free list holds pending page ${f}`);
      }
      // The batch's first commit plans as under synchronous = on; or a cap flush followed.
      if (before.pending.size > 0 && after.pending.size > 0) {
        for (const pg of after.pending) {
          if (before.pending.has(pg) || pg >= before.pageCount) continue;
          assert.ok(
            before.floorFree?.has(pg) === true,
            `update ${i} reused page ${pg}, which was not free at the floor`,
          );
        }
      }
      if (i === 200) midway = st.pageCount;
    }
    assert.ok(
      st.pageCount <= midway + manifestInlineCapacity(256) * 2,
      `the file kept growing under relaxed churn: ${midway} pages at update 200, ${st.pageCount} at 400`,
    );
  } finally {
    db.close();
  }
});

// A two-commit batch crashes so that only the second commit's writes (its body pages and meta) land:
// the first commit's pages, which the second commit's tree still references, are missing. The second
// meta's cumulative manifest lists them, so recovery must reject it and fall back to the floor — never
// accept a root whose dependencies did not land (validated-cow.md §4 rule 2).
test("the newest meta vouches for the whole batch", () => {
  const { db, path } = relaxedDb("vouch.jed", 256);
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)");
  db.execute("INSERT INTO t SELECT g, repeat('a', 20) FROM generate_series(1, 60) g");
  db.close(); // the floor: every row 'a'
  const prior = readFileSync(path);
  const reopened = openDatabase(path, { synchronous: "off", locking: "none" });
  try {
    // The first commit after an open stabilizes the adopted generation with a barrier
    // (validated-cow.md §3); it then starts the batch.
    reopened.execute("UPDATE t SET v = repeat('w', 20) WHERE id = 30");
    const rec = record(reopened);
    reopened.execute("UPDATE t SET v = repeat('b', 20) WHERE id = 1"); // leaf A
    const first = rec.ops.length;
    reopened.execute("UPDATE t SET v = repeat('c', 20) WHERE id = 60"); // leaf B
    assert.ok(
      rec.ops.every((op) => op.kind !== "sync"),
      "the two-commit batch reached a barrier; the test needs it pending",
    );
    // Power loss: the floor, plus every op of the second commit, and none of the first commit's.
    let img = new Uint8Array(prior);
    const grow = (n: number): void => {
      if (n <= img.length) return;
      const bigger = new Uint8Array(n);
      bigger.set(img);
      img = bigger;
    };
    for (const op of rec.ops.slice(first)) {
      if (op.kind === "write") {
        grow(op.offset + op.bytes.length);
        img.set(op.bytes, op.offset);
      } else if (op.kind === "size") grow(op.n);
    }
    const recovered = Database.fromImage(img);
    const rows = recovered.all("SELECT v FROM t WHERE id IN (1, 60) ORDER BY id");
    assert.deepEqual(
      rows.map((r) => r.v),
      ["a".repeat(20), "a".repeat(20)],
    );
  } finally {
    reopened.close();
  }
});

test("close reports a failed flush", () => {
  const { db } = relaxedDb("flushfail.jed");
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  db.execute("INSERT INTO t VALUES (1)");
  record(db).failSync = true;
  assert.equal(
    errCode(() => db.close()),
    "58030",
  );
});

test("synchronous = on keeps txid-parity slots", () => {
  const path = join(dir, "on.jed");
  const db = createDatabase({ path, skipFsync: true, locking: "none" });
  try {
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
    for (let i = 1; i <= 5; i++) {
      db.execute(`INSERT INTO t VALUES (${i})`);
      const v = db.version;
      assert.equal(metaTxidAt(path, Number(v & 1n)), v);
    }
  } finally {
    db.close();
  }
});

// A page-service owner mirrors its batch state into the service's shared state: the service's flush
// before a client begin_write empties the pending set and moves the floor, which the engine thread
// adopts at its next commit (page-service.md §6). The mirror is driven directly here, as the worker
// would.
test("the engine adopts a service flush through the shared batch state", () => {
  const { db, path } = relaxedDb("mirror.jed");
  try {
    const paging = storage(db).paging!;
    const state = new ServiceState();
    paging.setBatchMirror(state);
    assert.equal(state.floorSlot(), 1);
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
    db.execute("INSERT INTO t VALUES (1)");
    assert.ok(state.batchPending());
    assert.equal(metaTxidAt(path, 0), 3n); // both pending commits wrote slot 0
    // The service flushes (its own barrier on the file) and records the newest meta as the floor.
    paging.sync();
    state.batchFlushed();
    assert.equal(state.floorSlot(), 0);
    db.execute("INSERT INTO t VALUES (2)");
    // The next commit starts a new batch in the other slot, its manifest listing only its own pages.
    assert.equal(metaTxidAt(path, 1), 4n);
    assert.equal(metaTxidAt(path, 0), 3n);
    assert.ok(state.batchPending());
    assert.ok(paging.pendingEntries().length <= manifestInlineCapacity(4096));
  } finally {
    db.close();
  }
});

// Trailing truncation under synchronous = off (validated-cow.md §4, format.md *Trailing truncation*):
// only a commit that begins with nothing pending lowers the high-water, and the file is shortened only
// at the barrier that makes the lowering generation durable — the batch's cap flush or close — never
// while the batch is pending. A table dropped at the top of the file leaves a long free run there once
// a flush lets reclamation list its pages; until then the relaxed churn grows the file past it.
// Mirrors the Go TestTrailingTruncationWaitsForTheFlushUnderSynchronousOff.
test("relaxed trailing truncation waits for the flush", () => {
  const PAGE = 4096;
  const growthStep = (pages: number) =>
    Math.min((1024 * 1024) / PAGE, Math.max((16 * 1024) / PAGE, pages));
  const path = join(dir, "truncate.jed");
  const seed = createDatabase({ path, pageSize: PAGE, skipFsync: true, locking: "none" });
  seed.execute("CREATE TABLE t (id i64 PRIMARY KEY, pad text, n i64)");
  seed.execute("INSERT INTO t SELECT g, repeat('x', 300), g % 7 FROM generate_series(1, 100) g");
  seed.execute("CREATE TABLE big (id i64 PRIMARY KEY, pad text)");
  seed.execute("INSERT INTO big SELECT g, repeat('z', 300) FROM generate_series(1, 20000) g");
  seed.close();
  const db = openDatabase(path, { synchronous: "off", skipFsync: true, locking: "none" });
  const length = () => statSync(path).size;
  const state = () => {
    const st = storage(db);
    return { pc: st.pageCount, pending: st.paging!.hasPending() };
  };
  let peak = state().pc;
  db.execute("DROP TABLE big");
  let deferred = false;
  for (let k = 0; k < 1500; k++) {
    const before = length();
    const { pc: high, pending: pendingBefore } = state();
    db.execute(
      `UPDATE t SET pad = 'r${k}' || repeat('y', 200), n = n + 7 WHERE id = ${1 + (k % 100)}`,
    );
    const now = length();
    const { pc, pending } = state();
    peak = Math.max(peak, pc);
    assert.ok(
      !(pendingBefore && pc < high),
      `commit ${k} lowered the high-water from ${high} to ${pc} inside a pending batch`,
    );
    assert.ok(
      !(pending && now < before),
      `commit ${k} shortened the file from ${before} to ${now} bytes while its batch is pending`,
    );
    if (pending && pc < high && now > (pc + growthStep(pc)) * PAGE) deferred = true;
  }
  const pc = state().pc;
  assert.ok(
    pc * 3 <= peak,
    `the dropped table's pages are returned: ${peak} pages at the peak, ${pc} after churn`,
  );
  assert.ok(deferred, "no relaxed commit lowered the high-water with its shrink left to the flush");
  db.close();
  const limit = (pc + growthStep(pc)) * PAGE;
  assert.ok(length() <= limit, `close left the file ${length()} bytes (at most ${limit})`);
  const re = openDatabase(path, { locking: "none" });
  try {
    assert.equal(re.pageCount, pc);
    assert.equal(re.get("SELECT count(*) AS n FROM t")?.n, 100n);
  } finally {
    re.close();
  }
});
