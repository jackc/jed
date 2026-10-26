// KeySet / KeyMap are Set<string> / Map<string, V> that bucket long keys by a content hash (V8 hashes a
// string of 16384+ units by its length alone). Membership must be exactly string equality.

import assert from "node:assert/strict";
import { test } from "node:test";
import { KeyMap, KeySet } from "../src/keyset.ts";

const long = "x".repeat(20000);
// Same-length long keys differing only at the end, a long key equal in content but a distinct string
// object, short keys, and keys starting with U+0000 (which could otherwise look like a surrogate).
const keys = [
  long + "1",
  long + "2",
  long + "3",
  "short",
  "",
  "\u00000",
  "\u0000",
  "x".repeat(8192),
  "x".repeat(8193),
];

test("KeySet membership is string equality", () => {
  const set = new KeySet();
  for (const k of keys) {
    assert.equal(set.has(k), false, JSON.stringify(k.slice(-3)));
    set.add(k);
    set.add(k.split("").join("")); // equal content, separate string
  }
  assert.equal(set.size, keys.length);
  for (const k of keys) assert.equal(set.has(k.split("").join("")), true);
  assert.equal(set.has(long + "4"), false);
  assert.equal(set.has(long), false);
  set.clear();
  assert.equal(set.size, 0);
  assert.equal(set.has(keys[0]!), false);
});

test("KeyMap get/set/has and insertion-ordered entries", () => {
  const map = new KeyMap<number>();
  for (const [i, k] of keys.entries()) map.set(k, i);
  map.set(keys[1]!.split("").join(""), 100); // replaces in place, keeping its position
  assert.equal(map.size, keys.length);
  assert.equal(map.get(keys[1]!), 100);
  assert.equal(map.get(keys[0]!), 0);
  assert.equal(map.has(long + "4"), false);
  assert.equal(map.get("\u00001"), undefined);
  assert.deepEqual(
    [...map].map(([k, v]) => [k, v]),
    keys.map((k, i) => [k, i === 1 ? 100 : i]),
  );
});
