// Phase 1: the general expression evaluator — integer arithmetic (+ - * / %, unary
// minus), the expression-only boolean type, comparisons-as-values, AND/OR/NOT Kleene
// connectives, operator precedence, and parentheses. These complement the conformance
// corpus (spec/conformance/suites/expr/) with finer-grained per-feature assertions.

import assert from "node:assert/strict";
import { test } from "node:test";
import { dbWith, query } from "./util.ts";

test("comparisons project booleans (true / false / NULL)", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, a i32, b i32)",
    "INSERT INTO t VALUES (1, 5, 5)",
    "INSERT INTO t VALUES (2, 5, 9)",
    "INSERT INTO t VALUES (3, 5, NULL)",
  ]);
  assert.deepStrictEqual(query(db, "SELECT a = b FROM t ORDER BY id"), [
    ["true"],
    ["false"],
    ["NULL"],
  ]);
  assert.deepStrictEqual(query(db, "SELECT TRUE FROM t WHERE id = 1"), [["true"]]);
  assert.deepStrictEqual(query(db, "SELECT FALSE FROM t WHERE id = 1"), [["false"]]);
});
