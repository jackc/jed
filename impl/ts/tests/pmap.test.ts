// Persistent (copy-on-write) ordered map (spec/design/transactions.md §3, decision B1). Verified
// against a reference map and for structural-sharing snapshot independence — the property the
// transaction model relies on.

import assert from "node:assert/strict";
import { test } from "node:test";
import { PMap } from "../src/pmap.ts";
import { intValue } from "../src/lib.ts";
import type { Row } from "../src/storage.ts";

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

test("pmap: insert/get/remove vs a reference map", () => {
  const pm = new PMap();
  const ref = new Map<string, Row>();
  const n = 4000;

  for (const k of shuffled(n)) {
    const had = pm.insert(key(k), row(k)) !== undefined;
    const refHad = ref.has(keyStr(key(k)));
    assert.equal(had, refHad, `insert 'had' mismatch at ${k}`);
    ref.set(keyStr(key(k)), row(k));
  }
  assert.equal(pm.size, ref.size);
  for (let k = 0; k < n; k++) {
    assert.deepEqual(pm.get(key(k)), ref.get(keyStr(key(k))));
  }

  // Iteration is in ascending key order and matches the reference.
  const { keys, vals } = pm.inorder();
  assert.equal(keys.length, ref.size);
  for (let i = 1; i < keys.length; i++) {
    assert.ok(keyStr(keys[i - 1]) < keyStr(keys[i]), "iteration not in key order");
  }
  for (let i = 0; i < keys.length; i++) {
    assert.deepEqual(vals[i], ref.get(keyStr(keys[i])));
  }

  // Overwrite returns the old value and does not change size (kept in sync with the reference).
  const before = pm.size;
  assert.deepEqual(pm.insert(key(7), row(777)), row(7));
  ref.set(keyStr(key(7)), row(777));
  assert.equal(pm.size, before);

  for (const k of shuffled(n)) {
    const got = pm.remove(key(k));
    const want = ref.get(keyStr(key(k)));
    ref.delete(keyStr(key(k)));
    assert.deepEqual(got, want, `remove mismatch at ${k}`);
  }
  assert.equal(pm.size, 0);
  assert.equal(pm.remove(key(123)), undefined);
});

test("pmap: clone is an independent snapshot", () => {
  let base = new PMap();
  for (let k = 0; k < 2000; k++) base.insert(key(k), row(k));
  const snap = base.clone();

  // Mutate a separate clone heavily; the snapshot must be untouched.
  const other = base.clone();
  for (let k = 0; k < 2000; k++) other.insert(key(k), row(-k)); // overwrite every value
  for (let k = 2000; k < 3000; k++) other.insert(key(k), row(k)); // grow
  for (let k = 0; k < 500; k++) other.remove(key(k)); // shrink

  assert.equal(snap.size, 2000);
  for (let k = 0; k < 2000; k++) assert.deepEqual(snap.get(key(k)), row(k));
  assert.equal(other.size, 2500);
  assert.equal(other.get(key(0)), undefined);
  assert.deepEqual(other.get(key(1000)), row(-1000));
  assert.deepEqual(other.get(key(2500)), row(2500));
});

test("pmap: empty and single", () => {
  const pm = new PMap();
  assert.equal(pm.size, 0);
  assert.equal(pm.get(key(1)), undefined);
  assert.equal(pm.remove(key(1)), undefined);
  assert.equal(pm.insert(key(1), row(1)), undefined);
  assert.deepEqual(pm.get(key(1)), row(1));
  assert.deepEqual(pm.remove(key(1)), row(1));
  assert.equal(pm.size, 0);
});
