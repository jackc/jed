// Bound parameters ($N) — spec/design/api.md §5. Typing, binding, and the errors they raise are pinned
// once for every core by the shared corpus (suites/params, the `# params:` directive —
// conformance.md §1.2). What stays here binds host value kinds the directive does not spell (a binary
// float, raw bytes).

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError, intValue } from "../src/tooling.ts";
import type { Value } from "../src/lib.ts";
import { float64Value, render } from "../src/value.ts";
import { type Handle, dbWith, queryOutcome } from "./util.ts";

function rows(db: Handle, sql: string, params: Value[]): Value[][] {
  const o = queryOutcome(db, sql, params);
  if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
  return o.rows;
}

function paramErrCode(db: Handle, sql: string, params: Value[]): string {
  try {
    db.execute(sql, params);
  } catch (e) {
    if (e instanceof EngineError) return e.code();
    throw e;
  }
  throw new Error(`expected an EngineError for ${sql}`);
}

test("composite PK float parameter widens soundly", () => {
  const db = dbWith([
    "CREATE TABLE t (f f64, a i32, v i32, PRIMARY KEY (f, a))",
    "INSERT INTO t VALUES (1.5, 1, 10), (2.5, 1, 20)",
  ]);
  const got = rows(db, "SELECT v FROM t WHERE f = $1 AND a = $2", [
    float64Value(1.5),
    intValue(1n),
  ]);
  assert.deepStrictEqual(
    got.map((r) => render(r[0]!)),
    ["10"],
  );
});

test("a bytea host value binds to a bytea slot and is the wrong family for a text one", () => {
  const db = dbWith([]);
  const bytea = (b: number[]): Value => ({ kind: "bytea", bytes: new Uint8Array(b) });
  assert.equal(render(rows(db, "SELECT encode($1, 'hex')", [bytea([0xab])])[0]![0]!), "ab");
  assert.equal(paramErrCode(db, "SELECT length($1)", [bytea([1, 2])]), "42804");
});
