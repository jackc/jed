// Bounded CLOCK buffer pool (spec/design/pager.md §3) — a hit returns the cached node, the resident
// set never exceeds capacity, CLOCK grants a referenced page a second chance, and capacity-one evicts
// every time. Mirrors the Rust/Go pool tests.

import assert from "node:assert/strict";
import { test } from "node:test";
import { BufferPool } from "../src/bufferpool.ts";
import type { PNode } from "../src/pmap.ts";

// A sentinel leaf node carrying the page id, so a test can tell which page was returned.
function leaf(page: number): PNode {
  return { keys: [], vals: [], weights: [], children: [], page };
}

test("bufferpool: a hit returns the cached node without reloading", () => {
  const pool = new BufferPool(4);
  let loads = 0;
  const load = (p: number) => () => {
    loads++;
    return leaf(p);
  };
  assert.equal(pool.getOrLoad(7, load(70)).page, 70);
  assert.equal(pool.getOrLoad(7, load(70)).page, 70);
  assert.equal(loads, 1, "second access is a cache hit");
  assert.equal(pool.resident(), 1);
});

test("bufferpool: resident set never exceeds capacity", () => {
  const pool = new BufferPool(3);
  let loads = 0;
  for (let p = 0; p < 100; p++) {
    pool.getOrLoad(p, () => {
      loads++;
      return leaf(p);
    });
    assert.ok(pool.resident() <= 3, `resident ${pool.resident()} exceeds capacity`);
  }
  assert.equal(loads, 100, "every distinct page was a miss");
});

test("bufferpool: CLOCK gives a referenced page a second chance", () => {
  // Fill {0,1,2}; touch 0 (sets its ref bit); inserting 3 should evict 1 (the first unreferenced under
  // the hand), sparing the recently-touched 0.
  const pool = new BufferPool(3);
  let loads = 0;
  const load = (p: number) => () => {
    loads++;
    return leaf(p);
  };
  for (let p = 0; p < 3; p++) pool.getOrLoad(p, load(p));
  pool.getOrLoad(0, load(0)); // hit → ref bit on 0
  pool.getOrLoad(3, load(3)); // miss → evicts 1
  assert.equal(loads, 4);
  const before = loads;
  pool.getOrLoad(0, load(0)); // 0 spared — still cached
  assert.equal(loads, before, "0 was spared — still cached");
  pool.getOrLoad(1, load(1)); // 1 was evicted — reload
  assert.equal(loads, before + 1, "1 was evicted — reloaded");
});

test("bufferpool: capacity one evicts every time", () => {
  const pool = new BufferPool(1);
  let loads = 0;
  const load = (p: number) => () => {
    loads++;
    return leaf(p);
  };
  pool.getOrLoad(1, load(1));
  pool.getOrLoad(2, load(2));
  pool.getOrLoad(1, load(1)); // 1 was evicted by 2 → reload
  assert.equal(loads, 3);
  assert.equal(pool.resident(), 1);
});
