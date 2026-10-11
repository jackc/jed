// Zone-shift range guard (timezones.md §6): every path that moves a finite timestamp across a UT
// offset — the cross-family casts, AT TIME ZONE, date_trunc/EXTRACT/date_part in a zone,
// make_timestamptz, and the timestamptz → text render — traps 22008 when the shifted reading leaves
// jed's i64-µs range or lands exactly on a ±infinity sentinel (i64 min/max). jed's timestamp range
// (≈ 290309 BC … 294247 AD) is far wider than PostgreSQL's, so none of these extremes are
// oracle-checkable; this file pins them per core. Mirrors impl/rust/tests/zone_shift_range.rs and
// impl/go/zone_shift_range_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { EngineError } from "../src/errors.ts";
import { dbWith, queryOutcome } from "./util.ts";

// i64 max µs is 294247-01-10 04:00:54.775807 UTC; i64 min µs is 290309-12-21 19:59:05.224192 BC.
// Each seeded value sits within a few hours of an end. Session zones use the POSIX fixed-offset
// spelling, where a positive offset is WEST of UTC ('+05:00' = UTC-5, '-05:00' = UTC+5).
const TS_HI = "294247-01-10 04:00:00"; // shifts past i64 max in a UTC-5 reading
const TS_LO = "290309-12-22 00:00:00 BC"; // shifts past i64 min in a UTC+5 reading
const TS_LO_EDGE = "290309-12-22 00:59:05.224192 BC"; // i64 min + 5h exactly
const TZ_HI = "294247-01-10 04:00:00+00"; // + 5h overflows
const TZ_HI_EDGE = "294247-01-09 23:00:54.775807+00"; // i64 max − 5h exactly
const TZ_LO = "290309-12-22 00:00:00+00 BC"; // − 5h overflows
const I64_MAX = 9223372036854775807n;

// t(id i32 pk, ts timestamp, tz timestamptz, d date) with the near-range rows.
function seeded(): ReturnType<typeof dbWith> {
  return dbWith([
    "CREATE TABLE t (id i32 PRIMARY KEY, ts timestamp, tz timestamptz, d date)",
    `INSERT INTO t VALUES (1, '${TS_HI}', '${TZ_HI}', '294247-01-10')`,
    `INSERT INTO t VALUES (2, '${TS_LO}', '${TZ_LO}', '290309-12-22 BC')`,
    `INSERT INTO t VALUES (3, '${TS_LO_EDGE}', '${TZ_HI_EDGE}', '300000-01-01')`,
  ]);
}

function at(db: ReturnType<typeof dbWith>, expr: string, id: number) {
  const o = queryOutcome(db, `SELECT ${expr} FROM t WHERE id = ${id}`);
  if (o.kind !== "query") throw new Error("expected a query result");
  return o.rows[0][0];
}

function traps(db: ReturnType<typeof dbWith>, zone: string, expr: string, id: number): void {
  db.setTimeZone(zone);
  let got: unknown;
  try {
    got = at(db, expr, id);
  } catch (e) {
    if (!(e instanceof EngineError)) throw e;
    assert.equal(e.code(), "22008", `${zone} ${expr} (row ${id})`);
    return;
  }
  assert.fail(
    `${zone} ${expr} (row ${id}): expected 22008, got ${JSON.stringify(got, (_, v) => (typeof v === "bigint" ? v.toString() : v))}`,
  );
}

test("a zone shift past i64 max traps 22008", () => {
  const db = seeded();
  // A wall clock read in a UTC-5 zone moves +5h.
  for (const expr of [
    "ts :: timestamptz",
    "ts AT TIME ZONE '+05:00'",
    "make_timestamptz(294247, 1, 10, 4, 0, 0)",
    "d :: timestamptz", // midnight, read in the session zone
  ]) {
    traps(db, "+05:00", expr, 1);
  }
  traps(db, "UTC", "make_timestamptz(294247, 1, 10, 4, 0, 0, '+05:00')", 1);
  // An instant rendered in a UTC+5 zone moves +5h.
  for (const expr of [
    "tz :: timestamp",
    "tz :: date",
    "tz :: text",
    "tz AT TIME ZONE '-05:00'",
    "date_trunc('day', tz)",
    "EXTRACT(hour FROM tz)",
    "date_part('hour', tz)",
  ]) {
    traps(db, "-05:00", expr, 1);
  }
  traps(db, "UTC", "date_trunc('day', tz, '-05:00')", 1);
});

test("a zone shift past i64 min traps 22008", () => {
  const db = seeded();
  // A wall clock read in a UTC+5 zone moves −5h.
  for (const expr of ["ts :: timestamptz", "ts AT TIME ZONE '-05:00'", "d :: timestamptz"]) {
    traps(db, "-05:00", expr, 2);
  }
  // An instant rendered in a UTC-5 zone moves −5h.
  for (const expr of [
    "tz :: timestamp",
    "tz :: date",
    "tz :: text",
    "tz AT TIME ZONE '+05:00'",
    "date_trunc('day', tz)",
    "EXTRACT(hour FROM tz)",
    "date_part('hour', tz)",
  ]) {
    traps(db, "+05:00", expr, 2);
  }
});

test("a zone shift onto a ±infinity sentinel traps 22008", () => {
  const db = seeded();
  // Landing EXACTLY on i64 min / max would silently become ±infinity.
  traps(db, "-05:00", "ts :: timestamptz", 3);
  traps(db, "UTC", "ts AT TIME ZONE '-05:00'", 3);
  traps(db, "-05:00", "tz :: timestamp", 3);
  traps(db, "-05:00", "tz :: text", 3);
  traps(db, "UTC", "tz AT TIME ZONE '-05:00'", 3);
  // One microsecond inside the range is representable.
  db.setTimeZone("-05:00");
  const v = at(db, "(tz - INTERVAL '1 microsecond') :: timestamp", 3);
  assert.deepEqual(v, { kind: "timestamp", micros: I64_MAX - 1n });
});

test("a date widened past the timestamp range traps 22008", () => {
  // A date is wider than the timestamp range (date.md §1): its midnight widening must trap, not
  // produce an out-of-i64 product.
  const db = seeded();
  traps(db, "UTC", "d :: timestamp", 3);
  traps(db, "UTC", "d :: timestamptz", 3);
});

test("the same values shift inward without error", () => {
  // The guard rejects only an out-of-range result: the opposite-signed zone moves each value toward
  // the epoch and succeeds.
  const db = seeded();
  const kind = (zone: string, expr: string, id: number): string => {
    db.setTimeZone(zone);
    return at(db, expr, id).kind;
  };
  assert.equal(kind("-05:00", "ts :: timestamptz", 1), "timestamptz");
  assert.equal(kind("-05:00", "tz :: text", 2), "text");
  assert.equal(kind("+05:00", "tz :: timestamp", 1), "timestamp");
  assert.equal(kind("+05:00", "ts :: timestamptz", 2), "timestamptz");
});
