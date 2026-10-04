// Faults at the byte-device seam exercise the production serializer, allocator, and recovery.
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";
import type { BlockStore } from "../src/blockstore.ts";
import { crc64Update, metaChecksum, parseCommitMeta } from "../src/commit_manifest.ts";
import { type Engine, Snapshot } from "../src/executor.ts";
import { loadEngine, loadEnginePaged, toImage } from "../src/format.ts";
import { MemoryBlockStore } from "../src/memoryblockstore.ts";
import { Pager } from "../src/pager.ts";
import { SharedPaging } from "../src/paging.ts";
import { persistImpl } from "../src/persist.ts";
import { EngineError, execute } from "../src/tooling.ts";

test("complete manifest with self-freeing dependencies is corrupt", () => {
  const image = readFileSync(
    new URL("../../../spec/fileformat/fixtures/cow_invalid_free_list.jed", import.meta.url),
  );
  assert.throws(
    () => loadEngine(image),
    (error: unknown) => error instanceof EngineError && error.code() === "XX001",
  );
});

class RecordingStore implements BlockStore {
  memory: MemoryBlockStore;
  writes: { offset: number; bytes: Uint8Array }[] = [];
  events: string[] = [];
  durable: Uint8Array;
  reads = 0;
  constructor(image: Uint8Array) {
    this.memory = new MemoryBlockStore(image);
    this.durable = image.slice();
  }
  readAt(offset: number, len: number): Uint8Array {
    this.reads++;
    return this.memory.readAt(offset, len);
  }
  writeAt(offset: number, bytes: Uint8Array): void {
    this.events.push("write");
    this.writes.push({ offset, bytes: bytes.slice() });
    this.memory.writeAt(offset, bytes);
  }
  size(): number {
    return this.memory.size();
  }
  setSize(size: number): void {
    this.memory.setSize(size);
    this.durable = this.image();
  }
  sync(): void {
    this.events.push("sync");
    this.durable = this.image();
  }
  close(): void {}
  image(): Uint8Array {
    return this.memory.readAt(0, this.memory.size());
  }
  reset(): void {
    this.events = [];
    this.writes = [];
  }
}

function openStore(store: RecordingStore): Engine {
  const db = loadEnginePaged(new SharedPaging(Pager.fromStore(store), 64));
  db.persistHook = persistImpl;
  return db;
}
function setup(pageSize = 256): { db: Engine; store: RecordingStore } {
  const store = new RecordingStore(toImage(new Snapshot(1n), pageSize, 1n));
  const db = openStore(store);
  db.paging!.reserve(512); // exclude allocating barriers from steady-state assertions
  execute(db, "CREATE TABLE t (id i32 PRIMARY KEY, value text)");
  execute(db, "INSERT INTO t VALUES (1, 'old')");
  store.reset();
  return { db, store };
}
function count(image: Uint8Array): bigint {
  const db = loadEngine(image);
  const out = execute(db, "SELECT count(*) FROM t");
  assert.equal(out.kind, "query");
  if (out.kind !== "query" || out.rows[0][0].kind !== "int") throw new Error("expected integer");
  return out.rows[0][0].int;
}
function apply(image: Uint8Array, writes: { offset: number; bytes: Uint8Array }[]): Uint8Array {
  const copy = image.slice();
  for (const write of writes) copy.set(write.bytes, write.offset);
  return copy;
}

test("CRC64 ECMA-182 check value and incremental extension", () => {
  const input = new TextEncoder().encode("123456789");
  assert.equal(crc64Update(0n, input), 0x6c40df5f0b497347n);
  for (let split = 0; split <= input.length; split++) {
    assert.equal(
      crc64Update(crc64Update(0n, input.subarray(0, split)), input.subarray(split)),
      0x6c40df5f0b497347n,
    );
  }
});

test("one flush per steady commit; every persisted write subset recovers one complete generation", () => {
  const { db, store } = setup();
  const prior = store.image();
  execute(db, "INSERT INTO t VALUES (2, 'new')");
  assert.equal(store.events.filter((event) => event === "sync").length, 1);
  assert.ok(store.writes.length < 12);
  for (let mask = 0; mask < 2 ** store.writes.length; mask++) {
    const selected = store.writes.filter((_, index) => (mask & (1 << index)) !== 0);
    const rows = count(apply(prior, selected));
    assert.ok(rows === 1n || rows === 2n, `write subset ${mask} is atomic`);
    if (mask === 2 ** store.writes.length - 1) assert.equal(rows, 2n);
  }
});

test("overflow descriptors reject every missing or torn dependency even with meta persisted first", () => {
  const { db, store } = setup();
  const prior = store.image();
  const rows = Array.from({ length: 160 }, (_, i) => `(${i + 2}, '${"x".repeat(25)}')`);
  execute(db, `INSERT INTO t VALUES ${rows.join(", ")}`);
  assert.equal(store.events.filter((event) => event === "sync").length, 1);
  const metaWrite = store.writes.find((write) => write.offset < 512)!;
  const meta = parseCommitMeta(metaWrite.bytes, 256, store.size() / 256)!;
  assert.ok(meta.overflowCount > 1, "large commit uses chained overflow descriptors");
  assert.equal(count(store.image()), 161n);
  for (let missing = 0; missing < store.writes.length; missing++) {
    const kept = store.writes.filter((_, i) => i !== missing);
    assert.equal(count(apply(prior, kept)), 1n, `missing write ${missing}`);
    const torn = apply(prior, kept);
    const write = store.writes[missing];
    torn.set(write.bytes.subarray(0, 19), write.offset);
    assert.equal(count(torn), 1n, `torn write ${missing}`);
  }
  const body = store.writes.filter((write) => write.offset >= 512);
  for (let prefix = 0; prefix <= body.length; prefix++) {
    assert.equal(
      count(apply(prior, [metaWrite, ...body.slice(0, prefix)])),
      prefix === body.length ? 161n : 1n,
    );
  }
});

test("old internally checksummed pages cannot satisfy a new descriptor", () => {
  const { db, store } = setup();
  const before = store.image();
  execute(db, "INSERT INTO t VALUES (2, 'new')");
  const after = store.image();
  const target = store.writes.find((write) => write.offset >= 512 && write.bytes[0] === 2)!;
  let oldLeaf: Uint8Array | undefined;
  for (let offset = 512; offset < before.length; offset += 256) {
    if (before[offset] === 2) {
      oldLeaf = before.slice(offset, offset + 256);
      break;
    }
  }
  assert.ok(oldLeaf);
  after.set(oldLeaf, target.offset);
  assert.equal(count(after), 1n);
});

test("a recovered complete generation is flushed before successor writes and survives a second crash", () => {
  const { db, store } = setup();
  execute(db, "INSERT INTO t VALUES (2, 'unacknowledged')");
  const recoveredStore = new RecordingStore(store.image());
  // The disk has a complete candidate, but its predecessor may still be the only durably acknowledged
  // state. The first successor's adoption sync must precede even its first body write.
  const recovered = openStore(recoveredStore);
  recoveredStore.reset();
  recovered.paging!.armFault({ point: "body_write", n: 1 });
  assert.throws(() => execute(recovered, "INSERT INTO t VALUES (3, 'interrupted')"));
  assert.deepEqual(recoveredStore.events, ["sync"]);
  assert.equal(count(recoveredStore.durable), 2n);
  assert.throws(
    () => execute(recovered, "INSERT INTO t VALUES (4, 'must fail')"),
    /incomplete durable commit/,
  );
  assert.deepEqual(recoveredStore.events, ["sync"], "poisoned writer performs no further writes");
});

test("reopen pays one adoption barrier, then resumes one-flush commits", () => {
  const { store } = setup();
  const db = openStore(store);
  store.reset();
  execute(db, "INSERT INTO t VALUES (2, 'after open')");
  assert.equal(store.events[0], "sync");
  assert.equal(store.events.filter((event) => event === "sync").length, 2);
  // Shared freshness checks accepting this handle's already acknowledged meta must not add a barrier.
  loadEnginePaged(db.paging!);
  store.reset();
  execute(db, "INSERT INTO t VALUES (3, 'steady')");
  assert.equal(store.events.filter((event) => event === "sync").length, 1);
  assert.equal(count(store.image()), 3n);
});

test("failed final barrier poisons the writer and leaves a recoverable generation", () => {
  const { db, store } = setup();
  db.paging!.armFault({ point: "sync", n: 1 });
  assert.throws(() => execute(db, "INSERT INTO t VALUES (2, 'new')"));
  const writeCount = store.writes.length;
  assert.throws(() => execute(db, "DELETE FROM t"), /incomplete durable commit/);
  assert.equal(store.writes.length, writeCount);
  assert.equal(count(store.image()), 2n);
});

test("serialization errors leave local and recovered writers usable without storage work", () => {
  for (const recovered of [false, true]) {
    const initial = setup();
    const { store } = initial;
    const db = recovered ? openStore(store) : initial.db;
    const before = store.image();
    store.reset();
    const columns = Array.from({ length: 40 }, (_, i) => `c${i} i32`).join(",");
    assert.throws(
      () => execute(db, `CREATE TABLE wide (${columns})`),
      (error: unknown) => error instanceof EngineError && error.code() === "0A000",
    );
    assert.deepEqual(store.events, []);
    assert.deepEqual(store.image(), before);
    execute(db, "INSERT INTO t VALUES (2, 'after serialization error')");
    assert.equal(count(store.image()), 2n);
    assert.equal(store.events.filter((event) => event === "sync").length, recovered ? 2 : 1);
    if (recovered) assert.equal(store.events[0], "sync");
  }
});

test("meta checksum covers descriptor entries and zero padding", () => {
  const { db, store } = setup();
  execute(db, "INSERT INTO t VALUES (2, 'new')");
  const image = store.image();
  const slot = Number(db.txid & 1n);
  for (const offset of [64, 100, 255]) {
    const damaged = image.slice();
    damaged[slot * 256 + offset] ^= 1;
    assert.equal(count(damaged), 1n);
  }
  const meta = image.slice(slot * 256, (slot + 1) * 256);
  assert.equal(metaChecksum(meta), new DataView(meta.buffer).getUint32(32, false));
});

test("overflow descriptors and free-list pages reuse safely across sustained churn", () => {
  const { db, store } = setup();
  const rows = Array.from({ length: 100 }, (_, i) => `(${i + 2}, '${"x".repeat(30)}')`);
  execute(db, `INSERT INTO t VALUES ${rows.join(", ")}`);
  let middleHighWater = 0;
  let prior = store.image();
  for (let generation = 0; generation < 100; generation++) {
    prior = store.image();
    store.reset();
    execute(db, `UPDATE t SET value = '${"x".repeat(30)}-${generation}'`);
    if (generation === 49) middleHighWater = db.pageCount;
  }
  assert.ok(
    db.pageCount <= middleHighWater + 8,
    "overflow allocation must stabilize instead of appending forever",
  );
  const metaWrite = store.writes.find((write) => write.offset < 512)!;
  const meta = parseCommitMeta(metaWrite.bytes, 256, store.size() / 256)!;
  assert.ok(meta.overflowCount > 0);
  assert.ok(meta.freeListHead > 0);
  for (let missing = 0; missing < store.writes.length; missing++) {
    const image = apply(
      prior,
      store.writes.filter((_, i) => i !== missing),
    );
    const loaded = loadEngine(image);
    const out = execute(loaded, "SELECT DISTINCT value FROM t");
    assert.equal(out.kind, "query");
    if (out.kind !== "query") throw new Error("expected query");
    const omitted = store.writes[missing];
    const identical = omitted.bytes.every((byte, i) => prior[omitted.offset + i] === byte);
    const generation = identical ? 99 : 98;
    assert.deepEqual(
      out.rows,
      [[{ kind: "text", text: `${"x".repeat(30)}-${generation}` }]],
      `reused write ${missing} is atomic`,
    );
  }
});

test("unchanged shared meta reuses a successful descriptor validation", () => {
  const { db, store } = setup();
  const rows = Array.from({ length: 160 }, (_, i) => `(${i + 2}, '${"x".repeat(25)}')`);
  execute(db, `INSERT INTO t VALUES ${rows.join(", ")}`);
  const metaWrite = store.writes.findLast((write) => write.offset < 512)!;
  const meta = parseCommitMeta(metaWrite.bytes, 256, store.size() / 256)!;
  store.reads = 0;
  loadEnginePaged(db.paging!);
  const first = store.reads;
  store.reads = 0;
  loadEnginePaged(db.paging!);
  assert.equal(first - store.reads, meta.dirtyCount + meta.overflowCount);
});
