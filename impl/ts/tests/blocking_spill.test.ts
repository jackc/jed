// Internal resource/cursor invariants cannot be expressed by SQL corpus rows alone.
import assert from "node:assert/strict";
import { mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { RowSpool, SpillMap, SpillMultiMap, sortSpool } from "../src/blocking.ts";
import { EngineError, createDatabase, queryOutcome, render, type Engine } from "../src/tooling.ts";
import { FileSpillSink } from "../src/spillfile.ts";
import { decodeSpillRow, encodeSpillRow, type SpillScratch, type SpillSink } from "../src/spill.ts";
import { arrayValue, intValue, textValue } from "../src/value.ts";

class CountedSink implements SpillSink {
  private backing: FileSpillSink;
  created = 0;
  live = 0;
  peak = 0;
  written = 0;
  failAfter = Infinity;
  constructor(dir: string) {
    this.backing = new FileSpillSink(dir);
  }
  writeRun(bytes: Uint8Array) {
    return this.backing.writeRun(bytes);
  }
  createScratch(): SpillScratch {
    const file = this.backing.createScratch();
    this.created++;
    this.live++;
    this.peak = Math.max(this.peak, this.live);
    let closed = false;
    return {
      get size() {
        return file.size;
      },
      read: (position, length) => file.read(position, length),
      write: (position, bytes) => {
        this.written += bytes.length;
        if (this.written > this.failAfter)
          throw new EngineError("io_error", "injected scratch write failure");
        file.write(position, bytes);
      },
      close: () => {
        if (!closed) {
          closed = true;
          this.live--;
          file.close();
        }
      },
    };
  }
}

function fixture() {
  const dir = mkdtempSync(join(tmpdir(), "jed-blocking-test-"));
  const database = createDatabase({ path: join(dir, "db.jed"), skipFsync: true });
  const sink = new CountedSink(dir);
  (database as unknown as { core: { storage: Engine } }).core.storage.spillSink = sink;
  const db = database.session();
  db.execute("CREATE TABLE t (id i32 PRIMARY KEY, k i32, v decimal, s text)");
  db.execute(
    "INSERT INTO t VALUES " +
      Array.from(
        { length: 80 },
        (_, id) => `(${id}, ${id % 5}, ${(id % 13) - 6}.25, '${"x".repeat(id % 11)}')`,
      ).join(","),
  );
  return {
    db,
    sink,
    dir,
    close: () => {
      db.close();
      database.close();
      rmSync(dir, { recursive: true, force: true });
    },
  };
}

test("blocking operators actually spill, retain exact rows/costs, and close every scratch handle", () => {
  const f = fixture();
  try {
    const queries = [
      "SELECT id, s FROM t ORDER BY s DESC, id",
      "SELECT a.id, b.id FROM t a JOIN t b ON a.k = b.k WHERE a.id < 4",
      "SELECT a.id, b.id FROM t a JOIN t b ON a.k = b.k ORDER BY a.id LIMIT 7 OFFSET 2",
      "SELECT a.k, count(*) FROM t a JOIN t b ON a.k = b.k JOIN t c ON b.k = c.k WHERE a.id < 2 GROUP BY a.k",
      "SELECT DISTINCT k, s FROM t ORDER BY s DESC, k",
      "SELECT k, sum(DISTINCT v), avg(v), min(s), count(*) FROM t GROUP BY k ORDER BY k",
      "SELECT k, mode() WITHIN GROUP (ORDER BY v), percentile_disc(0.6) WITHIN GROUP (ORDER BY v), percentile_cont(0.3) WITHIN GROUP (ORDER BY v) FROM t GROUP BY k",
      "SELECT k, rank(0::decimal) WITHIN GROUP (ORDER BY v), dense_rank(0::decimal) WITHIN GROUP (ORDER BY v), cume_dist(0::decimal) WITHIN GROUP (ORDER BY v) FROM t GROUP BY k",
      "SELECT k, json_agg(v), jsonb_agg(s), json_object_agg(id, s), jsonb_object_agg(s, v) FROM t GROUP BY k",
      "SELECT DISTINCT k FROM (SELECT k FROM t) q",
    ];
    for (const sql of queries) {
      f.db.setWorkMem(0);
      const expected = queryOutcome(f.db, sql);
      const before = f.sink.created;
      f.db.setWorkMem(128);
      const actual = queryOutcome(f.db, sql);
      assert.deepEqual(actual, expected, sql);
      assert.ok(f.sink.created > before, `no scratch for ${sql}`);
      assert.equal(f.sink.live, 0, sql);
      assert.equal(readdirSync(f.dir).filter((name) => name.startsWith("jed-spill-")).length, 0);
    }
    const before = f.sink.created;
    f.db.setWorkMem(1 << 20);
    queryOutcome(f.db, "SELECT a.id, b.id FROM t a JOIN t b ON a.k = b.k WHERE a.id < 4");
    queryOutcome(f.db, "SELECT k, sum(v), count(DISTINCT s) FROM t GROUP BY k");
    assert.equal(f.sink.created, before, "fitting hash/aggregate state should stay resident");
    assert.ok(f.sink.peak <= 64, `descriptor peak ${f.sink.peak}`);
  } finally {
    f.close();
  }
});

test("early cursor close, evaluator/cost failures, and scratch write failure release all files", () => {
  const f = fixture();
  try {
    f.db.setWorkMem(64);
    const cursor = f.db.query("SELECT DISTINCT id, s FROM t ORDER BY s, id");
    assert.equal(cursor[Symbol.iterator]().next().done, false);
    assert.ok(f.sink.live > 0);
    cursor.close();
    assert.equal(f.sink.live, 0);
    assert.throws(
      () => queryOutcome(f.db, "SELECT k, sum(1 / (id - 60)) FROM t GROUP BY k"),
      (error: unknown) => error instanceof EngineError && error.code() === "22012",
    );
    assert.equal(f.sink.live, 0);
    assert.throws(
      () => queryOutcome(f.db, "SELECT id FROM t WHERE 1 / (id - 60) <= 0 ORDER BY s"),
      (error: unknown) => error instanceof EngineError && error.code() === "22012",
    );
    assert.equal(f.sink.live, 0);
    f.db.setMaxCost(150n);
    assert.throws(
      () => queryOutcome(f.db, "SELECT k, sum(id + 1) FROM t GROUP BY k"),
      (error: unknown) => error instanceof EngineError && error.code() === "54P01",
    );
    assert.equal(f.sink.live, 0);
    f.db.setMaxCost(0n);
    f.sink.failAfter = f.sink.written + 64;
    assert.throws(
      () => queryOutcome(f.db, "SELECT DISTINCT s FROM t"),
      (error: unknown) => error instanceof EngineError && error.code() === "58030",
    );
    assert.equal(f.sink.live, 0);
  } finally {
    f.close();
  }
});

test("spool and skewed bucket replay stay streaming with bounded descriptors", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-blocking-primitives-"));
  const sink = new CountedSink(dir);
  const rows = new RowSpool(64, sink);
  const map = new SpillMap(64, sink);
  const buckets = new SpillMultiMap(64, sink);
  let sorted: RowSpool | undefined;
  try {
    for (let i = 0; i < 2048; i++) {
      rows.push([intValue(BigInt(2047 - i)), textValue("wide".repeat(32))]);
      map.set(`key:${i}`, [intValue(BigInt(-i))]);
      buckets.append("one hot hash", [intValue(BigInt(i))]);
    }
    assert.ok(sink.written > 64 * 1000);
    // White-box resource invariants: spilling must release the original resident inputs and
    // maps, as well as create files. A disk copy beside retained rows would fail these checks.
    assert.equal((rows as unknown as { rows: unknown[] }).rows.length, 0);
    assert.equal((map as unknown as { memory: Map<string, unknown> }).memory.size, 0);
    assert.equal((buckets as unknown as { memory: Map<string, unknown> }).memory.size, 0);
    for (let i = 0; i < 2048; i++) assert.equal(render(map.get(`key:${i}`)![0]!), String(-i));
    let count = 0;
    for (const row of buckets.get("one hot hash")) assert.equal(render(row[0]!), String(count++));
    assert.equal(count, 2048);
    sorted = sortSpool(
      rows,
      (a, b) => Number((a[0]! as { int: bigint }).int - (b[0]! as { int: bigint }).int),
      64,
      sink,
    );
    count = 0;
    for (const row of sorted) assert.equal(render(row[0]!), String(count++));
    assert.equal(count, 2048);
    assert.ok(sink.peak < 24, `descriptor peak ${sink.peak}`);
  } finally {
    rows.close();
    map.close();
    buckets.close();
    sorted?.close();
    assert.equal(sink.live, 0);
    rmSync(dir, { recursive: true, force: true });
  }
});

test("scratch codec preserves signed values and empty recursive arrays", () => {
  const row = [intValue(-123n), arrayValue([]), arrayValue([intValue(-1n)])];
  assert.deepEqual(decodeSpillRow(encodeSpillRow(row)), row);
});
