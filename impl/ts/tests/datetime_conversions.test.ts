// date_trunc / EXTRACT / cross-family datetime casts — the deliberate PostgreSQL divergences
// (spec/design/timezones.md §9). The agreeing behavior is oracle-checked in
// suites/expr/{date_trunc,extract,datetime_cast}.test and runs on every core; these per-core tests
// cover only what the oracle corpus CANNOT express (CLAUDE.md §10) — the cases where jed deliberately
// differs from PG. Mirrors impl/rust/tests/datetime_conversions.rs.
//
//   - EXTRACT(field FROM ±infinity) (timestamp, timestamptz, or date) — jed's decimal is finite-only,
//     so 22003; PG returns ±Infinity (or NULL for an oscillating field). date_part follows EXTRACT
//     (22003 where PG returns float ±Infinity).
//   - a non-datetime, non-text source to a datetime target — jed 42804 (PG 42846).

import assert from "node:assert/strict";
import { test } from "node:test";
import { dbWith, errCode } from "./util.ts";

test("EXTRACT / date_part over an infinite datetime traps (22003 — jed decimal is finite)", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT EXTRACT(year FROM timestamp 'infinity')")),
    "22003",
  );
  assert.equal(
    errCode(() => db.execute("SELECT EXTRACT(epoch FROM timestamptz '-infinity')")),
    "22003",
  );
  for (const sql of [
    "SELECT EXTRACT(julian FROM timestamp 'infinity')",
    "SELECT EXTRACT(julian FROM date '-infinity')",
    "SELECT EXTRACT(year FROM date 'infinity')",
    "SELECT EXTRACT(day FROM date '-infinity')",
    "SELECT date_part('julian', timestamp '-infinity')",
    "SELECT date_part('julian', timestamptz 'infinity')",
    "SELECT date_part('julian', date 'infinity')",
  ]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "22003",
      sql,
    );
  }
});

test("a non-datetime, non-text source to a datetime target is forbidden (42804; PG 42846)", () => {
  // Runtime text → datetime has landed (cast_text_string_native.test.ts).
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT CAST(1 + 1 AS timestamp)")),
    "42804",
  );
});
