// date_trunc / EXTRACT / cross-family datetime casts — the deliberate PostgreSQL divergences
// (spec/design/timezones.md §9). The agreeing behavior is oracle-checked in
// suites/expr/{date_trunc,extract,datetime_cast}.test and runs on every core; these per-core tests
// cover only what the oracle corpus CANNOT express (CLAUDE.md §10) — the cases where jed deliberately
// differs from PG. Mirrors impl/rust/tests/datetime_conversions.rs.
//
//   - EXTRACT(field FROM ±infinity) of a monotonic field (year, decade, century, millennium, julian,
//     isoyear, epoch) over a timestamp, timestamptz, or date — jed's decimal is finite-only, so
//     22003; PG returns numeric ±Infinity. (The oscillating fields are NULL, as in PG, and
//     date_part's float8 carries the ±Infinity — both oracle-checked in
//     suites/expr/extract_infinity.test.)
//   - EXTRACT(timezone* FROM ±infinity timestamp) — jed validates EXTRACT's field at resolve, so it
//     is 0A000 as for a finite timestamp; PG decides per value and returns NULL.
//   - a non-datetime, non-text source to a datetime target — jed 42804 (PG 42846).

import assert from "node:assert/strict";
import { test } from "node:test";
import { dbWith, errCode } from "./util.ts";

test("EXTRACT of a monotonic field over an infinite datetime traps (22003 — jed decimal is finite)", () => {
  const db = dbWith([]);
  for (const field of ["year", "decade", "century", "millennium", "julian", "isoyear", "epoch"]) {
    for (const ty of ["timestamp", "timestamptz", "date"]) {
      for (const inf of ["infinity", "-infinity"]) {
        const sql = `SELECT EXTRACT(${field} FROM ${ty} '${inf}')`;
        assert.equal(
          errCode(() => db.execute(sql)),
          "22003",
          sql,
        );
      }
    }
  }
});

test("EXTRACT(timezone* FROM an infinite timestamp) is 0A000 at resolve (PG: NULL)", () => {
  const db = dbWith([]);
  for (const field of ["timezone", "timezone_hour", "timezone_minute"]) {
    const sql = `SELECT EXTRACT(${field} FROM timestamp 'infinity')`;
    assert.equal(
      errCode(() => db.execute(sql)),
      "0A000",
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
