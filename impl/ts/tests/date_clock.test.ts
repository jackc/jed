// Clock-relative date literals — the parts the corpus cannot express (spec/design/date.md §6).
// The literal surface (all five specials, the STABLE statement clock, INSERT/UPDATE/DEFAULT, the
// 42P17 index rejections, and the strict INSERT…SELECT divergence) is corpus-tested with injected
// clocks (suites/types/date_clock.test, run on every core); this file covers only what that
// cannot reach: the SESSION-ZONE interaction (the corpus has no zone-setting directive — the
// session TimeZone is host-API-only, session.md §6.2) and the never-folded property observed
// across clock changes on ONE handle. Mirrors impl/go/date_clock_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { fixedClock } from "../src/seam.ts";
import { dbWith, query } from "./util.ts";

// 2024-07-15 23:30:00 UTC — half an hour before a UTC midnight, so a UTC+2 session zone is
// already on 2024-07-16 while UTC is still on 2024-07-15.
const NEAR_MIDNIGHT_UTC = 1721086200000000n;

function clockDB(micros: bigint, stmts: string[]): ReturnType<typeof dbWith> {
  const db = dbWith(stmts);
  db.setClockSource(fixedClock(micros));
  return db;
}

const isNextDay = (db: ReturnType<typeof dbWith>, expr: string): boolean =>
  query(db, `SELECT (${expr}) = '2024-07-16'::date`)[0]![0] === "true";

test("clock-relative date literals use the session zone", () => {
  const db = clockDB(NEAR_MIDNIGHT_UTC, []);
  // Default UTC session: still 2024-07-15.
  assert.equal(isNextDay(db, "'today'::date"), false);
  // A UTC+2 session zone (the POSIX fixed-offset spelling '-02:00' — positive is WEST,
  // timezones.md §6): local wall clock is 01:30 on 2024-07-16.
  db.setTimeZone("-02:00");
  assert.equal(isNextDay(db, "'today'::date"), true);
  // The runtime text→date cast consults the same zone.
  db.execute("CREATE TABLE s (id i32 PRIMARY KEY, w text)");
  db.execute("INSERT INTO s VALUES (1, 'today')");
  assert.equal(isNextDay(db, "(SELECT w FROM s WHERE id = 1) :: date"), true);
});

test("DEFAULT 'today' never folds at CREATE TABLE", () => {
  // The DEFAULT is created under clock A; an INSERT under clock B (a day later) must see B's
  // day — a CREATE-TABLE-folded constant (PostgreSQL's behavior) could not. Same handle.
  const db = clockDB(NEAR_MIDNIGHT_UTC, [
    "CREATE TABLE d (id i32 PRIMARY KEY, dt date DEFAULT 'today')",
  ]);
  db.execute("INSERT INTO d (id) VALUES (1)");
  db.setClockSource(fixedClock(NEAR_MIDNIGHT_UTC + 86_400_000_000n));
  db.execute("INSERT INTO d (id) VALUES (2)");
  assert.equal(
    query(
      db,
      "SELECT (SELECT dt FROM d WHERE id = 2) = (SELECT dt FROM d WHERE id = 1) + 1",
    )[0]![0],
    "true",
  );
});
