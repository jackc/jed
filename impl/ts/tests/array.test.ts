// Array types (spec/design/array.md) — the S1–S4 vertical slice: a structural i32[] column, the
// ARRAY[…] constructor + the '{…}' literal, the compact value codec (S2), btree-NULL element
// comparison / ORDER BY / DISTINCT (S4), and array_out rendering. Mirrors impl/rust/tests/array.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { Engine, execute, loadEngine, toImage } from "../src/tooling.ts";
import { errCode, query } from "./util.ts";

function run(db: Engine, sql: string): void {
  execute(db, sql);
}

test("array column survives a whole-image round-trip", () => {
  const db = new Engine();
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, xs i32[], tags text[])");
  run(db, "INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])");
  run(db, "INSERT INTO t VALUES (2, ARRAY[1, NULL, 3], '{}')");
  run(db, "INSERT INTO t VALUES (3, NULL, NULL)");
  const loaded = loadEngine(toImage(db, 4096, 1n));
  assert.deepStrictEqual(query(loaded, "SELECT id, xs, tags FROM t ORDER BY id"), [
    ["1", "{10,20,30}", "{a,b}"],
    ["2", "{1,NULL,3}", "{}"],
    ["3", "NULL", "NULL"],
  ]);
});

// --- AC1: array-of-composite element types (spec/design/array.md §12) -----------------------------

test("AC1: composite-element array round-trips literal + ARRAY[ROW(…)] constructor; access works", () => {
  const db = new Engine();
  run(db, "CREATE TYPE addr AS (street text, zip i32)");
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, items addr[])");
  // The text-literal construction path (array_in → record_in per element).
  run(db, `INSERT INTO t VALUES (1, '{"(Main,90210)","(Side,5)"}')`);
  // The ARRAY[ROW(…)] constructor with composite element context (no ::addr cast — a jed extension).
  run(db, "INSERT INTO t VALUES (2, ARRAY[ROW('Other, Ln', 12)])");
  run(db, `INSERT INTO t VALUES (3, '{"(Main,)",NULL}')`);
  assert.deepStrictEqual(query(db, "SELECT id, items FROM t ORDER BY id"), [
    ["1", `{"(Main,90210)","(Side,5)"}`],
    ["2", `{"(\\"Other, Ln\\",12)"}`],
    ["3", `{"(Main,)",NULL}`],
  ]);
  // Subscript → the composite element (record_out, no braces); field access; slice → addr[].
  assert.deepStrictEqual(query(db, "SELECT items[1] FROM t WHERE id = 1"), [["(Main,90210)"]]);
  assert.deepStrictEqual(query(db, "SELECT (items[2]).street FROM t WHERE id = 1"), [["Side"]]);
  assert.deepStrictEqual(query(db, "SELECT items[1:1] FROM t WHERE id = 1"), [
    [`{"(Main,90210)"}`],
  ]);
});

test("AC1: an addr[] column survives a whole-image round-trip", () => {
  const db = new Engine();
  run(db, "CREATE TYPE addr AS (street text, zip i32)");
  run(db, "CREATE TABLE t (id i32 PRIMARY KEY, items addr[])");
  run(db, `INSERT INTO t VALUES (1, '{"(Main,90210)","(Side,5)"}')`);
  run(db, `INSERT INTO t VALUES (2, '{"(Main,)",NULL}')`);
  run(db, "INSERT INTO t VALUES (3, NULL)");
  const loaded = loadEngine(toImage(db, 4096, 1n));
  assert.deepStrictEqual(query(loaded, "SELECT id, items FROM t ORDER BY id"), [
    ["1", `{"(Main,90210)","(Side,5)"}`],
    ["2", `{"(Main,)",NULL}`],
    ["3", "NULL"],
  ]);
});

test("AC1: composite element NULL-field ordering operators are definite (the total-order fix)", () => {
  const db = new Engine();
  run(db, "CREATE TYPE addr AS (street text, zip i32)");
  // Equal arrays with a NULL composite field: definite, never UNKNOWN.
  assert.deepStrictEqual(
    query(
      db,
      `SELECT '{"(1,)"}'::addr[] <= '{"(1,)"}'::addr[], ` +
        `'{"(1,)"}'::addr[] >= '{"(1,)"}'::addr[], ` +
        `'{"(1,)"}'::addr[] < '{"(1,)"}'::addr[]`,
    ),
    [["true", "true", "false"]],
  );
  // A NULL field sorts after a present field.
  assert.deepStrictEqual(
    query(
      db,
      `SELECT '{"(a,)"}'::addr[] > '{"(a,1)"}'::addr[], '{"(a,1)"}'::addr[] < '{"(a,)"}'::addr[]`,
    ),
    [["true", "true"]],
  );
});

test("AC1: a composite-element array is still never keyable (0A000)", () => {
  const db = new Engine();
  run(db, "CREATE TYPE addr AS (street text, zip i32)");
  assert.strictEqual(
    errCode(() => run(db, "CREATE TABLE t (items addr[] PRIMARY KEY)")),
    "0A000",
  );
});
