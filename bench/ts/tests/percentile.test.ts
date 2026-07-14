import assert from "node:assert/strict";
import test from "node:test";
import { percentile } from "../src/lib.ts";

test("lower sample percentiles", () => {
  const samples = Array.from({ length: 11 }, (_, i) => BigInt(i));
  assert.equal(percentile(samples, 50), 5n);
  assert.equal(percentile(samples, 90), 9n);
  assert.equal(percentile(samples, 99), 9n);
});
