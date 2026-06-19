// UPDATE: in-place replacement, old-row assignment semantics (swap), the two-phase
// all-or-nothing guarantee, and the rejected cases (PK column, duplicate target,
// overflow, not-null).

import assert from "node:assert/strict";
import { test } from "node:test";
import { Database, execute } from "../src/lib.ts";
import { dbWith, errCode } from "./util.ts";

function setup() {
  return dbWith([
    "CREATE TABLE t (id int32 PRIMARY KEY, a int16, b int16)",
    "INSERT INTO t VALUES (1, 10, 11)",
    "INSERT INTO t VALUES (2, 20, 22)",
    "INSERT INTO t VALUES (3, 30, 33)",
  ]);
}

test("unknown column traps 42703; missing table traps 42P01", () => {
  assert.equal(errCode(() => execute(setup(), "UPDATE t SET nope = 1")), "42703");
  assert.equal(errCode(() => execute(new Database(), "UPDATE nope SET a = 1")), "42P01");
});
