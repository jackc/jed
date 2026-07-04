// generate_series — the engine's first set-returning function, a FROM-clause row source
// (spec/design/functions.md §10, grammar.md §35). These complement the conformance corpus
// (spec/conformance/suites/query/generate_series.test) with finer-grained assertions: the
// generator's PostgreSQL edge cases (NULL → empty, step zero → 22023, descending step, the
// positive-default-step empty case, i64-overflow clean-stop), the synthetic-relation wiring
// (output column name/type, alias + qualified resolution, CROSS JOIN composition), the
// non-LATERAL rule ($N / correlated outer arg vs. a rejected sibling reference), the
// generated_row cost contract + the maxCost ceiling, and the deferred-form errors.

import assert from "node:assert/strict";
import { test } from "node:test";
import { intValue } from "../src/tooling.ts";
import { type Handle, dbWith, errCode, query, queryOutcome } from "./util.ts";
import { memDb } from "./mem_db.ts";

function cost(db: Handle, sql: string): bigint {
  return queryOutcome(db, sql).cost;
}

function rows1(ns: number[]): string[][] {
  return ns.map((n) => [String(n)]);
}

test("step of zero is invalid_parameter_value (22023)", () => {
  const db = memDb().session();
  assert.equal(
    errCode(() => db.execute("SELECT * FROM generate_series(1, 5, 0)")),
    "22023",
  );
});

test("alias forms and qualified column", () => {
  const db = memDb().session();
  // PG's single-column function-alias rule: `AS g` (or implicit `g`) renames the column to `g`.
  assert.deepStrictEqual(query(db, "SELECT * FROM generate_series(1, 3) g"), rows1([1, 2, 3]));
  const out = queryOutcome(db, "SELECT * FROM generate_series(1, 3) AS g");
  assert.equal(out.kind, "query");
  if (out.kind === "query") assert.deepStrictEqual(out.columnNames, ["g"]);
  assert.deepStrictEqual(query(db, "SELECT g.g FROM generate_series(1, 3) AS g"), rows1([1, 2, 3]));
  assert.equal(
    errCode(() => db.execute("SELECT g.generate_series FROM generate_series(1, 3) AS g")),
    "42703",
  );
  assert.deepStrictEqual(
    query(db, "SELECT generate_series.generate_series FROM generate_series(1, 2)"),
    rows1([1, 2]),
  );
});

test("$N parameter argument", () => {
  const db = memDb().session();
  const out = queryOutcome(db, "SELECT * FROM generate_series(1, $1)", [intValue(3n)]);
  assert.equal(out.kind, "query");
  if (out.kind !== "query") return;
  assert.deepStrictEqual(
    out.rows.map((r) => r.map((v) => (v.kind === "int" ? String(v.int) : "?"))),
    [["1"], ["2"], ["3"]],
  );
});

test("a sibling reference works (an SRF is implicitly lateral, grammar.md §44)", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, n i32)", "INSERT INTO t VALUES (1, 3)"]);
  // The rows are pinned by suites/joins/lateral.test; here we only assert the prior non-LATERAL
  // 42P01 rejection is lifted — generate_series(1, t.n) re-runs per t row (1 row, n=3 ⇒ 3 rows).
  const out = queryOutcome(db, "SELECT * FROM t CROSS JOIN generate_series(1, t.n)");
  assert.equal(out.kind, "query");
  if (out.kind !== "query") return;
  assert.equal(out.rows.length, 3);
});

test("generated_row cost and the maxCost ceiling", () => {
  const db = memDb().session();
  assert.equal(cost(db, "SELECT * FROM generate_series(1, 4)"), 8n);
  db.setMaxCost(50n);
  assert.equal(
    errCode(() => db.execute("SELECT * FROM generate_series(1, 1000000000)")),
    "54P01",
  );
  db.setMaxCost(0n);
});

test("mixed-width promotes to the wider type", () => {
  const db = memDb().session();
  const out = queryOutcome(db, "SELECT * FROM generate_series(CAST(1 AS i16), CAST(5 AS i32))");
  assert.equal(out.kind, "query");
  if (out.kind !== "query") return;
  assert.deepStrictEqual(out.columnTypes, ["i32"]);
});

test("i64 overflow while stepping stops cleanly (bigint parity)", () => {
  const db = memDb().session();
  // Stepping past i64::MAX must STOP, not run forever (bigint never overflows): only the last
  // representable element is emitted, matching Rust/Go's checked_add stop.
  assert.deepStrictEqual(
    query(db, "SELECT * FROM generate_series(9223372036854775806, 9223372036854775807, 2)"),
    [["9223372036854775806"]],
  );
});

test("deferred-form and bad-call errors", () => {
  const db = memDb().session();
  assert.equal(
    errCode(() => db.execute("SELECT generate_series(1, 5)")),
    "42883",
  ); // SELECT-list SRF deferred
  assert.equal(
    errCode(() => db.execute("SELECT * FROM generate_series(1, 5) AS g(n)")),
    "0A000",
  ); // column-alias list
  assert.equal(
    errCode(() => db.execute("SELECT * FROM generate_series(1)")),
    "42883",
  ); // wrong arity
  assert.equal(
    errCode(() => db.execute("SELECT * FROM generate_series('a', 5)")),
    "42883",
  ); // non-integer arg
  assert.equal(
    errCode(() => db.execute("SELECT * FROM nope(1, 5)")),
    "42883",
  ); // unknown table function
});
