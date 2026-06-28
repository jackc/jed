// INSERT: positional type-checking, the overflow / not-null / duplicate-key traps, and
// no-PK synthetic rowid behaviour. i64 extremes must round-trip exactly (the bigint
// path — the dimension this core exists to exercise).

import assert from "node:assert/strict";
import { test } from "node:test";
import { Engine, execute, executeParams, intValue } from "../src/tooling.ts";
import { dbWith, errCode, query } from "./util.ts";

function nums(): Engine {
  return dbWith(["CREATE TABLE nums (id i32 PRIMARY KEY, small i16, big i64)"]);
}

test("insert round-trips i64 extremes exactly", () => {
  const db = nums();
  execute(db, "INSERT INTO nums VALUES (1, -32768, -9223372036854775808)");
  execute(db, "INSERT INTO nums VALUES (2, 32767, 9223372036854775807)");
  assert.deepStrictEqual(query(db, "SELECT small, big FROM nums ORDER BY id"), [
    ["-32768", "-9223372036854775808"],
    ["32767", "9223372036854775807"],
  ]);
});

test("no-PK table accepts repeated rows (synthetic rowid)", () => {
  const db = dbWith(["CREATE TABLE r (a i16)"]);
  execute(db, "INSERT INTO r VALUES (5)");
  execute(db, "INSERT INTO r VALUES (5)");
  assert.equal(query(db, "SELECT a FROM r").length, 2);
});

test("insert into a missing table traps 42P01", () => {
  assert.equal(
    errCode(() => execute(new Engine(), "INSERT INTO nope VALUES (1)")),
    "42P01",
  );
});

// --- multi-row INSERT (spec/design/grammar.md §12) --------------------------------

test("no-PK multi-row INSERT keeps insertion order; a failed batch stores nothing", () => {
  const db = dbWith(["CREATE TABLE log (a i16)"]);
  // No PK ⇒ monotonic synthetic rowids, allocated left-to-right; key order = insertion order.
  execute(db, "INSERT INTO log VALUES (30), (10), (20)");
  assert.deepStrictEqual(query(db, "SELECT a FROM log"), [["30"], ["10"], ["20"]]);
  // A failing batch (second row overflows) stores neither row.
  assert.equal(
    errCode(() => execute(db, "INSERT INTO log VALUES (40), (99999)")),
    "22003",
  );
  assert.deepStrictEqual(query(db, "SELECT a FROM log"), [["30"], ["10"], ["20"]]);
});

// --- INSERT ... SELECT (spec/design/grammar.md §24) -----------------------------------
// Most behaviour is pinned by the shared corpus (suites/dml/insert_select.test). These cover
// the param-in-source case (the corpus is literal-only) and assert the cost number directly.

test("INSERT ... SELECT binds a $N inside the source query", () => {
  const db = dbWith([
    "CREATE TABLE src (id i32 PRIMARY KEY, a i16)",
    "INSERT INTO src VALUES (1, 10), (2, 20), (3, 30)",
    "CREATE TABLE dst (id i32 PRIMARY KEY, a i16)",
  ]);
  // A $1 inside the source SELECT binds through the SELECT's own resolver.
  executeParams(db, "INSERT INTO dst SELECT id, a FROM src WHERE id >= $1", [intValue(2n)]);
  assert.deepStrictEqual(query(db, "SELECT id FROM dst ORDER BY id"), [["2"], ["3"]]);
});

test("INSERT ... SELECT cost is the embedded SELECT's accrued cost", () => {
  const db = dbWith([
    "CREATE TABLE src (id i32 PRIMARY KEY, a i16, b i64)",
    "INSERT INTO src VALUES (1, 10, 100), (2, 20, 200), (3, 30, 300)",
    "CREATE TABLE dst (id i32 PRIMARY KEY, a i16, b i64)",
  ]);
  // 1 page_read (src is one leaf) + 3 scanned + 3 produced + 0 projection (bare columns) = 7;
  // storing the rows is unmetered.
  const o = execute(db, "INSERT INTO dst SELECT id, a, b FROM src");
  assert.equal(o.kind, "statement");
  assert.equal(o.cost, 7n);
});
