// Persistent (copy-on-write) page-backed B+tree (spec/design/bplus-reshape.md, decision B1;
// spec/design/transactions.md §3; spec/fileformat/format.md). Verified against a reference map, for
// structural-sharing snapshot independence, and for the size-driven node invariants the byte
// contract relies on (records only in leaves; interior nodes a record-free separator skeleton).
// Mirrors impl/rust/src/pmap.rs `mod tests` and the Go equivalent.

import assert from "node:assert/strict";
import { test } from "node:test";
import { PMap, leafOverhead, unboundedBound } from "../src/pmap.ts";
import type { KeyBound, LeafShape, PNode } from "../src/pmap.ts";
import { intValue } from "../src/lib.ts";
import type { Row } from "../src/storage.ts";

// A small page cap so a few-thousand-entry map is several levels deep — exercises split,
// merge-then-split, root growth and collapse (the in-RAM analog of page_size 256). W is a realistic
// per-entry weight (8-byte key + an 8-byte i64 slot = 16 bytes, well under RECORD_MAX), so a
// 240-byte leaf holds ~12 entries before splitting.
const CAP = 240;
const W = 16;
// row() has one fixed-width value column — the v24 leaf overhead scales with the class mix
// (format.md "Leaf node").
const SHAPE: LeafShape = { fixed: 1, var: 0 };

function key(n: number): Uint8Array {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, BigInt(n));
  return b;
}

function row(n: number): Row {
  return [intValue(BigInt(n))];
}

// pmCount returns pm's exact row count, asserting the map knows it. An in-memory map (built from
// empty by insert) always knows its count; table skeletons restore it from v28 catalog data.
function pmCount(pm: PMap): number {
  const c = pm.getCount();
  assert.notEqual(c, null, "expected a known row count on an in-memory map");
  return Number(c!);
}

function keyStr(k: Uint8Array): string {
  let s = "";
  for (const b of k) s += String.fromCharCode(b);
  return s;
}

function compare(a: Uint8Array, b: Uint8Array): number {
  const n = a.length < b.length ? a.length : b.length;
  for (let i = 0; i < n; i++) {
    if (a[i] !== b[i]) return a[i]! < b[i]! ? -1 : 1;
  }
  return a.length === b.length ? 0 : a.length < b.length ? -1 : 1;
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

// nodePayload mirrors pmap's serialized-size arithmetic (format.md): a leaf is Σ weights +
// leafOverhead(N, shape); an interior node is 8·N + 4 + Σ sep_len (record-free, v24).
function nodePayload(n: PNode, shape: LeafShape): number {
  if (n.children.length === 0) {
    let total = 0;
    for (const w of n.weights) total += w;
    return total + leafOverhead(n.keys.length, shape);
  }
  let seps = 0;
  for (const k of n.keys) seps += k.length;
  return 8 * n.keys.length + 4 + seps;
}

// The structural invariants the byte contract relies on (format.md "Fan-out"): every node fits a
// page; every leaf is non-empty; an interior node has N+1 children (N ≥ 0 only in the degenerate
// near-cap-separator case — these small-key tests never produce it, so N ≥ 1 is asserted); records
// (vals/weights) live only in leaves; all leaves at the same depth; and every key in a subtree
// respects its bounding separators (left < sep ≤ right).
function checkInvariants(pm: PMap): void {
  const walk = (
    n: PNode,
    isRoot: boolean,
    lo: Uint8Array | null,
    hi: Uint8Array | null,
  ): number => {
    if (n.children.length === 0) {
      assert.ok(n.keys.length > 0 || isRoot, "non-root leaf is empty");
      if (!n.packed) assert.equal(n.keys.length, n.vals.length);
      assert.equal(n.keys.length, n.weights.length);
    } else {
      assert.ok(n.keys.length > 0 || isRoot, "0-key interior unexpected");
      assert.equal(n.vals.length, 0, "interior node carries records");
      assert.equal(n.weights.length, 0, "interior node carries weights");
      assert.equal(n.children.length, n.keys.length + 1, "interior child count");
    }
    for (let i = 1; i < n.keys.length; i++) {
      assert.ok(compare(n.keys[i - 1]!, n.keys[i]!) < 0, "keys out of order");
    }
    // Subtree keys respect the bounding separators: lo ≤ key < hi (lo inclusive because a
    // separator equals the right subtree's first key at split time).
    for (const k of n.keys) {
      if (lo !== null) assert.ok(compare(k, lo) >= 0, "key below its subtree's low separator");
      if (hi !== null) assert.ok(compare(k, hi) < 0, "key at/above its subtree's high separator");
    }
    const payload = nodePayload(n, SHAPE);
    assert.ok(payload <= CAP, `node payload ${payload} exceeds cap ${CAP}`);
    if (n.children.length === 0) return 1;
    let depth: number | null = null;
    for (let i = 0; i < n.children.length; i++) {
      const clo = i === 0 ? lo : n.keys[i - 1]!;
      const chi = i === n.keys.length ? hi : n.keys[i]!;
      const child = n.children[i]!.node;
      assert.ok(child !== null, "in-memory tree has no OnDisk children");
      const d = walk(child, false, clo, chi);
      if (depth === null) depth = d;
      else assert.equal(depth, d, "leaves at unequal depth");
    }
    return depth! + 1;
  };
  const root = pm.rootNode();
  if (root !== null) walk(root, true, null, null);
}

test("pmap: insert/get/remove vs a reference map", () => {
  const pm = new PMap();
  const ref = new Map<string, Row>();
  const n = 4000;

  for (const k of shuffled(n)) {
    const had = pm.insert(key(k), row(k), W, CAP, SHAPE, null) !== undefined;
    const refHad = ref.has(keyStr(key(k)));
    assert.equal(had, refHad, `insert 'had' mismatch at ${k}`);
    ref.set(keyStr(key(k)), row(k));
  }
  assert.equal(pmCount(pm), ref.size);
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
  const before = pmCount(pm);
  assert.deepEqual(pm.insert(key(7), row(777), W, CAP, SHAPE, null), row(7));
  ref.set(keyStr(key(7)), row(777));
  assert.equal(pmCount(pm), before);
  assert.deepEqual(pm.get(key(7), null), row(777));

  // Interleave removes with invariant checks so merge-then-split is exercised mid-stream.
  let step = 0;
  for (const k of shuffled(n)) {
    const got = pm.remove(key(k), CAP, SHAPE, null);
    const want = ref.get(keyStr(key(k)));
    ref.delete(keyStr(key(k)));
    assert.deepEqual(got, want, `remove mismatch at ${k}`);
    if (step++ % 257 === 0) checkInvariants(pm);
  }
  assert.equal(pmCount(pm), 0);
  assert.equal(pm.inorder(null).keys.length, 0);
  assert.equal(pm.remove(key(123), CAP, SHAPE, null), undefined);
});

test("pmap: clone is an independent snapshot", () => {
  const base = new PMap();
  for (let k = 0; k < 2000; k++) base.insert(key(k), row(k), W, CAP, SHAPE, null);
  const snap = base.clone();

  // Mutate a separate clone heavily; the snapshot must be untouched.
  const other = base.clone();
  for (let k = 0; k < 2000; k++) other.insert(key(k), row(-k), W, CAP, SHAPE, null); // overwrite every value
  for (let k = 2000; k < 3000; k++) other.insert(key(k), row(k), W, CAP, SHAPE, null); // grow
  for (let k = 0; k < 500; k++) other.remove(key(k), CAP, SHAPE, null); // shrink

  assert.equal(pmCount(snap), 2000);
  for (let k = 0; k < 2000; k++) assert.deepEqual(snap.get(key(k), null), row(k));
  checkInvariants(snap);
  assert.equal(pmCount(other), 2500);
  assert.equal(other.get(key(0), null), undefined);
  assert.deepEqual(other.get(key(1000), null), row(-1000));
  assert.deepEqual(other.get(key(2500), null), row(2500));
  checkInvariants(other);
});

test("pmap: empty and single", () => {
  const pm = new PMap();
  assert.equal(pmCount(pm), 0);
  assert.equal(pm.get(key(1), null), undefined);
  assert.equal(pm.remove(key(1), CAP, SHAPE, null), undefined);
  assert.equal(pm.insert(key(1), row(1), W, CAP, SHAPE, null), undefined);
  assert.deepEqual(pm.get(key(1), null), row(1));
  assert.deepEqual(pm.remove(key(1), CAP, SHAPE, null), row(1));
  assert.equal(pmCount(pm), 0);
  assert.equal(pm.rootNode(), null);
});

// Wide records (near RECORD_MAX) force tiny fan-out — the stress case for the split point and the
// fit guarantee. With weight 100 (≤ RECORD_MAX(240, 1) = 106) a two-record leaf fits but a third
// overflows.
test("pmap: wide values keep nodes valid", () => {
  const pm = new PMap();
  for (const k of shuffled(300)) {
    pm.insert(key(k), row(k), 100, CAP, SHAPE, null);
    checkInvariants(pm);
  }
  for (const k of shuffled(300)) {
    pm.remove(key(k), CAP, SHAPE, null);
    checkInvariants(pm);
  }
  assert.equal(pmCount(pm), 0);
});

// Near-cap KEYS (the max-size-separator case, format.md "Interior node"): separators are key
// copies, so two of them overflow an interior node, forcing the pinned degenerate N = 2 → m = 1
// split and legal 0-key interiors. The map must stay correct through inserts, scans, and removes
// (a looser invariant check — 0-key interiors are legal here).
test("pmap: near-cap keys force degenerate interior nodes", () => {
  // Index-tree shape: zero value columns, record = key alone. RECORD_MAX(0) = (240-12)/2 = 114;
  // keys of 110 bytes keep records under the cap while two separators (2·110 + 20) overflow an
  // interior.
  const shape: LeafShape = { fixed: 0, var: 0 };
  const bigKey = (n: number): Uint8Array => {
    const k = new Uint8Array(110).fill(0xab);
    new DataView(k.buffer).setBigUint64(0, BigInt(n));
    return k;
  };
  const pm = new PMap();
  const ref = new Map<string, Row>();
  for (const k of shuffled(60)) {
    pm.insert(bigKey(k), [], 110, CAP, shape, null);
    ref.set(keyStr(bigKey(k)), []);
  }
  assert.equal(pmCount(pm), ref.size);
  // Structure: fits + routing correctness (0-key interiors allowed).
  const walk = (n: PNode): void => {
    assert.ok(nodePayload(n, shape) <= CAP, "node overflows its page");
    if (n.children.length > 0) {
      assert.equal(n.children.length, n.keys.length + 1);
      for (const c of n.children) {
        assert.ok(c.node !== null, "in-memory tree has no OnDisk children");
        walk(c.node);
      }
    }
  };
  const root = pm.rootNode();
  assert.ok(root !== null);
  walk(root);
  const { keys } = pm.inorder(null);
  const want = [...ref.keys()].sort();
  assert.deepEqual(
    keys.map((k) => keyStr(k)),
    want,
  );
  for (let k = 0; k < 60; k++) {
    assert.ok(pm.get(bigKey(k), null) !== undefined);
  }
  for (const k of shuffled(60)) {
    assert.deepEqual(pm.remove(bigKey(k), CAP, shape, null), []);
  }
  assert.equal(pmCount(pm), 0);
});

test("pmap: direct point get counts one descent and reconstruction", () => {
  const pm = new PMap();
  for (let k = 0; k < 2000; k++) pm.insert(key(k), row(k), W, CAP, SHAPE, null);
  assert.ok(pm.height() > 1, "test needs a multi-level tree");

  const hit = pm.getCounted(key(777), null);
  assert.deepEqual(hit.row, row(777));
  assert.equal(hit.nodes, pm.height(), "one root-to-leaf descent");
  assert.equal(hit.rowsReconstructed, 1, "a hit reconstructs exactly one row");

  const miss = pm.getCounted(key(3000), null);
  assert.equal(miss.row, undefined);
  assert.equal(miss.nodes, pm.height(), "a miss still descends once");
  assert.equal(miss.rowsReconstructed, 0, "a miss reconstructs no row");
});

// The bounded scan yields exactly the in-bound rows, in order, and the counted nodes match
// overlapNodeCount; the pull cursor and the reverse walk agree with it.
test("pmap: bounded scans and cursor agree", () => {
  const pm = new PMap();
  for (const k of shuffled(2000)) pm.insert(key(k), row(k), W, CAP, SHAPE, null);
  const b: KeyBound = { lo: key(500), loInc: true, hi: key(1500), hiInc: false };
  const { keys, vals, nodes } = pm.rangeEntriesCounted(b, null);
  assert.equal(keys.length, 1000);
  assert.deepEqual(keys[0], key(500));
  assert.deepEqual(keys[999], key(1499));
  assert.equal(nodes, pm.overlapNodeCount(b));

  // Push walk agrees.
  const push: [Uint8Array, Row][] = [];
  pm.scanRange(b, null, (k: Uint8Array, r: Row) => {
    push.push([k, r]);
    return true;
  });
  assert.deepEqual(
    push,
    keys.map((k, i) => [k, vals[i]!]),
  );

  // Reverse push walk is the exact reverse.
  const rev: [Uint8Array, Row][] = [];
  pm.scanRangeRev(b, null, (k: Uint8Array, r: Row) => {
    rev.push([k, r]);
    return true;
  });
  assert.deepEqual(rev, [...push].reverse());

  // Pull cursor agrees, both directions.
  const fwd = [...pm.scanRangeIter(b, null)];
  assert.deepEqual(fwd, push);
  const bwd = [...pm.scanRangeRevIter(b, null)];
  assert.deepEqual(bwd, rev);

  // Exclusive lo / inclusive hi.
  const b2: KeyBound = { lo: key(500), loInc: false, hi: key(1500), hiInc: true };
  const got = pm.rangeEntries(b2, null);
  assert.deepEqual(got.keys[0], key(501));
  assert.deepEqual(got.keys[got.keys.length - 1], key(1500));
  assert.equal(got.keys.length, 1000);
});

test("pmap: reverse scan is the forward scan reversed", () => {
  // scanRangeRev must yield the EXACT reverse of scanRange's row sequence over a MULTI-LEVEL tree —
  // the windowed interior descent that single-leaf conformance tables (the DESC-LIMIT corpus cases)
  // cannot exercise. 200 entries at CAP build several levels.
  const pm = new PMap();
  for (let n = 0; n < 200; n++) pm.insert(key(n), row(n), W, CAP, SHAPE, null);
  assert.ok(pm.nodeCount() > 2, "test needs a multi-level tree");

  const decode = (k: Uint8Array): number =>
    Number(new DataView(k.buffer, k.byteOffset, 8).getBigUint64(0));
  const collect = (b: KeyBound, rev: boolean): number[] => {
    const out: number[] = [];
    const visit = (k: Uint8Array): boolean => {
      out.push(decode(k));
      return true;
    };
    if (rev) pm.scanRangeRev(b, null, visit);
    else pm.scanRange(b, null, visit);
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
  pm.scanRangeRev(unboundedBound(), null, (k: Uint8Array) => {
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
  for (let n = 0; n < 200; n++) pm.insert(key(n), row(n), W, CAP, SHAPE, null);
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
    if (rev) pm.scanRangeRev(b, null, visit);
    else pm.scanRange(b, null, visit);
    return out;
  };
  // Drain the pull generator into the same shape.
  const pulled = (b: KeyBound, rev: boolean): Pair[] => {
    const out: Pair[] = [];
    const it = rev ? pm.scanRangeRevIter(b, null) : pm.scanRangeIter(b, null);
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
      ? pm.scanRangeRevIter(unboundedBound(), null)
      : pm.scanRangeIter(unboundedBound(), null);
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
