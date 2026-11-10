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

test("MemoryBlockStore shares aligned page reads and never edits a stored block", () => {
  // A 256-byte page size, named at the meta header's offset 8.
  const image = new Uint8Array(512);
  new DataView(image.buffer).setUint32(8, 256, false);
  image[300] = 7;
  const store = MemoryBlockStore.owning(image);

  const page = store.readAt(256, 256);
  assert.equal(page, store.readAt(256, 256)); // the stored block itself, not a copy
  assert.equal(page[44], 7);

  // A rewrite installs a fresh block: the page a reader holds keeps the bytes it read.
  store.writeAt(300, Uint8Array.of(9));
  assert.equal(page[44], 7);
  assert.equal(store.readAt(256, 256)[44], 9);

  // Growth appends zero blocks; a shrink into a block zeroes its cut tail for a later growth.
  store.setSize(1024);
  assert.deepEqual([...store.readAt(512, 4)], [0, 0, 0, 0]);
  store.setSize(300);
  store.setSize(512);
  assert.equal(store.readAt(256, 256)[44], 0);
  assert.equal(store.readAt(0, 12)[10], 1);
});
