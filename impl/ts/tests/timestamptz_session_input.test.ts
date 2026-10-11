// Offset-less timestamptz input in the session zone — the parts the corpus cannot express
// (spec/design/timezones.md §9.3). The PG-agreeing surface (literals, casts, assignment, DEFAULT,
// array/range/record text) is oracle-checked in suites/types/timestamptz_session_input.test; this
// file covers the two deliberate divergences — a prepared statement re-reads the session zone on
// every execution (PG freezes the prepare-time zone) and an index expression or predicate holding
// an offset-less literal is 42P17 (PG folds it into the definition) — plus a text value bound to a
// timestamptz parameter (host-API surface). Mirrors impl/rust/tests/timestamptz_session_input.rs.

import assert from "node:assert/strict";
import { test } from "node:test";
import type { PreparedStatement } from "../src/lib.ts";
import { intValue, render, textValue, type Value } from "../src/value.ts";
import { dbWith, errCode } from "./util.ts";

type Db = ReturnType<typeof dbWith>;

function mem(): Db {
  // 12:00 UTC, and 12:00 at UTC+2 (= 10:00 UTC).
  return dbWith([
    "CREATE TABLE ev (id i32 PRIMARY KEY, at timestamptz)",
    "INSERT INTO ev VALUES (1, '2024-07-15 12:00:00+00'), (2, '2024-07-15 10:00:00+00')",
  ]);
}

function ids(db: Db, stmt: PreparedStatement, params: Value[] = []): string[] {
  const out: string[] = [];
  for (const row of db.queryPrepared(stmt, params)) out.push(render(row[0]!));
  return out;
}

test("a prepared statement re-reads the session zone", () => {
  const db = mem();
  const stmt = db.prepareStatement("SELECT id FROM ev WHERE at = '2024-07-15 12:00:00'");
  assert.deepEqual(ids(db, stmt), ["1"], "UTC session");
  // The POSIX fixed-offset spelling '-02:00' is UTC+2 (timezones.md §6): 12:00 local = 10:00 UTC.
  db.setTimeZone("-02:00");
  assert.deepEqual(
    ids(db, stmt),
    ["2"],
    "a cached plan must not keep the instant folded under the prepare-time zone",
  );
  // An explicit offset does not depend on the zone either way.
  const fixed = db.prepareStatement("SELECT id FROM ev WHERE at = '2024-07-15 12:00:00+00'");
  assert.deepEqual(ids(db, fixed), ["1"]);
});

test("a prepared INSERT re-reads the session zone", () => {
  const db = mem();
  const ins = db.prepareStatement("INSERT INTO ev VALUES ($1, '2024-07-15 12:00:00')");
  db.executePrepared(ins, [intValue(3n)]);
  db.setTimeZone("-02:00");
  db.executePrepared(ins, [intValue(4n)]);
  db.setTimeZone("UTC");
  const q = db.prepareStatement("SELECT id FROM ev WHERE at = '2024-07-15 10:00:00' ORDER BY id");
  assert.deepEqual(ids(db, q), ["2", "4"]);
});

test("a bound text parameter reads the session zone", () => {
  const db = mem();
  db.setTimeZone("-02:00");
  const stmt = db.prepareStatement("SELECT id FROM ev WHERE at = $1");
  assert.deepEqual(ids(db, stmt, [textValue("2024-07-15 12:00:00")]), ["2"]);
  assert.deepEqual(ids(db, stmt, [textValue("2024-07-15 12:00:00+00")]), ["1"]);
});

test("an offset-less literal in an index is 42P17", () => {
  const db = mem();
  for (const sql of [
    "CREATE INDEX ON ev (id) WHERE at > '2024-01-01 00:00:00'",
    "CREATE INDEX ON ev ((at > timestamptz '2024-01-01'))",
    "CREATE INDEX ON ev (id) WHERE at < ('{\"2024-01-01 00:00:00\"}'::timestamptz[])[1]",
  ]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "42P17",
      sql,
    );
  }
  // An explicit offset never consults the zone, so it stays immutable and indexable.
  db.execute("CREATE INDEX ON ev (id) WHERE at > '2024-01-01 00:00:00+00'");
  db.execute("CREATE INDEX ON ev ((at > timestamptz 'infinity'))");
});
