// date_trunc / EXTRACT / cross-family datetime casts — the deliberate PostgreSQL divergences
// (spec/design/timezones.md §9). The agreeing behavior is oracle-checked in
// suites/expr/{date_trunc,extract,datetime_cast}.test and runs on every core; these per-core tests
// cover only what the oracle corpus CANNOT express (CLAUDE.md §10) — the cases where jed deliberately
// differs from PG. Mirrors impl/rust/tests/datetime_conversions.rs.
//
//   - EXTRACT(julian …) — jed defers the field (0A000); PG returns a value (timezones.md §9.2).
//   - date_part('field', …) — jed has no such function (42883); PG returns double precision, deferred.
//   - EXTRACT(field FROM ±infinity) — jed's decimal is finite-only, so 22003; PG returns ±Infinity.
//   - a non-datetime / non-literal-text source to a datetime target — jed 0A000 (text→datetime is a
//     valid PG cast; int→datetime is PG 42846).

import assert from "node:assert/strict";
import { test } from "node:test";
import { dbWith, errCode } from "./util.ts";

test("EXTRACT(julian …) is a deferred field (0A000)", () => {
  const db = dbWith([]);
  for (const sql of [
    "SELECT EXTRACT(julian FROM timestamp '2024-03-15 00:00:00')",
    "SELECT EXTRACT(julian FROM date '2024-03-15')",
  ]) {
    assert.equal(
      errCode(() => db.execute(sql)),
      "0A000",
      sql,
    );
  }
});

test("date_part is deferred (42883 — returns float8, jed has no float)", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT date_part('hour', timestamp '2024-03-15 13:00:00')")),
    "42883",
  );
});

test("EXTRACT over an infinite timestamp traps (22003 — jed decimal is finite)", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT EXTRACT(year FROM timestamp 'infinity')")),
    "22003",
  );
  assert.equal(
    errCode(() => db.execute("SELECT EXTRACT(epoch FROM timestamptz '-infinity')")),
    "22003",
  );
});

test("a non-datetime / non-literal-text source to a datetime target is deferred (0A000)", () => {
  const db = dbWith([]);
  assert.equal(
    errCode(() => db.execute("SELECT CAST(1 + 1 AS timestamp)")),
    "0A000",
  );
  assert.equal(
    errCode(() => db.execute("SELECT CAST(current_setting('x.y', true) AS timestamptz)")),
    "0A000",
  );
});
