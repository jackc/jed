import assert from "node:assert/strict";
import test from "node:test";

import { Meter } from "../src/cost.ts";
import { HashJoinTable, type HashJoinPlan } from "../src/executor.ts";
import { intValue } from "../src/value.ts";

test("forced hash collisions recheck full keys", () => {
  const plan: HashJoinPlan = {
    keys: [{ left: 0, right: 1, type: { kind: "scalar", scalar: "i32" } }],
  };
  const rows = [
    [intValue(1n), intValue(10n)],
    [intValue(2n), intValue(20n)],
    [intValue(2n), intValue(21n)],
  ];
  const table = new HashJoinTable(plan, 1, 0, rows, new Meter(), () => 0n);
  assert.deepEqual(table.probe(plan, [intValue(2n)], new Meter()), [1, 2]);
});
