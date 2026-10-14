// Bound parameters ($N) — spec/design/api.md §5. Typing, binding, and the errors they raise are pinned
// once for every core by the shared corpus (suites/params, the `# params:` directive —
// conformance.md §1.2). What stays here binds host value kinds the directive does not spell (a binary
// float, raw bytes, a range value) and the JS-array mapping of the ergonomic layer.

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError, intValue } from "../src/tooling.ts";
import type { Value } from "../src/lib.ts";
import { float64Value, rangeValue, render } from "../src/value.ts";
import { memDb } from "./mem_db.ts";
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

// A range host value binds canonicalized, as a range constructor's result is (api.md §5): an inclusive
// discrete upper bound moves to the canonical `[)` form, and crossed bounds are 22000.
test("a range host value binds canonicalized", () => {
  const db = dbWith([]);
  const inclusive = rangeValue(intValue(1n), intValue(3n), true, true);
  assert.equal(render(rows(db, "SELECT $1::i32range", [inclusive])[0]![0]!), "[1,4)");
  const crossed = rangeValue(intValue(3n), intValue(1n), true, false);
  assert.equal(paramErrCode(db, "SELECT $1::i32range", [crossed]), "22000");
});

// A JS array binds as one array parameter (api.md §11).
test("a JS array binds as an array parameter", () => {
  const db = memDb();
  db.run("CREATE TABLE t (id i32 PRIMARY KEY, tags text[])");
  db.run("INSERT INTO t VALUES (1, '{a}'), (2, '{b}'), (3, '{a,b}')");
  const ids = db.prepare("SELECT id FROM t WHERE id = ANY($1) ORDER BY id").all([1, 3]);
  assert.deepEqual(
    ids.map((r) => r.id),
    [1n, 3n],
  );
  assert.deepEqual(db.prepare("SELECT count(*) AS n FROM t WHERE tags @> $1").get(["a"]), {
    n: 2n,
  });
});
