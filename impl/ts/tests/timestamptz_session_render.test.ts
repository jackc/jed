// Session-zone rendering — the parts the corpus cannot express (spec/design/timezones.md §9.5). The
// PG-agreeing surface (result cells in the record's zone, timestamptz nested in array / composite /
// range output and in `array → text`, its cost and 42P17) is oracle-checked in
// suites/types/timestamptz_session_render.test; this file covers the host render API — the canonical
// render stays UTC, Session.render / Transaction.render follow the session's current zone, and a
// render at jed's out-of-PG-range extremes is 22008 — plus the documented divergence that EXPLAIN
// keeps the canonical UTC spelling of a timestamptz constant. Mirrors
// impl/rust/tests/timestamptz_session_render.rs and impl/go/timestamptz_session_render_test.go.

import assert from "node:assert/strict";
import { test } from "node:test";
import { intValue, nullValue, render, type Value } from "../src/value.ts";
import { dbWith, errCode, queryOutcome } from "./util.ts";

type Db = ReturnType<typeof dbWith>;

function one(db: Db, sql: string): Value {
  const o = queryOutcome(db, sql);
  if (o.kind !== "query") throw new Error(`expected a query result for ${sql}`);
  return o.rows[0]![0]!;
}

test("session render follows the current zone", () => {
  const db = dbWith([]);
  const v = one(db, "SELECT ARRAY[timestamptz '2024-01-15 12:00:00+00', timestamptz 'infinity']");
  // The default session zone is UTC: the session render equals the canonical render.
  assert.equal(db.render(v), render(v));
  // The value is zone-free; the render reads the zone in effect when it is called. The POSIX
  // fixed-offset spelling '+05:00' is UTC-5.
  db.setTimeZone("+05:00");
  assert.equal(db.render(v), '{"2024-01-15 07:00:00-05",infinity}');
  db.setTimeZone("-05:30");
  assert.equal(db.render(v), '{"2024-01-15 17:30:00+05:30",infinity}');
  // The canonical render stays zone-free whatever the session zone.
  assert.equal(render(v), '{"2024-01-15 12:00:00+00",infinity}');
  // A value with no timestamptz renders the same either way.
  assert.equal(db.render(intValue(7n)), "7");
  assert.equal(db.render(nullValue()), "NULL");
});

test("transaction render uses the owning session zone", () => {
  const db = dbWith([]);
  db.setTimeZone("+05:00");
  const text = db.update((tx) => {
    let v: Value | undefined;
    for (const row of tx.query("SELECT timestamptz '2024-01-15 12:00:00+00'")) {
      v = row[0];
      break;
    }
    return tx.render(v!);
  });
  assert.equal(text, "2024-01-15 07:00:00-05");
});

test("session render at the i64 extreme is 22008", () => {
  // i64::MAX µs is 294247-01-10 04:00:54.775807 UTC — far outside PostgreSQL's range. A UTC-5
  // reading of a finite instant within five hours of it overflows (timezones.md §6).
  const db = dbWith([]);
  const v = one(db, "SELECT timestamptz '294247-01-10 04:00:00+00'");
  assert.equal(render(v), "294247-01-10 04:00:00+00");
  db.setTimeZone("-05:00");
  assert.equal(
    errCode(() => db.render(v)),
    "22008",
  );
  // Nested in a container, the same instant fails the whole render.
  const arr = one(db, "SELECT ARRAY[timestamptz '294247-01-10 04:00:00+00']");
  assert.equal(
    errCode(() => db.render(arr)),
    "22008",
  );
});

test("EXPLAIN keeps the canonical UTC constant", () => {
  // A documented divergence: PostgreSQL deparses the constant in the session zone; jed's plan text is
  // context-free, so the constant prints in its canonical UTC spelling under any session zone.
  const db = dbWith(["CREATE TABLE ev (id i32 PRIMARY KEY, at timestamptz)"]);
  db.setTimeZone("+05:00");
  const o = queryOutcome(
    db,
    "EXPLAIN (VERBOSE) SELECT id FROM ev WHERE at > timestamptz '2024-01-15 12:00:00+00'",
  );
  if (o.kind !== "query") throw new Error("expected a query result");
  const text = o.rows
    .flat()
    .filter((v) => v.kind === "text")
    .map((v) => (v as Value & { kind: "text" }).text)
    .join("\n");
  assert.ok(
    text.includes("'2024-01-15 12:00:00+00'"),
    `EXPLAIN should print the canonical UTC constant:\n${text}`,
  );
});
