import assert from "node:assert/strict";
import { test } from "node:test";
import { MemoryBlockStore } from "../src/memoryblockstore.ts";

test("MemoryBlockStore grows with zero fill and returns copied reads", () => {
  const store = new MemoryBlockStore(Uint8Array.of(1, 2, 3));

  store.setSize(6);
  assert.deepEqual([...store.readAt(0, 6)], [1, 2, 3, 0, 0, 0]);

  store.writeAt(2, Uint8Array.of(9, 8, 7));
  assert.deepEqual([...store.readAt(0, 6)], [1, 2, 9, 8, 7, 0]);

  const copy = store.readAt(0, 3);
  copy[0] = 99;
  assert.deepEqual([...store.readAt(0, 3)], [1, 2, 9]);

  store.setSize(4);
  assert.equal(store.size(), 4);
  assert.deepEqual([...store.readAt(0, 4)], [1, 2, 9, 8]);
});

test("MemoryBlockStore short read fails closed", () => {
  const store = new MemoryBlockStore(Uint8Array.of(1, 2, 3));
  assert.throws(() => store.readAt(2, 2), /short read/);
});
