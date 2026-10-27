// Internal resource/cursor invariants cannot be expressed by SQL corpus rows alone.
import assert from "node:assert/strict";
import { mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { RowSpool, SpillMap, SpillMultiMap, SpoolSorter, sortSpool } from "../src/blocking.ts";
import { QueryAccount, StateCharge } from "../src/cost.ts";
import { rowBytes } from "../src/memsize.ts";
import { EngineError, createDatabase, queryOutcome, render, type Engine } from "../src/tooling.ts";
import { FileSpillSink } from "../src/spillfile.ts";
import {
  decodeSpillRow,
  encodeSpillRow,
  Sorter,
  type SpillScratch,
  type SpillSink,
} from "../src/spill.ts";
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

test("a spilled map keeps lookups short as keys grow, and values replace in place or move", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-blocking-map-"));
  const sink = new CountedSink(dir);
  const map = new SpillMap(64, sink);
  const keys = 20000;
  const long = (i: number) => `long:${i}:${"y".repeat(5000)}`;
  try {
    for (let i = 0; i < keys; i++) map.set(`key:${i}`, [intValue(BigInt(i))]);
    for (let i = 0; i < 20; i++) map.set(long(i), [textValue(`v${i}`)]);
    // White-box: the directory grew past its first segment by splitting, so chains stay about one
    // node long instead of keys / 4096.
    const shape = map as unknown as { segments: number[]; base: number; split: number };
    assert.ok(shape.segments.length > 2, `segments ${shape.segments.length}`);
    assert.ok(keys + 20 <= shape.base + shape.split);
    // Same-size values rewrite in place; longer ones move to a larger block; shorter ones reuse it.
    for (let i = 0; i < keys; i += 3) map.set(`key:${i}`, [intValue(BigInt(-i))]);
    for (let i = 1; i < keys; i += 3) map.set(`key:${i}`, [textValue("w".repeat(i % 200))]);
    for (let i = 1; i < keys; i += 9) map.set(`key:${i}`, [textValue("s")]);
    const end = () => (map as unknown as { file: { end: number } }).file.end;
    const before = end();
    for (let i = 0; i < keys; i += 3) map.set(`key:${i}`, [intValue(BigInt(i * 7))]);
    assert.equal(end(), before, "same-size replacement should not append");
    for (let i = 0; i < keys; i++) {
      const want =
        i % 3 === 0
          ? String(i * 7)
          : i % 9 === 1
            ? "s"
            : i % 3 === 1
              ? "w".repeat(i % 200)
              : String(i);
      assert.equal(render(map.get(`key:${i}`)![0]!), want, `key:${i}`);
    }
    for (let i = 0; i < 20; i++) assert.equal(render(map.get(long(i))![0]!), `v${i}`);
    assert.equal(map.get("key:-1"), undefined);
    assert.equal(map.get(long(20)), undefined);
    assert.equal(map.insert("key:5", 0), false);
    assert.equal(map.insert("key:fresh", 0), true);
    assert.equal(map.insert("key:fresh", 0), false);
    assert.equal(sink.peak, 1);
  } finally {
    map.close();
    assert.equal(sink.live, 0);
    rmSync(dir, { recursive: true, force: true });
  }
});

test("a spilled spool replays rows larger than its buffers, repeatedly and while growing", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-blocking-spool-"));
  const sink = new CountedSink(dir);
  const spool = new RowSpool(64, sink);
  const width = (i: number) => (i % 50 === 7 ? 200_000 : i % 13);
  try {
    for (let i = 0; i < 400; i++)
      spool.push([intValue(BigInt(i)), textValue("z".repeat(width(i)))]);
    const check = (count: number) => {
      let n = 0;
      for (const row of spool) {
        assert.equal(render(row[0]!), String(n));
        assert.equal((row[1] as { text: string }).text.length, width(n));
        n++;
      }
      assert.equal(n, count);
    };
    check(400);
    check(400);
    // A replay sees the rows present when it began, even if more arrive meanwhile.
    let n = 0;
    for (const _ of spool) if (n++ < 5) spool.push([intValue(BigInt(400 + n - 1)), textValue("")]);
    assert.equal(n, 400);
    assert.equal(spool.length, 405);
  } finally {
    spool.close();
    assert.equal(sink.live, 0);
    rmSync(dir, { recursive: true, force: true });
  }
});

test("scratch codec preserves signed values and empty recursive arrays", () => {
  const row = [intValue(-123n), arrayValue([]), arrayValue([intValue(-1n)])];
  assert.deepEqual(decodeSpillRow(encodeSpillRow(row)), row);
});

// memory.md §6.8: each head read back from a spilled run reserves its row_bytes while it is in the
// merge heap; the final in-memory buffer's rows are already charged with the buffer. A head the account
// rejects is held uncharged: the merge cannot spill further.
test("merge heads of spilled runs are charged", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-blocking-test-"));
  const files = new FileSpillSink(dir);
  // A run-file-only sink selects the external Sorter; a scratch sink selects the SpoolSorter.
  const runSink: SpillSink = { writeRun: (bytes) => files.writeRun(bytes) };
  const compare = (a: readonly unknown[], b: readonly unknown[]) =>
    Number((a[0] as { int: bigint }).int) - Number((b[0] as { int: bigint }).int);
  const rowSize = rowBytes([intValue(0n), intValue(0n)]);
  const drain = (sorted: { next(): unknown; close(): void }) => {
    let count = 0;
    while (sorted.next() !== null) count++;
    return count;
  };
  try {
    // Budget 3 rows: rows 0-3 and 4-7 spill as runs, rows 8-9 stay buffered. The external Sorter
    // keeps both runs; the SpoolSorter compacts them into one.
    for (const [make, runs] of [
      [(c: StateCharge) => new Sorter(compare, rowSize * 3, runSink, c), 2],
      [(c: StateCharge) => new SpoolSorter(compare, rowSize * 3, files, c), 1],
    ] as const) {
      const acct = new QueryAccount(2 ** 40);
      const sort = make(new StateCharge(acct));
      for (let i = 0; i < 10; i++) sort.push([intValue(BigInt(i % 3)), intValue(BigInt(i))]);
      const sorted = sort.finish();
      assert.equal(acct.used, (runs + 2) * rowSize);
      assert.equal(drain(sorted), 10);
      sorted.close();
      assert.equal(acct.used, 0);

      const tight = new QueryAccount(1);
      const tightSort = make(new StateCharge(tight));
      for (let i = 0; i < 10; i++) tightSort.push([intValue(BigInt(i % 3)), intValue(BigInt(i))]);
      const tightSorted = tightSort.finish();
      assert.equal(drain(tightSorted), 10);
      tightSorted.close();
      assert.equal(tight.used, 0);
    }
  } finally {
    rmSync(dir, { recursive: true, force: true });
  }
});

// An account well below work_mem makes each spill structure spill on its first rejected reservation
// instead of throwing 54P05; the rejected element is uncharged, nothing stays charged once spilled,
// and every element is still readable in order (memory.md §6.6).
test("rejected reservations spill instead of failing", () => {
  const dir = mkdtempSync(join(tmpdir(), "jed-blocking-test-"));
  const sink = new CountedSink(dir);
  const budget = 1 << 30;
  const charged = () => {
    const acct = new QueryAccount(200);
    return { acct, charge: new StateCharge(acct) };
  };
  const int = (row: readonly unknown[]) => Number((row[0] as { int: bigint }).int);
  const compare = (a: readonly unknown[], b: readonly unknown[]) => int(a) - int(b);
  const row = (i: number) => [intValue(BigInt(i)), textValue("x".repeat(40))];
  const s = charged();
  const spool = new RowSpool(budget, sink, s.charge);
  const m = charged();
  const map = new SpillMap(budget, sink, m.charge);
  const h = charged();
  const buckets = new SpillMultiMap(budget, sink, h.charge);
  const so = charged();
  const sorter = new Sorter(compare, budget, sink, so.charge);
  const ss = charged();
  const spoolSorter = new SpoolSorter(compare, budget, sink, ss.charge);
  try {
    for (let i = 0; i < 100; i++) {
      spool.push(row(i));
      map.set(`key:${i}`, row(i), 8);
      buckets.append("hot", row(i));
      sorter.push([intValue(BigInt(i % 7)), intValue(BigInt(i))]);
      spoolSorter.push([intValue(BigInt(i % 7)), intValue(BigInt(i))]);
      assert.ok(so.acct.used <= 200 && ss.acct.used <= 200);
    }
    for (const { acct } of [s, m, h]) assert.equal(acct.used, 0);
    let count = 0;
    for (const r of spool) assert.equal(int(r), count++);
    assert.equal(count, 100);
    assert.equal(int(map.get("key:42")!), 42);
    count = 0;
    for (const r of buckets.get("hot")) assert.equal(int(r), count++);
    assert.equal(count, 100);
    for (const sorted of [sorter.finish(), spoolSorter.finish()]) {
      let previous = [-1, -1];
      count = 0;
      for (let r = sorted.next(); r !== null; r = sorted.next()) {
        const key = int(r);
        const position = Number((r[1] as { int: bigint }).int);
        assert.ok(key > previous[0]! || (key === previous[0] && position > previous[1]!));
        previous = [key, position];
        count++;
      }
      assert.equal(count, 100);
      sorted.close();
    }
  } finally {
    spool.close();
    map.close();
    buckets.close();
    rmSync(dir, { recursive: true, force: true });
  }
});
