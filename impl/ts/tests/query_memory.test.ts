// Host-API surface of the live query-memory budget (spec/design/memory.md §2/§5): the setting's
// default and normalization, cursor lifetime, and the engine-owned `all` collector. Abort thresholds
// themselves are pinned in spec/conformance/suites/resource/query_memory.test.

import assert from "node:assert/strict";
import { test } from "node:test";
import { QueryAccount, queryMemoryUnderflows } from "../src/cost.ts";
import { EngineError } from "../src/tooling.ts";
import { memDb } from "./mem_db.ts";

const is54P05 = (e: unknown): boolean => e instanceof EngineError && e.code() === "54P05";

test("query memory setting defaults to unlimited", () => {
  const db = memDb();
  const s = db.session();
  try {
    assert.equal(s.maxQueryMemoryBytes, 0n);
    s.setMaxQueryMemoryBytes(4096n);
    assert.equal(s.maxQueryMemoryBytes, 4096n);
    s.setMaxQueryMemoryBytes(-1n);
    assert.equal(s.maxQueryMemoryBytes, 0n);
    // Unlimited never aborts, even for a large materialized result.
    let n = 0;
    for (const _ of s.query("SELECT repeat('x', 1000) FROM generate_series(1, 2000)")) n++;
    assert.equal(n, 2000);
  } finally {
    s.close();
    db.close();
  }
});

test("the engine-owned all() collector is admitted", () => {
  const db = memDb();
  const s = db.session();
  try {
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
    s.execute("INSERT INTO t SELECT g FROM generate_series(1, 100) g");
    s.setMaxQueryMemoryBytes(64n * 100n);
    // A streaming scan holds no row buffer; draining it by hand is the host's own memory.
    let n = 0;
    for (const _ of s.query("SELECT id FROM t")) n++;
    assert.equal(n, 100);
    // The engine-owned collector charges each collected row (32 + 32 bytes): 100 rows need 6400.
    assert.equal(s.all("SELECT id FROM t").length, 100);
    s.setMaxQueryMemoryBytes(64n * 100n - 1n);
    assert.throws(() => s.all("SELECT id FROM t"), is54P05);
  } finally {
    s.close();
    db.close();
  }
});

test("a collector 54P05 inside an open block aborts the block", () => {
  const db = memDb();
  const s = db.session();
  try {
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
    s.execute("INSERT INTO t VALUES (1), (2)");
    s.execute("BEGIN");
    s.setMaxQueryMemoryBytes(64n);
    assert.throws(() => s.all("SELECT id FROM t"), is54P05);
    s.setMaxQueryMemoryBytes(0n);
    assert.throws(
      () => s.all("SELECT id FROM t"),
      (e: unknown) => e instanceof EngineError && e.code() === "25P02",
    );
    s.execute("ROLLBACK");
  } finally {
    s.close();
    db.close();
  }
});

test("query memory budget stays with its cursor", () => {
  const db = memDb();
  const s = db.session({ maxQueryMemoryBytes: 1n << 20n });
  try {
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
    s.execute("INSERT INTO t VALUES (1), (2)");
    // A result opened under 1 MiB keeps that budget after the setting changes.
    const old = s.query("SELECT id FROM t ORDER BY id DESC");
    try {
      s.setMaxQueryMemoryBytes(1n);
      assert.throws(() => s.all("SELECT id FROM t ORDER BY id DESC"), is54P05);
      const it = old[Symbol.iterator]();
      assert.equal(it.next().done, false);
      assert.equal(it.next().done, false);
      assert.equal(it.next().done, true);
    } finally {
      old.close();
    }
  } finally {
    s.close();
    db.close();
  }
});

test("the account admits up to its limit and counts an over-release", () => {
  const acct = new QueryAccount(100);
  acct.reserve(60);
  acct.reserve(40); // equality is allowed
  assert.throws(() => acct.reserve(1), is54P05);
  acct.release(100);
  assert.equal(acct.used, 0);
  const before = queryMemoryUnderflows.count;
  acct.release(1); // a release without its reservation: clamped, and counted
  assert.equal(acct.used, 0);
  assert.equal(queryMemoryUnderflows.count, before + 1);
  // An unlimited account measures and counts nothing.
  const free = new QueryAccount(0);
  free.reserve(1 << 30);
  free.release(1 << 30);
  assert.equal(free.used, 0);
  assert.equal(queryMemoryUnderflows.count, before + 1);
});

test("a rejected collector admission reports a reached cost ceiling", () => {
  const db = memDb();
  const s = db.session();
  const is = (code: string) => (e: unknown) => e instanceof EngineError && e.code() === code;
  try {
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY)");
    s.execute("INSERT INTO t SELECT g FROM generate_series(1, 100) g");
    // A streaming scan holds no row buffer; only the collector charges (64 bytes per row).
    const sql = "SELECT id FROM t LIMIT 1000";
    const rows = s.query(sql);
    for (const _ of rows) {
      // drain
    }
    const cost = rows.cost;
    // At the exact ceiling the scan's next guard aborts once every row is out — with or without an
    // ample budget, which adds no guard point of its own.
    s.setMaxCost(cost);
    assert.throws(() => s.all(sql), is("54P01"));
    s.setMaxQueryMemoryBytes(64n * 100n);
    assert.throws(() => s.all(sql), is("54P01"));
    // When the last row's admission is rejected — right after its charge reached the ceiling and
    // before the scan's next guard — the reached ceiling wins.
    s.setMaxQueryMemoryBytes(64n * 100n - 1n);
    assert.throws(() => s.all(sql), is("54P01"));
    // Below the ceiling the memory error stands.
    s.setMaxCost(cost + 1n);
    assert.throws(() => s.all(sql), is("54P05"));
  } finally {
    s.close();
    db.close();
  }
});
