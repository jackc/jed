// Host-API surface of the live query-memory budget (spec/design/memory.md §2/§5): the setting's
// default and normalization, cursor lifetime, and the engine-owned `all` collector. Abort thresholds
// themselves are pinned in spec/conformance/suites/resource/query_memory.test.

import assert from "node:assert/strict";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { QueryAccount, queryMemoryUnderflows } from "../src/cost.ts";
import { EngineError, createDatabase, queryMemoryPeak, queryOutcome } from "../src/tooling.ts";
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
    // The columnar projection lane gathers the id column (100 values × 32 = 3200 bytes of lane state,
    // memory.md §6.5), held until its emission completes.
    s.setMaxQueryMemoryBytes(3200n);
    // Draining by hand: the lane is the engine's, the rows are the host's own memory.
    let n = 0;
    for (const _ of s.query("SELECT id FROM t")) n++;
    assert.equal(n, 100);
    // The engine-owned collector also charges each collected row (32 + 32 bytes): 3200 + 6400.
    s.setMaxQueryMemoryBytes(3200n + 64n * 100n);
    assert.equal(s.all("SELECT id FROM t").length, 100);
    s.setMaxQueryMemoryBytes(3200n + 64n * 100n - 1n);
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

// Spill-capable operator state charges only its resident portion (memory.md §6.6): on a file-backed
// database the bounded-spill lane's structures release their charge as they spill, so a budget that a
// fully resident DISTINCT exceeds is enough once workMem makes it spill — and the account itself never
// forces the spill. Disk-only behavior, so it lives here rather than in the corpus.
test("spilling operator state releases its charge", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-query-memory-spill-"));
  const db = createDatabase({ path: join(dir, "query_memory_spill.jed"), skipFsync: true });
  const s = db.session();
  try {
    s.execute("CREATE TABLE t (id i32 PRIMARY KEY, s text)");
    s.execute("INSERT INTO t SELECT g, repeat('x', g) FROM generate_series(1, 300) g");
    const sql = "SELECT DISTINCT s FROM t";
    const peak = (workMem: number): number => {
      s.setWorkMem(workMem);
      s.setMaxQueryMemoryBytes(1n << 40n);
      queryMemoryPeak.value = 0;
      queryOutcome(s, sql);
      return queryMemoryPeak.value;
    };
    const resident = peak(1 << 30);
    const spilling = peak(4096);
    assert.ok(spilling < resident / 4, `spilling peak ${spilling} vs resident ${resident}`);
    // The spilling run fits a budget the resident run exceeds.
    s.setMaxQueryMemoryBytes(BigInt(spilling));
    s.setWorkMem(4096);
    const outcome = queryOutcome(s, sql);
    assert.equal(outcome.kind === "query" ? outcome.rows.length : -1, 300);
    s.setWorkMem(1 << 30);
    assert.throws(() => queryOutcome(s, sql), is54P05);
  } finally {
    s.close();
    db.close();
    rmSync(dir, { recursive: true, force: true });
  }
});
