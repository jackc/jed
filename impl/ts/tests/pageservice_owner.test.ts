// The TS page-service owner's internals the process corpus cannot reach (spec/pageservice/README.md
// §3.5, §4, §5.2; spec/design/page-service.md §14): the shared-memory pin table, written-index and
// resolution windows the engine thread and the service worker share, the CommitDelta validation, and
// the endpoint path selection.

import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import {
  buildCommitManifest,
  crc64Update,
  manifestEntriesOf,
  manifestInlineCapacity,
  manifestOverflowCapacity,
} from "../src/commit_manifest.ts";
import "../src/crc32_node.ts";
import { makePage, metaPage } from "../src/format.ts";
import {
  PS_COMMIT_STATUS_COMMITTED,
  PS_COMMIT_STATUS_NOT_COMMITTED,
  PS_COMMIT_STATUS_UNKNOWN,
  PS_INVALIDATION_WINDOW_COMMITS,
  type PsCommitBegin,
  type PsIndexedPage,
} from "../src/pageservice/codec.ts";
import { Delta, type DeltaBase, DeltaFailure, legal } from "../src/pageservice/delta.ts";
import { endpointPath } from "../src/pageservice/endpoint.ts";
import {
  ACK_CRASH,
  ACK_HANG_UP,
  ACK_NORMAL,
  OWNER_SERVING,
  OWNER_YIELDING,
  PS_RESOLUTION_WINDOW_COMMITS,
  ServiceState,
  WINDOW_ARENA,
} from "../src/pageservice/shared_state.ts";

function meta(pageCount: number): Uint8Array {
  const m = new Uint8Array(256);
  new DataView(m.buffer).setUint32(24, pageCount);
  return m;
}

test("pageservice state: pins are counted per txid and feed the oldest-pinned watermark", () => {
  const s = new ServiceState();
  s.lock();
  assert.equal(s.oldestPinnedLocked(), null);
  assert.ok(s.pinLocked(7n));
  assert.ok(s.pinLocked(7n));
  assert.ok(s.pinLocked(9n));
  assert.equal(s.oldestPinnedLocked(), 7n);
  s.unpinLocked(7n, 1);
  assert.equal(s.oldestPinnedLocked(), 7n);
  s.unpinLocked(7n, 1);
  assert.equal(s.oldestPinnedLocked(), 9n);
  s.unpinLocked(9n, 1);
  assert.equal(s.oldestPinnedLocked(), null);
  s.unlock();
});

test("pageservice state: a second view of the buffer shares the state", () => {
  const s = new ServiceState();
  const other = new ServiceState(s.buffer);
  s.lock();
  s.resetLocked(3n, meta(10), false);
  s.pinLocked(3n);
  s.unlock();
  other.lock();
  assert.equal(other.committedTxidLocked(), 3n);
  assert.equal(other.pageCountLocked(), 10);
  assert.equal(other.oldestPinnedLocked(), 3n);
  other.unlock();
  const generation = other.fileGeneration();
  s.lock();
  s.resetLocked(4n, meta(5), true);
  s.unlock();
  assert.equal(other.fileGeneration(), generation + 1);
});

test("pageservice state: invalidation lists exactly the indices written after last_txid", () => {
  const s = new ServiceState();
  s.lock();
  s.resetLocked(10n, meta(20), false);
  s.publishLocked(11n, meta(20), [5, 3, 5]);
  s.publishLocked(12n, meta(20), [4, 3]);
  assert.deepEqual(s.invalidationLocked(true, 10n, 12n), { all: false, indices: [3, 4, 5] });
  assert.deepEqual(s.invalidationLocked(true, 11n, 12n), { all: false, indices: [3, 4] });
  assert.deepEqual(s.invalidationLocked(true, 12n, 12n), { all: false, indices: [] });
  // Another incarnation, no cache, a cache from the future, or one older than the window.
  for (const [same, last] of [
    [false, 11n],
    [true, 0n],
    [true, 13n],
    [true, 9n],
  ] as const) {
    assert.deepEqual(s.invalidationLocked(same, last, 12n), { all: true, indices: [] });
  }
  s.unlock();
});

test("pageservice state: the window keeps the last 256 commits and drops older ones", () => {
  const s = new ServiceState();
  s.lock();
  s.resetLocked(0n, meta(20), false);
  for (let t = 1; t <= PS_INVALIDATION_WINDOW_COMMITS + 10; t++) {
    s.publishLocked(BigInt(t), meta(20), [t + 1]);
  }
  const newest = BigInt(PS_INVALIDATION_WINDOW_COMMITS + 10);
  assert.equal(s.invalidationLocked(true, 9n, newest).all, true);
  const kept = s.invalidationLocked(true, 10n, newest);
  assert.equal(kept.all, false);
  assert.equal(kept.indices.length, PS_INVALIDATION_WINDOW_COMMITS);
  s.unlock();
});

test("pageservice state: an arena overflow drops the oldest commits, a huge commit everything", () => {
  const s = new ServiceState();
  s.lock();
  s.resetLocked(1n, meta(20), false);
  const half = Array.from({ length: WINDOW_ARENA / 2 + 1 }, (_, i) => i + 2);
  s.publishLocked(2n, meta(20), half);
  s.publishLocked(3n, meta(20), half);
  s.publishLocked(4n, meta(20), [7]);
  // Commit 2 no longer fits beside commit 3, so a cache from txid 1 must drop everything; the commits
  // still retained answer exactly.
  assert.equal(s.invalidationLocked(true, 1n, 4n).all, true);
  assert.deepEqual(s.invalidationLocked(true, 3n, 4n), { all: false, indices: [7] });
  s.publishLocked(
    5n,
    meta(20),
    Array.from({ length: WINDOW_ARENA + 1 }, (_, i) => i + 2),
  );
  assert.equal(s.invalidationLocked(true, 4n, 5n).all, true);
  assert.deepEqual(s.invalidationLocked(true, 5n, 5n), { all: false, indices: [] });
  s.unlock();
});

test("pageservice endpoint: the bundle directory when the path fits, else a runtime directory", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-ps-"));
  try {
    const uid = process.geteuid!();
    const incarnation = Uint8Array.from({ length: 16 }, (_, i) => i + 1);
    const short = join(dir, "a.jed");
    mkdirSync(`${short}.lock`); // the coordination bundle exists before an owner picks its endpoint
    assert.equal(
      endpointPath(short, uid, incarnation),
      join(`${short}.lock`, `uid-${uid}`, "endpoint"),
    );
    const runtime = join(dir, "run");
    const deep = join(dir, "x".repeat(100), "a.jed");
    // XDG_RUNTIME_DIR must be private to the user; an unchecked one is skipped for /tmp/jed-<uid>.
    assert.equal(
      endpointPath(deep, uid, incarnation, "linux", { XDG_RUNTIME_DIR: runtime }),
      join(`/tmp/jed-${uid}`, "jed-0102030405060708.sock"),
    );
    mkdirSync(runtime, { mode: 0o700 });
    assert.equal(
      endpointPath(deep, uid, incarnation, "linux", { XDG_RUNTIME_DIR: runtime }),
      join(runtime, "jed-0102030405060708.sock"),
    );
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// testDelta builds a valid CommitDelta over a committed state of committedPages pages: dirty pages at
// the given indices (the first is the catalog root), its meta, and its manifest chain.
function testDelta(
  ps: number,
  committedPages: number,
  indices: number[],
): { cb: PsCommitBegin; body: PsIndexedPage[] } {
  let pageCount = committedPages;
  const dirty = indices.map((index, i) => {
    pageCount = Math.max(pageCount, index + 1);
    return { index, bytes: makePage(ps, i === 0 ? 1 : 2, 1, 0, Uint8Array.of(i, i >> 8, 7)) };
  });
  const left = Math.max(0, dirty.length - manifestInlineCapacity(ps));
  const overflow = Math.ceil(left / manifestOverflowCapacity(ps));
  const ids: number[] = [];
  for (let i = 0; i < overflow; i++) ids.push(pageCount++);
  const manifest = buildCommitManifest(ps, 6n, manifestEntriesOf(dirty), ids);
  const meta = metaPage(ps, 6n, indices[0]!, pageCount, 0, indices.length, manifest);
  return {
    cb: {
      kind: "commit_begin",
      txid: 6n,
      meta,
      manifestPages: manifest.pages.map((pg) => ({ index: pg.index, page: pg.bytes })),
    },
    body: dirty.map((pg) => ({ index: pg.index, page: pg.bytes })),
  };
}

// applyDelta runs the owner's three checks over a delta, rethrowing the first failure.
function applyDelta(base: DeltaBase, cb: PsCommitBegin, body: PsIndexedPage[]): void {
  const d = Delta.begin(base, cb);
  for (const pg of body) {
    if (!legal(base, pg.index, d.parsed.pageCount)) {
      throw new DeltaFailure("accepted an illegal page");
    }
    d.checkPage(pg);
  }
  d.complete();
}

function cloneDelta(
  cb: PsCommitBegin,
  body: PsIndexedPage[],
): { cb: PsCommitBegin; body: PsIndexedPage[] } {
  return {
    cb: {
      kind: "commit_begin",
      txid: cb.txid,
      meta: cb.meta.slice(),
      manifestPages: cb.manifestPages.map((pg) => ({ index: pg.index, page: pg.page.slice() })),
    },
    body: body.map((pg) => ({ index: pg.index, page: pg.page.slice() })),
  };
}

test("pageservice delta: a valid delta passes; each structural fault is refused", () => {
  const ps = 512;
  const base: DeltaBase = { committedTxid: 5n, pageSize: ps, pageCount: 10, granted: new Set([3]) };
  // 40 dirty pages overflow the 512-byte meta's 37 inline entries into one manifest page.
  const indices = [12, 3];
  for (let i = 13; indices.length < 40; i++) indices.push(i);
  const { cb, body } = testDelta(ps, 10, indices);
  assert.equal(cb.manifestPages.length, 1);
  applyDelta(base, cb, body);
  type Mutation = (b: DeltaBase, c: PsCommitBegin, pages: PsIndexedPage[]) => PsIndexedPage[];
  const cases: [string, Mutation][] = [
    [
      "txid does not follow",
      (_b, c, p) => {
        c.txid = 7n;
        return p;
      },
    ],
    [
      "committed txid moved",
      (b, _c, p) => {
        b.committedTxid = 4n;
        return p;
      },
    ],
    [
      "no reuse grant",
      (b, _c, p) => {
        b.granted = null;
        return p;
      },
    ],
    [
      "page count shrinks",
      (b, _c, p) => {
        b.pageCount = 80;
        return p;
      },
    ],
    [
      "page size differs",
      (b, _c, p) => {
        b.pageSize = 1024;
        return p;
      },
    ],
    [
      "manifest page missing",
      (_b, c, p) => {
        c.manifestPages = [];
        return p;
      },
    ],
    ["body page missing", (_b, _c, p) => p.slice(1)],
    ["body page twice", (_b, _c, p) => [...p, p[0]!]],
    [
      "unlisted page",
      (_b, _c, p) => {
        p[0]!.index = 99;
        return p;
      },
    ],
    [
      "page swapped",
      (_b, _c, p) => {
        const first = p[0]!.page;
        p[0]!.page = p[1]!.page;
        p[1]!.page = first;
        return p;
      },
    ],
  ];
  for (const [name, mutate] of cases) {
    const b = { ...base };
    const m = cloneDelta(cb, body);
    const pages = mutate(b, m.cb, m.body);
    assert.throws(() => applyDelta(b, m.cb, pages), DeltaFailure, name);
  }
  // Every single-byte change to the meta, the manifest, or a body page is caught: each is covered by
  // a CRC the owner checks before writing anything it guards.
  let rng = 0x9e3779b97f4a7c15n;
  const next = (n: number): number => {
    rng ^= (rng << 13n) & 0xffff_ffff_ffff_ffffn;
    rng ^= rng >> 7n;
    rng ^= (rng << 17n) & 0xffff_ffff_ffff_ffffn;
    return Number(rng % BigInt(n));
  };
  for (let trial = 0; trial < 3000; trial++) {
    const m = cloneDelta(cb, body);
    const which = next(3);
    const target =
      which === 0
        ? m.cb.meta
        : which === 1
          ? m.cb.manifestPages[0]!.page
          : m.body[next(m.body.length)]!.page;
    target[next(target.length)]! ^= 1 + next(255);
    assert.throws(() => applyDelta(base, m.cb, m.body), DeltaFailure, `trial ${trial}`);
  }
});

// A relaxed client's delta is checked against the owner's named pending set (README §3.5): the
// cumulative manifest lists each pending page with its digest, those pages never arrive again, and the
// invalidation entry names only the pages the delta wrote.
test("pageservice delta: a relaxed delta carries the owner's pending set", () => {
  const ps = 512;
  const { cb, body: all } = testDelta(ps, 10, [12, 4, 13]);
  const pending = new Map<number, bigint>();
  const body: PsIndexedPage[] = [];
  for (const pg of all) {
    if (pg.index === 4) pending.set(4, crc64Update(0n, pg.page));
    else body.push(pg);
  }
  const base: DeltaBase = {
    committedTxid: 5n,
    pageSize: ps,
    pageCount: 10,
    granted: new Set(),
    pending,
  };
  applyDelta(base, cb, body);
  assert.deepEqual(Delta.begin(base, cb).written, [12, 13], "written is the delta's own pages");
  const cases: [string, (b: DeltaBase) => PsIndexedPage[]][] = [
    [
      "pending digest differs",
      (b) => {
        b.pending = new Map([[4, pending.get(4)! ^ 1n]]);
        return body;
      },
    ],
    [
      "pending page not listed",
      (b) => {
        b.pending = new Map([
          [4, pending.get(4)!],
          [5, 9n],
        ]);
        return body;
      },
    ],
    ["pending page sent again", () => all],
    [
      "no pending set",
      (b) => {
        b.pending = null;
        return body;
      },
    ],
  ];
  for (const [name, mutate] of cases) {
    const b = { ...base };
    const pages = mutate(b);
    const m = cloneDelta(cb, pages);
    assert.throws(() => applyDelta(b, m.cb, m.body), DeltaFailure, name);
  }
  // A pending page sent again is refused by check B itself, before any legality test.
  const d = Delta.begin(base, cb);
  assert.throws(() => d.checkPage(all.find((pg) => pg.index === 4)!), DeltaFailure);
});

test("pageservice state: the resolution window answers commit_status", () => {
  const s = new ServiceState();
  const metaOf = (txid: bigint): Uint8Array => {
    const m = new Uint8Array(64);
    m[0] = Number(txid & 0xffn);
    return m;
  };
  s.lock();
  s.resetLocked(10n, metaOf(10n), false);
  s.publishLocked(11n, metaOf(11n), [2]);
  s.publishLocked(12n, metaOf(12n), [2]);
  const digest = (txid: bigint): bigint => crc64Update(0n, metaOf(txid));
  assert.equal(s.resolveLocked(11n, digest(11n)), PS_COMMIT_STATUS_COMMITTED);
  // Another commit took the txid; a txid above the committed one; one below the window.
  assert.equal(s.resolveLocked(11n, digest(99n)), PS_COMMIT_STATUS_NOT_COMMITTED);
  assert.equal(s.resolveLocked(13n, digest(13n)), PS_COMMIT_STATUS_NOT_COMMITTED);
  assert.equal(s.resolveLocked(9n, digest(9n)), PS_COMMIT_STATUS_UNKNOWN);
  for (let txid = 13n; txid < 13n + BigInt(PS_RESOLUTION_WINDOW_COMMITS); txid++) {
    s.publishLocked(txid, metaOf(txid), [2]);
  }
  const kept = s.resolutionTxidsLocked();
  assert.equal(kept.length, PS_RESOLUTION_WINDOW_COMMITS);
  assert.equal(kept[0], 13n);
  s.unlock();
});

test("pageservice state: the handoff state and the ack hooks are shared and consumed once", () => {
  const s = new ServiceState();
  const other = new ServiceState(s.buffer);
  assert.equal(other.ownerState(), OWNER_SERVING);
  s.lock();
  s.setOwnerStateLocked(OWNER_YIELDING);
  s.setRequesterLocked(42n);
  s.unlock();
  other.lock();
  assert.equal(other.ownerState(), OWNER_YIELDING);
  assert.equal(other.requesterLocked(), 42n);
  other.unlock();
  assert.equal(other.takeAckHook(), ACK_NORMAL);
  s.armAckHook(ACK_HANG_UP);
  assert.equal(other.takeAckHook(), ACK_HANG_UP);
  assert.equal(other.takeAckHook(), ACK_NORMAL);
  s.armAckHook(ACK_CRASH);
  assert.equal(other.takeAckHook(), ACK_CRASH);
});
