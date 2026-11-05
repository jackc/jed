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
import { datePartJulian } from "../src/datetime_fn.ts";
import { makeTimestamp, parseTimestamp } from "../src/timestamp.ts";
import { dbWith, errCode, query } from "./util.ts";

test("timestamp construction and Julian extraction preserve the full finite i64 range", () => {
  const minimum = -(1n << 63n) + 1n;
  assert.equal(parseTimestamp("290309-12-21 19:59:05.224193 BC"), minimum);
  assert.equal(makeTimestamp(-290309n, 12n, 21n, 19n, 59n, 5.224193), minimum);
  for (const text of ["290309-12-21 19:59:05.224192 BC", "290309-12-21 19:59:05.224191 BC"]) {
    assert.equal(
      errCode(() => parseTimestamp(text)),
      "22008",
      text,
    );
  }
  const nearMinimum = parseTimestamp("290309-12-21 19:59:06 BC");
  assert.equal(datePartJulian(nearMinimum), -104311404 + 71946 / 86400);
  const db = dbWith([]);
  for (const sql of [
    "SELECT make_date('-9223372036854775808'::bigint, 1, 1)",
    "SELECT make_timestamp('-9223372036854775808'::bigint, 1, 1, 0, 0, 0.0::f64)",
    "SELECT make_timestamptz('-9223372036854775808'::bigint, 1, 1, 0, 0, 0.0::f64)",
  ]) {
    assert.equal(
      errCode(() => query(db, sql)),
      "22008",
      sql,
    );
  }
  assert.deepEqual(query(db, "SELECT EXTRACT(julian FROM timestamp '290309-12-21 19:59:06 BC')"), [
    ["-104311403.16729166666666666667"],
  ]);
});

test("date_trunc rejects results below jed's finite i64 timestamp range", () => {
  // jed's BC range extends past PostgreSQL's, so this belongs outside the oracle corpus.
  const db = dbWith([]);
  const lowest = "290309-12-21 19:59:05.224193 BC";
  for (const type of ["timestamp", "timestamptz"]) {
    for (const unit of [
      "millisecond",
      "second",
      "minute",
      "hour",
      "day",
      "week",
      "month",
      "quarter",
      "year",
      "decade",
      "century",
      "millennium",
    ]) {
      const sql = `SELECT date_trunc('${unit}', ${type} '${lowest}')`;
      assert.equal(
        errCode(() => query(db, sql)),
        "22008",
        sql,
      );
    }
  }
  assert.deepEqual(query(db, `SELECT date_trunc('microseconds', timestamp '${lowest}')`), [
    [lowest],
  ]);
  assert.deepEqual(query(db, "SELECT date_trunc('second', timestamp '290309-12-21 19:59:06 BC')"), [
    ["290309-12-21 19:59:06 BC"],
  ]);
});

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

test("timestamp month shifts preserve the earliest finite second", () => {
  // These BC dates are outside PostgreSQL's supported range.
  const db = dbWith([]);
  for (const type of ["timestamp", "timestamptz"]) {
    for (const shift of ["- interval '1 month'", "+ interval '-1 month'"]) {
      const sql = `SELECT (${type} '290308-01-21 19:59:05.224193 BC' ${shift}) = ${type} '290309-12-21 19:59:05.224193 BC'`;
      assert.deepEqual(query(db, sql), [["true"]], sql);
    }
    const sql = `SELECT ${type} '290308-01-21 19:59:05.224192 BC' - interval '1 month'`;
    assert.equal(
      errCode(() => query(db, sql)),
      "22008",
      sql,
    );
  }
});
