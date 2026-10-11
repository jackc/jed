// Runtime text → timestamp / timestamptz / interval / bytea casts — the parts the PG-clean oracle
// corpus cannot express (spec/types/casts.toml, spec/design/types.md §5). The accepted grammars
// AGREE with PostgreSQL and are oracle-checked in suites/cast/text_to_string_native.test (run on
// every core); this file covers only the DIVERGENCES: the jed-stricter input grammars (identical to
// the literal path), the SQLSTATE subcodes that differ from PG, the forbidden-pair convention
// (42804 where PG reports 42846), the i64 guard on the session-zone shift, and $1::interval. Every
// cast is on a NON-LITERAL text column, so it exercises the per-row eval path, not the resolve-time
// literal fold. Mirrors impl/go/cast_text_string_native_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { intervalValue, render } from "../src/value.ts";
import { dbWith, errCode, query, queryOutcome } from "./util.ts";

// Build t(id i32 pk, s text) with one row per string (id = 1..).
function seeded(rows: string[]): ReturnType<typeof dbWith> {
  const stmts = ["CREATE TABLE t (id i32 PRIMARY KEY, s text)"];
  for (const [i, s] of rows.entries()) {
    stmts.push(`INSERT INTO t VALUES (${i + 1}, '${s}')`);
  }
  return dbWith(stmts);
}

test("runtime text → string-native casts: jed-stricter grammars and differing SQLSTATEs", () => {
  const cases: [string, string, string][] = [
    ["now", "s :: timestamp", "22007"], // PG: the statement time; jed: no clock specials
    ["today", "s :: timestamptz", "22007"], // PG: midnight today; jed: no clock specials
    ["epoch", "s :: timestamp", "22007"], // PG: 1970-01-01 00:00:00; jed: not accepted
    ["01/15/2024 12:00", "s :: timestamp", "22007"], // PG (DateStyle MDY) accepts; jed: ISO only
    ["2024-01-01 12:00:00+16", "s :: timestamptz", "22008"], // PG: 22009 displacement
    ["P1D", "s :: interval", "22007"], // PG: ISO-8601 1 day; jed: unit + time only
    ["2147483648 days", "s :: interval", "22008"], // PG: 22015 interval field overflow
    ["abc", "s :: bytea", "22P02"], // PG: escape format \x616263; jed: hex only
    ["\\x0", "s :: bytea", "22P02"], // PG: 22023 odd number of hex digits
    ["\\xzz", "s :: bytea", "22P02"], // PG: 22023 invalid hexadecimal digit
  ];
  for (const [s, expr, want] of cases) {
    const db = seeded([s]);
    assert.equal(
      errCode(() => db.execute(`SELECT ${expr} FROM t WHERE id = 1`)),
      want,
      `${s} ${expr}`,
    );
  }
});

test("a forbidden source to a string-native target is 42804 (PG 42846)", () => {
  const db = seeded([]);
  for (const sql of [
    "SELECT 1::interval",
    "SELECT CAST(1 + 1 AS timestamp)",
    "SELECT 1::timestamptz",
    "SELECT 1::bytea",
    "SELECT (DATE '2024-01-01')::interval",
  ]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "42804",
      sql,
    );
  }
});

test("runtime text → timestamptz: a session-zone shift past i64 traps 22008", () => {
  // A wall clock just below jed's i64 ceiling, read in a UTC-5 session (the POSIX fixed-offset
  // spelling '+05:00' — positive is WEST), shifts past i64: 22008, never an out-of-range instant.
  const db = seeded(["294247-01-10 04:00:00"]);
  db.setTimeZone("+05:00");
  assert.equal(
    errCode(() => db.execute("SELECT s :: timestamptz FROM t WHERE id = 1")),
    "22008",
  );
  // The same wall clock in UTC is representable.
  db.setTimeZone("UTC");
  assert.equal(query(db, "SELECT s :: timestamptz FROM t WHERE id = 1").length, 1);
});

test("runtime text → timestamptz reads an offset-less string in a fixed-offset session zone", () => {
  // A fixed-offset session zone needs no tz data: '-02:00' is UTC+2, so 12:00 local is 10:00 UTC.
  const db = seeded(["2024-07-15 12:00:00", "2024-07-15 12:00:00+00"]);
  db.setTimeZone("-02:00");
  const utc = "(s :: timestamptz) AT TIME ZONE 'UTC'";
  assert.deepEqual(query(db, `SELECT ${utc} FROM t WHERE id = 1`), [["2024-07-15 10:00:00"]]);
  assert.deepEqual(query(db, `SELECT ${utc} FROM t WHERE id = 2`), [["2024-07-15 12:00:00"]]);
});

test("$1::interval types the parameter as interval", () => {
  const db = seeded([]);
  const o = queryOutcome(db, "SELECT $1::interval", [
    intervalValue({ months: 0, days: 3, micros: 0n }),
  ]);
  if (o.kind !== "query") throw new Error("expected a query result");
  assert.equal(render(o.rows[0][0]), "3 days");
});
