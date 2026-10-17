// Any built-in scalar → text (spec/types/casts.toml, spec/design/types.md §5) — only what the
// PG-clean oracle corpus cannot hold. The rendering of every type, the session-zone timestamptz form,
// the 42P17 index rejection, and element-wise T[] → text[] agree with PostgreSQL and live in
// suites/cast/scalar_to_text.test. Here: the casts are EXPLICIT only (an assignment or implicit use
// is 42804 where PG accepts it), range → text stays 0A000 (PG renders it), and the
// $1::text parameter declaration. Mirrors impl/rust/tests/cast_scalar_text.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import { nullValue } from "../src/tooling.ts";
import type { Value } from "../src/lib.ts";
import { dbWith, errCode, query, queryOutcome } from "./util.ts";

const text = (s: string): Value => ({ kind: "text", text: s });

// Only the explicit spelling converts: storing or comparing a non-text value against text stays a
// 42804 datatype mismatch (PostgreSQL assignment-casts to text and would accept the INSERT).
test("the text cast is explicit only", () => {
  const db = dbWith(["CREATE TABLE t (id i32 PRIMARY KEY, s text)"]);
  for (const sql of [
    "INSERT INTO t VALUES (1, 42)",
    "INSERT INTO t VALUES (1, true)",
    "INSERT INTO t SELECT 1, date '2024-01-02'",
    "SELECT id FROM t WHERE s = 42",
  ]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "42804",
      sql,
    );
  }
  db.execute("INSERT INTO t SELECT 1, 42::text");
  assert.deepEqual(query(db, "SELECT s FROM t WHERE s = 42::text"), [["42"]]);
});

// A range value has no text cast yet (0A000), though PostgreSQL renders it (composite → text
// landed — cast/composite_casts.test).
test("range → text is 0A000", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT ('[1,5)'::i32range)::text")),
    "0A000",
  );
});

// $1::text declares the parameter as text (the cast-target parameter typing, api.md §5); a
// varchar(n) target truncates the bound value.
test("$1::text declares the parameter as text", () => {
  const db = dbWith([]);
  const one = (sql: string, bind: Value): Value => {
    const o = queryOutcome(db, sql, [bind]);
    if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
    return o.rows[0]![0]!;
  };
  assert.deepEqual(one("SELECT $1::text", text("abc")), text("abc"));
  assert.deepEqual(one("SELECT $1::varchar(2)", text("abc")), text("ab"));
  assert.deepEqual(one("SELECT $1::text", nullValue()), nullValue());
});
