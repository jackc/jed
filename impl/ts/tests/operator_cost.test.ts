// Per-operator cost base (functions.md §8). The evaluator charges operatorCost(name) for an
// operator node instead of a flat operatorEval; operatorCost returns the operator's catalog cost if
// authored, else the uniform operatorEval. The conformance corpus CANNOT observe this while every
// built-in uses the uniform default (CLAUDE.md §10), so these per-core tests pin the mechanism.
// Mirrored in Rust (executor.rs registry_tests) and Go (operator_cost_test.go).

import assert from "node:assert/strict";
import { test } from "node:test";
import { COSTS } from "../src/costs.ts";
import { operatorCost } from "../src/executor.ts";
import { OPERATORS } from "../src/operators.ts";

// operatorCost must reflect the generated OPERATORS table for EVERY operator — proving the lookup
// is data-driven, so authoring a cost in catalog.toml is honored with no evaluator change.
test("operatorCost reflects the catalog cost field", () => {
  for (const o of OPERATORS) {
    const want = o.cost === 0 ? COSTS.operatorEval : BigInt(o.cost);
    assert.equal(operatorCost(o.name), want, `operatorCost(${o.name})`);
  }
  // An unknown name falls back to the uniform operatorEval.
  assert.equal(operatorCost("definitely_not_an_operator"), COSTS.operatorEval);
});

// Every operator name the evaluator charges through (arith/comparison ops ARE the catalog names;
// neg/not/and/or are wired literals) must resolve to a real catalog operator.
test("wired operator names exist in the catalog", () => {
  const names = [
    "add",
    "sub",
    "mul",
    "div",
    "mod",
    "eq",
    "ne",
    "lt",
    "gt",
    "le",
    "ge",
    "neg",
    "not",
    "and",
    "or",
  ];
  for (const name of names) {
    assert.ok(
      OPERATORS.some((o) => o.name === name),
      `wired operator name ${name} is not in the catalog`,
    );
  }
});
