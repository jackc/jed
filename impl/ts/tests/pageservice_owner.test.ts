// The TS page-service owner's internals the process corpus cannot reach (spec/pageservice/README.md
// §4, §5.2; spec/design/page-service.md §14): the shared-memory pin table and written-index window the
// engine thread and the service worker share, and the endpoint path selection.

import assert from "node:assert/strict";
import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";

import { PS_INVALIDATION_WINDOW_COMMITS } from "../src/pageservice/codec.ts";
import { endpointPath } from "../src/pageservice/endpoint.ts";
import { ServiceState, WINDOW_ARENA } from "../src/pageservice/shared_state.ts";

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
