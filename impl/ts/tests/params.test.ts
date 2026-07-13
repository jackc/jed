// Phase 7: parameterized queries ($N bind parameters) — spec/design/api.md §5. Parameters are a
// host-API surface (not the shared corpus): their type is inferred from context and supplied
// values are coerced two-phase before any row is touched.

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError, intValue, nullValue } from "../src/tooling.ts";
import type { Value } from "../src/lib.ts";
import { float64Value } from "../src/value.ts";
import { type Handle, dbWith, queryOutcome } from "./util.ts";
import { memDb } from "./mem_db.ts";

function text(s: string): Value {
  return { kind: "text", text: s };
}

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

function ints(rs: Value[][]): bigint[] {
  return rs.map((r) => (r[0]!.kind === "int" ? r[0]!.int : -1n));
}

test("WHERE pk = $1 point lookup", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
    "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
  ]);
  assert.deepStrictEqual(ints(rows(db, "SELECT v FROM t WHERE id = $1", [intValue(2n)])), [20n]);
});

test("composite PK parameter tuple bound", () => {
  const db = dbWith([
    "CREATE TABLE t (a i32, b i16, v i32, PRIMARY KEY (b, a))",
    "INSERT INTO t VALUES (1, 1, 10), (2, 1, 20), (3, 1, 30), (1, 2, 40)",
  ]);
  const got = rows(db, "SELECT v FROM t WHERE b = $1 AND a >= $2 ORDER BY a", [
    intValue(1n),
    intValue(2n),
  ]);
  assert.deepStrictEqual(ints(got), [20n, 30n]);
});

test("composite PK float parameter widens soundly", () => {
  const db = dbWith([
    "CREATE TABLE t (f f64, a i32, v i32, PRIMARY KEY (f, a))",
    "INSERT INTO t VALUES (1.5, 1, 10), (2.5, 1, 20)",
  ]);
  const got = rows(db, "SELECT v FROM t WHERE f = $1 AND a = $2", [
    float64Value(1.5),
    intValue(1n),
  ]);
  assert.deepStrictEqual(ints(got), [10n]);
});

test("param adopts narrow column type and traps overflow", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, s i16)",
    "INSERT INTO t VALUES (1, 100)",
  ]);
  assert.equal(paramErrCode(db, "SELECT id FROM t WHERE s = $1", [intValue(100000n)]), "22003");
  assert.deepStrictEqual(ints(rows(db, "SELECT id FROM t WHERE s = $1", [intValue(100n)])), [1n]);
});

test("INSERT VALUES params round-trip", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, name text)"]);
  db.execute("INSERT INTO t VALUES ($1, $2)", [intValue(7n), text("alice")]);
  const got = rows(db, "SELECT id, name FROM t WHERE id = $1", [intValue(7n)]);
  assert.equal(got.length, 1);
  assert.equal(got[0]![0]!.kind === "int" ? got[0]![0]!.int : -1n, 7n);
  assert.equal(got[0]![1]!.kind === "text" ? got[0]![1]!.text : "", "alice");
});

test("INSERT param NULL into NOT NULL traps 23502", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, name text NOT NULL)"]);
  assert.equal(
    paramErrCode(db, "INSERT INTO t VALUES ($1, $2)", [intValue(1n), nullValue()]),
    "23502",
  );
});

test("INSERT param wrong family traps 42804", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, n i32)"]);
  assert.equal(
    paramErrCode(db, "INSERT INTO t VALUES ($1, $2)", [intValue(1n), text("x")]),
    "42804",
  );
});

test("UPDATE SET and WHERE params", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, v i32)",
    "INSERT INTO t VALUES (1, 10), (2, 20)",
  ]);
  db.execute("UPDATE t SET v = $1 WHERE id = $2", [intValue(99n), intValue(2n)]);
  assert.deepStrictEqual(ints(rows(db, "SELECT v FROM t WHERE id = $1", [intValue(2n)])), [99n]);
});

test("DELETE WHERE param", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1), (2), (3)"]);
  db.execute("DELETE FROM t WHERE id = $1", [intValue(2n)]);
  assert.deepStrictEqual(ints(rows(db, "SELECT id FROM t", [])), [1n, 3n]);
});

test("text param inference", () => {
  const db = dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, name text)",
    "INSERT INTO t VALUES (1, 'alice'), (2, 'bob')",
  ]);
  assert.deepStrictEqual(ints(rows(db, "SELECT id FROM t WHERE name = $1", [text("bob")])), [2n]);
});

test("LEAST infers a param from its common type", () => {
  // GREATEST/LEAST note a bare parameter at their unified scalar type, like a comparison operand
  // (grammar.md §52). Source branch A skipped this, so LEAST($1, 10) failed 42P18. A per-core test
  // because param binding is a host-API surface (not the shared corpus).
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)"]);
  assert.deepStrictEqual(ints(rows(db, "SELECT LEAST($1, 10)", [intValue(7n)])), [7n]);
});

test("bare SELECT $1 is indeterminate 42P18", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)"]);
  assert.equal(paramErrCode(db, "SELECT $1 FROM t", [intValue(1n)]), "42P18");
});

test("gap in param indices is 42P18", () => {
  const db = dbWith(["CREATE TABLE t (a i32 PRIMARY KEY, b i32)"]);
  assert.equal(
    paramErrCode(db, "SELECT a FROM t WHERE a = $1 OR b = $3", [
      intValue(1n),
      intValue(2n),
      intValue(3n),
    ]),
    "42P18",
  );
});

test("conflicting inference is 42804", () => {
  const db = dbWith(["CREATE TABLE t (a i32 PRIMARY KEY, name text)"]);
  assert.equal(
    paramErrCode(db, "SELECT a FROM t WHERE a = $1 OR name = $1", [intValue(1n)]),
    "42804",
  );
});

test("count mismatch is 42601", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1)"]);
  assert.equal(paramErrCode(db, "SELECT id FROM t WHERE id = $1", []), "42601");
  assert.equal(
    paramErrCode(db, "SELECT id FROM t WHERE id = $1", [intValue(1n), intValue(2n)]),
    "42601",
  );
});

test("NULL param three-valued", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, v i32)", "INSERT INTO t VALUES (1, 10)"]);
  assert.deepStrictEqual(rows(db, "SELECT id FROM t WHERE v = $1", [nullValue()]), []);
});

test("param in IN list", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)", "INSERT INTO t VALUES (1), (2), (3)"]);
  assert.deepStrictEqual(
    ints(rows(db, "SELECT id FROM t WHERE id IN ($1, $2)", [intValue(1n), intValue(3n)])),
    [1n, 3n],
  );
});

test("DDL with params traps 42601", () => {
  const db = memDb().session();
  assert.equal(paramErrCode(db, "CREATE TABLE t (id i32 PRIMARY KEY)", [intValue(1n)]), "42601");
});

test("param typed by the :: cast operator", () => {
  // `$1::int` declares `$1` as int — PostgreSQL types a parameter by its cast target
  // (api.md §5, grammar.md §37). No surrounding context is needed, so this is NOT 42P18.
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)"]);
  assert.deepStrictEqual(ints(rows(db, "SELECT $1::int", [intValue(42n)])), [42n]);
  // The CAST(... AS ...) spelling infers the parameter's type identically.
  assert.deepStrictEqual(ints(rows(db, "SELECT CAST($1 AS int)", [intValue(7n)])), [7n]);
});

test("param :: cast narrows and traps 22003", () => {
  // `$1::smallint` declares `$1` as i16; a bound value out of i16 range traps 22003 at bind.
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)"]);
  assert.equal(paramErrCode(db, "SELECT $1::smallint", [intValue(100000n)]), "22003");
});

test("param cast to a deferred target is 0A000", () => {
  // Casting a parameter to a deferred target (text) is 0A000, like any non-string-literal cast.
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)"]);
  assert.equal(paramErrCode(db, "SELECT $1::text", [intValue(1n)]), "0A000");
});

test(":: inherits deferred narrowings and rejects a lone colon", () => {
  // `::` desugars to CAST, so casting a non-string-literal value to text is the same deferred
  // 0A000 narrowing the CAST spelling carries. The boolean cast has since landed — `5::boolean`
  // is now valid (→ true; cast_bool_int.test.ts).
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)"]);
  assert.equal(paramErrCode(db, "SELECT 5::text", []), "0A000");
  // A lone `:` is not part of jed's surface — a 42601 syntax error from the lexer.
  assert.equal(paramErrCode(db, "SELECT 1 : 2", []), "42601");
});

test("lexer rejects bad param tokens", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY)"]);
  for (const sql of [
    "SELECT id FROM t WHERE id = $0",
    "SELECT id FROM t WHERE id = $",
    "SELECT id FROM t WHERE id = $01",
  ]) {
    let code = "";
    try {
      db.execute(sql);
    } catch (e) {
      if (e instanceof EngineError) code = e.code();
    }
    assert.equal(code, "42601", `${sql} should be 42601`);
  }
});
