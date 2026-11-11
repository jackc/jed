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

// rowKey shares long top-level text/json bodies as SegKey segments; its encoding must stay
// distinctRowKey's, and a KeySet/KeyMap must treat equal rows as one key and unequal ones apart.
test("rowKey encodes as distinctRowKey and dedups by value", async () => {
  const { distinctRowKey, rowKey } = await import("../src/executor.ts");
  const { SegKey } = await import("../src/keyset.ts");
  const { textValue, intValue, nullValue } = await import("../src/value.ts");
  const big = "y".repeat(5000);
  const rows = [
    [textValue(big + "a")],
    [textValue(big + "b")],
    [intValue(1n), textValue(big + "a"), nullValue()],
    [intValue(1n), textValue(big + "a"), textValue(big + "a")],
    [{ kind: "json" as const, text: big + "a" }],
    [textValue("short"), intValue(2n)],
    [textValue("")],
  ];
  for (const row of rows) {
    const k = rowKey(row);
    const flat = typeof k === "string" ? k : k.flat();
    assert.equal(flat, distinctRowKey(row));
  }
  assert.ok(rowKey(rows[0]!) instanceof SegKey);
  assert.equal(typeof rowKey(rows[5]!), "string");
  const set = new KeySet<string | InstanceType<typeof SegKey>>();
  const map = new KeyMap<number, string | InstanceType<typeof SegKey>>();
  for (const [i, row] of rows.entries()) {
    set.add(rowKey(row));
    map.set(rowKey(row), i);
  }
  // Equal content in separate strings is one key; text and json of the same body are not.
  for (const [i, row] of rows.entries()) {
    const copy = row.map((v) =>
      "text" in v ? { ...v, text: v.text.split("").join("") } : v,
    ) as typeof row;
    assert.equal(set.has(rowKey(copy)), true);
    assert.equal(map.get(rowKey(copy)), i);
  }
  assert.equal(set.size, rows.length);
  assert.equal(set.has(rowKey([textValue(big + "c")])), false);
});
