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
import { Environment } from "../src/environment.ts";
import { EngineError, engineError } from "../src/errors.ts";
import type { Engine } from "../src/executor.ts";
import { createDatabase, openDatabase } from "../src/file.ts";
import { BatchDeadline } from "../src/idle_flush.ts";
import { setDurability } from "../src/persist.ts";
import { type ManualClock, manualClock } from "../src/seam.ts";
import type { SharedPaging } from "../src/paging.ts";
import { ServiceState } from "../src/pageservice/shared_state.ts";
import { attachMemory, Database } from "../src/shared.ts";
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

// relaxedDb is a relaxed database with no time-based flush, so wall time never moves a flush.
function relaxedDb(name: string, pageSize?: number): { db: Database; path: string } {
  return relaxedDbTimed(name, pageSize, 0, new Environment());
}

// relaxedDbTimed is a relaxed database whose batches end intervalMs after they start on env's clock.
function relaxedDbTimed(
  name: string,
  pageSize: number | undefined,
  intervalMs: number,
  environment: Environment,
): { db: Database; path: string } {
  const path = join(dir, name);
  const db = createDatabase({
    path,
    pageSize,
    synchronous: "off",
    flushIntervalMs: intervalMs,
    environment,
    locking: "none",
  });
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
  const reopened = openDatabase(path, { synchronous: "off", flushIntervalMs: 0, locking: "none" });
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
  const db = openDatabase(path, {
    synchronous: "off",
    flushIntervalMs: 0,
    skipFsync: true,
    locking: "none",
  });
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

// manualEnv is a manual clock and an environment that reads it.
function manualEnv(): { clock: ManualClock; env: Environment } {
  const clock = manualClock(0n);
  return { clock, env: Environment.builder().clockSource(clock).build() };
}

function pending(db: Database): boolean {
  return storage(db).paging!.hasPending();
}

// eventually waits up to five seconds for cond, on the event loop.
async function eventually(cond: () => boolean): Promise<boolean> {
  const start = Date.now();
  while (Date.now() - start < 5000) {
    if (cond()) return true;
    await sleep(5);
  }
  return cond();
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

// A commit that reads the clock at or past its batch's deadline flushes the batch before it plans
// (validated-cow.md §4.1 rule 3), and starts the next batch itself.
test("a relaxed deadline flushes before the next commit", () => {
  const { clock, env } = manualEnv();
  const { db, path } = relaxedDbTimed("deadline.jed", undefined, 1000, env);
  try {
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
    const rec = record(db);
    db.execute("INSERT INTO t VALUES (1)");
    clock.advance(999_999n);
    db.execute("INSERT INTO t VALUES (2)");
    assert.equal(rec.syncs, 0, "a commit before the deadline flushed");
    clock.advance(1n);
    const before = db.version;
    db.execute("INSERT INTO t VALUES (3)");
    assert.equal(rec.syncs, 1, "the commit at the deadline did not flush first");
    // The flush made `before` the floor, so the new commit wrote the other slot and is pending.
    assert.equal(metaTxidAt(path, Number(before & 1n)), before);
    assert.equal(metaTxidAt(path, Number((before & 1n) ^ 1n)), db.version);
    assert.ok(pending(db), "the commit after the flush is not pending");
    // Its batch started at the reading of the flushing commit: the next deadline is a whole interval on.
    clock.advance(999_999n);
    db.execute("INSERT INTO t VALUES (4)");
    assert.equal(rec.syncs, 1, "the new batch ended early");
  } finally {
    db.close();
  }
});

// The idle flusher flushes a batch no commit follows once the clock passes its deadline, and never
// before (validated-cow.md §4.1 rule 5).
test("the idle flusher waits for the clock", async () => {
  const { clock, env } = manualEnv();
  const { db } = relaxedDbTimed("idle.jed", undefined, 20, env);
  try {
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
    const rec = record(db);
    db.execute("INSERT INTO t VALUES (1)");
    await sleep(150);
    assert.equal(rec.syncs, 0, "the flusher flushed before the clock passed the deadline");
    assert.ok(pending(db));
    clock.advance(20_000n);
    assert.ok(await eventually(() => !pending(db)), "the idle flusher never flushed the batch");
    assert.equal(rec.syncs, 1);
  } finally {
    db.close();
  }
});

// The same commits and clock readings write the same file whether the idle flusher ran each flush or
// the next commit did (validated-cow.md §4.1).
test("the idle flusher writes the same bytes", async () => {
  const images: Uint8Array[] = [];
  for (const background of [true, false]) {
    const { clock, env } = manualEnv();
    const { db, path } = relaxedDbTimed(`idle${background}.jed`, 256, 10, env);
    if (!background) {
      setDurability(storage(db), { relaxed: true, flushIntervalMs: 10, idleFlush: false });
    }
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY, v text)");
    for (let i = 1; i <= 40; i++) {
      db.execute(`INSERT INTO t VALUES (${i}, repeat('v', ${i % 30}))`);
      if (i % 6 === 0) db.execute(`DELETE FROM t WHERE id = ${i - 2}`);
      if (i % 5 === 0) {
        clock.advance(10_000n);
        if (background) {
          assert.ok(
            await eventually(() => !pending(db)),
            `the idle flusher never flushed after commit ${i}`,
          );
        }
      }
    }
    db.close();
    images.push(readFileSync(path));
  }
  assert.deepEqual(images[0], images[1], "the idle flusher changed the file's bytes");
});

// A batch's deadline is per database: an attachment follows its handle's interval and clock but keeps
// its own batch.
test("a relaxed deadline is per database", () => {
  const { clock, env } = manualEnv();
  const { db } = relaxedDbTimed("deadline-main.jed", undefined, 1000, env);
  try {
    const other = join(dir, "deadline-other.jed");
    createDatabase({ path: other, environment: env, locking: "none" }).close();
    db.attach("o", { file: true, path: other, locking: "none" });
    db.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
    clock.advance(500_000n);
    db.execute("CREATE TABLE o.u (id i32 PRIMARY KEY)");
    clock.advance(500_000n);
    // Main's deadline has passed; the attachment's has not.
    db.execute("INSERT INTO o.u VALUES (1)");
    const attachedPending = () =>
      (
        db as unknown as { core: { attachments: Map<string, { storage: Engine }> } }
      ).core.attachments
        .get("o")!
        .storage.paging!.hasPending();
    assert.ok(attachedPending(), "the attachment's batch ended at main's deadline");
    assert.ok(pending(db), "main flushed without a commit of its own");
    db.execute("INSERT INTO t VALUES (1)");
    assert.ok(attachedPending());
  } finally {
    db.close();
  }
});

// The idle flusher never flushes while one of the domain's commits runs, even when the commit awaits
// across the deadline (validated-cow.md §4.1 rule 4, ts-runtime.md §11): it waits for the commit to end.
test("the idle flusher waits for an awaiting commit", async () => {
  const clock = manualClock(0n);
  const deadline = new BatchDeadline(5);
  let batch = true;
  let flushes = 0;
  const target = {
    pending: () => batch,
    flush: () => {
      flushes++;
      batch = false;
      return true;
    },
  };
  deadline.enter();
  deadline.afterCommit(true, true, clock.nowMicros(), target, clock);
  clock.advance(5_000n);
  await sleep(50);
  assert.equal(flushes, 0, "the idle flusher flushed inside a commit");
  deadline.leave();
  assert.ok(
    await eventually(() => flushes === 1),
    "the idle flusher never flushed after the commit",
  );
  deadline.stop();
});

// The maxStorageBytes precheck of a commit that also writes an in-memory attachment plans a pending
// batch's reuse restriction, while the commit itself may first flush the batch at its deadline
// (validated-cow.md §4.1) and plan without it. The answer cannot differ: the file form limits live
// pages (memory.md §8.7), which do not depend on which page ids a commit takes. Run the same
// transactions with no batching, with batches, and with a deadline flush before every commit: every
// 54P06 lands on the same transaction. Mirrors the Rust test.
test("the storage precheck ignores the deadline", () => {
  const runs: string[][] = [];
  for (const [k, mode] of ["on", "batched", "deadline"].entries()) {
    const { clock, env } = manualEnv();
    const db =
      mode === "on"
        ? createDatabase({
            path: join(dir, `precheck${k}.jed`),
            pageSize: 512,
            environment: env,
            locking: "none",
          })
        : relaxedDbTimed(`precheck${k}.jed`, 512, mode === "deadline" ? 1 : 0, env).db;
    try {
      db.attach("m", attachMemory(), false);
      db.execute("CREATE TABLE t (id i64 PRIMARY KEY, v text)");
      db.execute("CREATE TABLE m.log (id i64 PRIMARY KEY)");
      db.setMaxStorageBytes("main", 40n * 512n);
      const outcomes: string[] = [];
      for (let i = 0; i < 120; i++) {
        let sql: string;
        if (i % 4 < 2) {
          sql = `INSERT INTO t SELECT g, md5(g::text) || md5((g + 1)::text) FROM generate_series(${i * 10}, ${i * 10 + 9}) g`;
        } else if (i % 4 === 2) {
          sql = `DELETE FROM t WHERE id % 50 = ${i % 50}`;
        } else {
          sql = `UPDATE t SET v = left(md5(v), ${1 + (i % 20)}) || v WHERE id % 5 = 0`;
        }
        const s = db.session();
        let code = "";
        try {
          s.begin(true);
          s.execute(sql);
          s.execute(`INSERT INTO m.log VALUES (${i})`);
          s.commit();
        } catch (e) {
          if (!(e instanceof EngineError)) throw e;
          code = e.code();
        } finally {
          s.close();
        }
        outcomes.push(code);
        clock.advance(1_000n);
      }
      assert.ok(outcomes.includes("54P06"), `${mode}: the limit never tripped`);
      assert.ok(outcomes.includes(""), `${mode}: every commit failed`);
      runs.push(outcomes);
    } finally {
      db.close();
    }
  }
  assert.deepEqual(runs[1], runs[0], "batching moved a 54P06");
  assert.deepEqual(runs[2], runs[0], "the deadline flush moved a 54P06");
});
