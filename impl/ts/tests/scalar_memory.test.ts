import assert from "node:assert/strict";
import { test } from "node:test";
import { Meter, LifetimeBudget } from "../src/cost.ts";
import { EngineError } from "../src/tooling.ts";
import { memDb } from "./mem_db.ts";
import { queryOutcome } from "./util.ts";

test("scalar budget stays with its cursor across other statements and setting changes", () => {
  const db = memDb();
  const s = db.session({ maxScalarBytes: 3n });
  s.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
  s.execute("INSERT INTO t VALUES (1), (2)");
  const old = s.query("SELECT repeat('x', 3) FROM t ORDER BY id");
  try {
    const it = old[Symbol.iterator]();
    assert.equal(it.next().done, false);
    s.setMaxScalarBytes(100n);
    queryOutcome(s, "SELECT repeat('y', 9)");
    assert.throws(
      () => it.next(),
      (e: unknown) => e instanceof EngineError && e.code() === "54P04",
    );
    s.setMaxScalarBytes(0n);
    assert.equal(s.maxScalarBytes, 67108864n);
  } finally {
    old.close();
    s.close();
    db.close();
  }
});

test("saturation preserves the first crossed ceiling", () => {
  const max = 9223372036854775807n;
  const life = new LifetimeBudget(max - 1n);
  life.total = max - 2n;
  const m = new Meter(max, life);
  m.charge(max);
  assert.equal(m.accrued, max);
  assert.equal(life.total, max);
  assert.throws(
    () => m.guard(),
    (e: unknown) => e instanceof EngineError && e.code() === "54P02",
  );
  m.charge(10n);
  assert.equal(m.accrued, max);
});
