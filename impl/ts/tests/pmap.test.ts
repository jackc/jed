// Persistent (copy-on-write) page-backed B-tree (spec/design/transactions.md §3, decision B1;
// spec/fileformat/format.md). Verified against a reference map, for structural-sharing snapshot
// independence, and for the size-driven node invariants the byte contract relies on.

import assert from "node:assert/strict";
import { test } from "node:test";
import { PMap, unboundedBound } from "../src/pmap.ts";
import type { KeyBound, PNode } from "../src/pmap.ts";
import { intValue } from "../src/lib.ts";
import type { Row } from "../src/storage.ts";

// A small page cap so a few-thousand-entry map is several levels deep — exercises split,
// merge-then-split, root growth and collapse (the in-RAM analog of page_size 256). W is a realistic
// per-entry weight (8-byte key + a ~5-byte int value record), well under RECORD_MAX = (240-12)/2.
const CAP = 240;
const W = 15;
// row() has one value column — the PAX leaf directory overhead scales with it (format.md v23).
const K = 1;

function key(n: number): Uint8Array {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, BigInt(n));
  return b;
}

function row(n: number): Row {
  return [intValue(BigInt(n))];
}

function keyStr(k: Uint8Array): string {
  let s = "";
  for (const b of k) s += String.fromCharCode(b);
  return s;
}

// A deterministic permutation of 0..n (LCG-driven) — no RNG / wall-clock, so reproducible.
function shuffled(n: number): number[] {
  const v = Array.from({ length: n }, (_, i) => i);
  let state = 0x9e3779b97f4a7c15n;
  const mask = (1n << 64n) - 1n;
  for (let i = v.length - 1; i >= 1; i--) {
    state = (state * 6364136223846793005n + 1442695040888963407n) & mask;
    const j = Number((state >> 33n) % BigInt(i + 1));
    [v[i], v[j]] = [v[j], v[i]];
  }
  return v;
}

// Every node (except the root) must fit a page and stay non-empty — the structural invariant the
// byte contract relies on (spec/fileformat/format.md).
function checkInvariants(pm: PMap): void {
  const walk = (n: PNode | null, isRoot: boolean): void => {
    if (n === null) return;
    assert.ok(n.keys.length > 0 || isRoot, "non-root node is empty");
    assert.equal(n.keys.length, n.vals.length);
    assert.equal(n.keys.length, n.weights.length);
    if (n.children.length > 0) {
      assert.equal(n.children.length, n.keys.length + 1, "interior child count");
    }
    let payload = 0;
    for (const w of n.weights) payload += w;
    const nk = n.keys.length;
    // A leaf carries the PAX directories (format.md v23 directoryOverhead(N,K)); an interior its
    // N+1 child pointers.
    payload +=
      n.children.length > 0
        ? 4 * n.children.length
        : 4 * (nk + 1) + 4 * (K + 1) + 4 * (nk + 1) * K - 2 * nk;
    assert.ok(payload <= CAP, `node payload ${payload} exceeds cap ${CAP}`);
    for (const c of n.children) walk(c.node, false);
  };
  walk(pm.rootNode(), true);
}

test("pmap: insert/get/remove vs a reference map", () => {
  const pm = new PMap();
  const ref = new Map<string, Row>();
  const n = 4000;

  for (const k of shuffled(n)) {
    const had = pm.insert(key(k), row(k), W, CAP, K, null) !== undefined;
    const refHad = ref.has(keyStr(key(k)));
    assert.equal(had, refHad, `insert 'had' mismatch at ${k}`);
    ref.set(keyStr(key(k)), row(k));
  }
  assert.equal(pm.size, ref.size);
  checkInvariants(pm);
  for (let k = 0; k < n; k++) {
    assert.deepEqual(pm.get(key(k), null), ref.get(keyStr(key(k))));
  }

  // Iteration is in ascending key order and matches the reference.
  const { keys, vals } = pm.inorder(null);
  assert.equal(keys.length, ref.size);
  for (let i = 1; i < keys.length; i++) {
    assert.ok(keyStr(keys[i - 1]) < keyStr(keys[i]), "iteration not in key order");
  }
  for (let i = 0; i < keys.length; i++) {
    assert.deepEqual(vals[i], ref.get(keyStr(keys[i])));
  }

  // Overwrite returns the old value and does not change size (kept in sync with the reference).
  const before = pm.size;
  assert.deepEqual(pm.insert(key(7), row(777), W, CAP, K, null), row(7));
  ref.set(keyStr(key(7)), row(777));
  assert.equal(pm.size, before);

  // Interleave removes with invariant checks so merge-then-split is exercised mid-stream.
  let step = 0;
  for (const k of shuffled(n)) {
    const got = pm.remove(key(k), CAP, K, null);
    const want = ref.get(keyStr(key(k)));
    ref.delete(keyStr(key(k)));
    assert.deepEqual(got, want, `remove mismatch at ${k}`);
    if (step++ % 257 === 0) checkInvariants(pm);
  }
  assert.equal(pm.size, 0);
  assert.equal(pm.remove(key(123), CAP, K, null), undefined);
});

test("pmap: clone is an independent snapshot", () => {
  const base = new PMap();
  for (let k = 0; k < 2000; k++) base.insert(key(k), row(k), W, CAP, K, null);
  const snap = base.clone();

  // Mutate a separate clone heavily; the snapshot must be untouched.
  const other = base.clone();
  for (let k = 0; k < 2000; k++) other.insert(key(k), row(-k), W, CAP, K, null); // overwrite every value
  for (let k = 2000; k < 3000; k++) other.insert(key(k), row(k), W, CAP, K, null); // grow
  for (let k = 0; k < 500; k++) other.remove(key(k), CAP, K, null); // shrink

  assert.equal(snap.size, 2000);
  for (let k = 0; k < 2000; k++) assert.deepEqual(snap.get(key(k), null), row(k));
  checkInvariants(snap);
  assert.equal(other.size, 2500);
  assert.equal(other.get(key(0), null), undefined);
  assert.deepEqual(other.get(key(1000), null), row(-1000));
  assert.deepEqual(other.get(key(2500), null), row(2500));
  checkInvariants(other);
});

// Wide values (near RECORD_MAX) force tiny fan-out — the stress case for the split point and the
// non-empty-halves guarantee. With weight 100 (≤ RECORD_MAX(240,1) = 106 — the PAX leaf reserves
// 12+16·K, format.md v23) a two-record leaf fits but a third overflows.
test("pmap: wide values keep nodes valid", () => {
  const pm = new PMap();
  for (const k of shuffled(300)) {
    pm.insert(key(k), row(k), 100, CAP, K, null);
    checkInvariants(pm);
  }
  for (const k of shuffled(300)) {
    pm.remove(key(k), CAP, K, null);
    checkInvariants(pm);
  }
  assert.equal(pm.size, 0);
});

test("pmap: empty and single", () => {
  const pm = new PMap();
  assert.equal(pm.size, 0);
  assert.equal(pm.get(key(1), null), undefined);
  assert.equal(pm.remove(key(1), CAP, K, null), undefined);
  assert.equal(pm.insert(key(1), row(1), W, CAP, K, null), undefined);
  assert.deepEqual(pm.get(key(1), null), row(1));
  assert.deepEqual(pm.remove(key(1), CAP, K, null), row(1));
  assert.equal(pm.size, 0);
});

test("pmap: reverse scan is the forward scan reversed", () => {
  // scanRangeRev must yield the EXACT reverse of scanRange's row sequence over a MULTI-LEVEL tree —
  // the interior-node interleaving (separators between children) and the asymmetric inclusive-lo
  // edge that single-leaf conformance tables (the DESC-LIMIT corpus cases) cannot exercise. 200
  // entries at CAP build several levels.
  const pm = new PMap();
  for (let n = 0; n < 200; n++) pm.insert(key(n), row(n), W, CAP, K, null);
  assert.ok(pm.nodeCount() > 2, "test needs a multi-level tree");

  const decode = (k: Uint8Array): number =>
    Number(new DataView(k.buffer, k.byteOffset, 8).getBigUint64(0));
  const collect = (b: KeyBound, rev: boolean): number[] => {
    const out: number[] = [];
    const visit = (k: Uint8Array): boolean => {
      out.push(decode(k));
      return true;
    };
    if (rev) pm.scanRangeRev(b, null, null, visit);
    else pm.scanRange(b, null, null, visit);
    return out;
  };
  const bounds: KeyBound[] = [
    unboundedBound(),
    { lo: key(50), loInc: true, hi: key(150), hiInc: true },
    { lo: key(50), loInc: false, hi: key(150), hiInc: false },
    { lo: key(195), loInc: true, hi: null, hiInc: false },
    { lo: key(100), loInc: true, hi: key(100), hiInc: true },
    { lo: key(73), loInc: true, hi: key(181), hiInc: false },
  ];
  for (let i = 0; i < bounds.length; i++) {
    const fwd = collect(bounds[i]!, false);
    const rev = collect(bounds[i]!, true);
    assert.deepEqual(
      rev,
      [...fwd].reverse(),
      `reverse scan must equal forward-reversed for bound #${i}`,
    );
  }
  // The reverse short-circuit stops from the HIGH end: stopping after 3 visits yields the 3 largest
  // keys descending, faulting no further.
  const got: number[] = [];
  pm.scanRangeRev(unboundedBound(), null, null, (k) => {
    got.push(decode(k));
    return got.length < 3;
  });
  assert.deepEqual(got, [199, 198, 197]);
});

test("pmap: pull cursor (scanRangeIter) matches the push scan", () => {
  // The S2 pull cursor (scanRangeIter / scanRangeRevIter) must yield the EXACT same (key, row)
  // sequence as the push scanRange / scanRangeRev over a MULTI-LEVEL tree — the contract the streaming
  // pipeline (S3) rests on. Internal machinery, not corpus-expressible (CLAUDE.md §10), so it is
  // unit-tested per core against the existing push scan.
  const pm = new PMap();
  for (let n = 0; n < 200; n++) pm.insert(key(n), row(n), W, CAP, K, null);
  assert.ok(pm.nodeCount() > 2, "test needs a multi-level tree");

  const decode = (k: Uint8Array): number =>
    Number(new DataView(k.buffer, k.byteOffset, 8).getBigUint64(0));
  const val = (r: Row): number => {
    const v = r[0]!;
    assert.equal(v.kind, "int", "unexpected row value");
    return Number((v as { kind: "int"; int: bigint }).int);
  };
  type Pair = [number, number];
  // Collect the push scan's sequence as [key, row-value] pairs.
  const pushed = (b: KeyBound, rev: boolean): Pair[] => {
    const out: Pair[] = [];
    const visit = (k: Uint8Array, r: Row): boolean => {
      out.push([decode(k), val(r)]);
      return true;
    };
    if (rev) pm.scanRangeRev(b, null, null, visit);
    else pm.scanRange(b, null, null, visit);
    return out;
  };
  // Drain the pull generator into the same shape.
  const pulled = (b: KeyBound, rev: boolean): Pair[] => {
    const out: Pair[] = [];
    const it = rev ? pm.scanRangeRevIter(b, null, null) : pm.scanRangeIter(b, null, null);
    for (const [k, r] of it) out.push([decode(k), val(r)]);
    return out;
  };
  const bounds: KeyBound[] = [
    unboundedBound(),
    { lo: key(50), loInc: true, hi: key(150), hiInc: true },
    { lo: key(50), loInc: false, hi: key(150), hiInc: false },
    { lo: key(195), loInc: true, hi: null, hiInc: false },
    { lo: key(100), loInc: true, hi: key(100), hiInc: true },
    { lo: key(73), loInc: true, hi: key(181), hiInc: false },
    { lo: key(150), loInc: true, hi: key(50), hiInc: true }, // empty (lo > hi)
  ];
  for (let i = 0; i < bounds.length; i++) {
    for (const rev of [false, true]) {
      assert.deepEqual(
        pulled(bounds[i]!, rev),
        pushed(bounds[i]!, rev),
        `cursor must match scanRange for bound #${i} rev=${rev}`,
      );
    }
  }

  // Early abandonment: pulling only 3 rows then abandoning the generator yields the first 3 of the
  // full sequence (forward and reverse), proving the pull short-circuit (the streaming win).
  for (const rev of [false, true]) {
    const full = pushed(unboundedBound(), rev);
    const it = rev
      ? pm.scanRangeRevIter(unboundedBound(), null, null)
      : pm.scanRangeIter(unboundedBound(), null, null);
    const out: Pair[] = [];
    for (const [k, r] of it) {
      out.push([decode(k), val(r)]);
      if (out.length === 3) break; // break runs the generator's return path — no further faulting
    }
    assert.deepEqual(
      out,
      full.slice(0, 3),
      `early-abandoned cursor must be the prefix (rev=${rev})`,
    );
  }
});
