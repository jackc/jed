import assert from "node:assert/strict";
import { test } from "node:test";
import { singleValuesInsertEligible } from "../src/executor.ts";

test("single-row INSERT fast path selects only one plain VALUES candidate", () => {
  assert.equal(singleValuesInsertEligible(1, false), true);
  assert.equal(singleValuesInsertEligible(0, false), false);
  assert.equal(singleValuesInsertEligible(2, false), false);
  assert.equal(singleValuesInsertEligible(1, true), false);
});
