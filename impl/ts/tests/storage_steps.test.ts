import assert from "node:assert/strict";
import { test } from "node:test";
import { BufferPool } from "../src/bufferpool.ts";
import { ioSteps, runAsync, runSync, type Steps } from "../src/execution.ts";
import { loadEnginePaged } from "../src/format.ts";
import { MemoryBlockStore } from "../src/memoryblockstore.ts";
import { Pager } from "../src/pager.ts";
import { SharedPaging } from "../src/paging.ts";
import { PMap, onDiskRef, unboundedBound, type LeafSource, type PNode } from "../src/pmap.ts";
import type { Row, TableStore } from "../src/storage.ts";
import { Engine, execute, toImage } from "../src/tooling.ts";
import { intValue } from "../src/value.ts";
import { fillerText } from "./util.ts";

class DelayedStore extends MemoryBlockStore {
  syncReads: number[] = [];
  asyncReads: number[] = [];
  turns = 0;
  afterRead?: () => void;

  override readAt(offset: number, length: number): Uint8Array {
    this.syncReads.push(offset);
    return super.readAt(offset, length);
  }

  async readAtAsync(offset: number, length: number): Promise<Uint8Array> {
    this.asyncReads.push(offset);
    await new Promise<void>((resolve) =>
      setImmediate(() => {
        this.turns++;
        resolve();
      }),
    );
    this.afterRead?.();
    return super.readAt(offset, length);
  }
}

function paged(image: Uint8Array) {
  const device = new DelayedStore(image);
  const paging = new SharedPaging(Pager.fromStore(device), 1);
  const engine = loadEnginePaged(paging);
  device.syncReads.length = 0;
  const store = engine.committed.stores.values().next().value!;
  return { device, paging, store, engine };
}

function* collect(store: TableStore, reverse = false): Steps<Row[]> {
  const iterator = store.scanRowsStepsIter(unboundedBound(), reverse);
  const rows: Row[] = [];
  try {
    for (;;) {
      const item = yield* iterator.nextSteps();
      if (item.done) return rows;
      rows.push(yield* store.resolveColumnsSteps(item.value, [true, true]));
    }
  } finally {
    if (iterator.returnSteps !== undefined) yield* iterator.returnSteps();
  }
}

test("storage steps: delayed leaf and overflow reads resume once with bounded residency", async () => {
  const engine = new Engine();
  engine.pageSize = 256;
  execute(engine, "CREATE TABLE t (id i32 PRIMARY KEY, body text)");
  execute(engine, "BEGIN");
  const body = fillerText(900);
  for (let i = 0; i < 80; i++) execute(engine, `INSERT INTO t VALUES (${i}, '${body}')`);
  execute(engine, "COMMIT");
  const image = toImage(engine.committed, 256, engine.committed.txid);
  const baseline = paged(image);
  const expected = runSync(collect(baseline.store));
  assert.equal(expected.length, 80);

  const suspended = paged(image);
  const actual = await runAsync(collect(suspended.store));
  assert.deepEqual(actual, expected);
  assert.deepEqual(
    suspended.device.asyncReads,
    baseline.device.syncReads,
    "awaited reads follow the same demand order, without a preload or replay",
  );
  assert.equal(suspended.device.syncReads.length, 0, "the async provider handles every query read");
  assert.equal(suspended.device.turns, suspended.device.asyncReads.length);
  assert.ok(suspended.paging.residentLeaves() <= 1);

  const reversed = paged(image);
  assert.deepEqual(await runAsync(collect(reversed.store, true)), expected.toReversed());

  const early = paged(image);
  const iterator = early.store.scanRowsStepsIter(unboundedBound(), false);
  assert.equal((await runAsync(iterator.nextSteps())).done, false);
  const reads = early.device.asyncReads.length;
  if (iterator.returnSteps !== undefined) await runAsync(iterator.returnSteps());
  assert.equal((await runAsync(iterator.nextSteps())).done, true);
  assert.equal(early.device.asyncReads.length, reads, "early close faults no later leaf");
  assert.ok(reads < suspended.device.asyncReads.length);
});

function leaf(page: number): PNode {
  return { keys: [], vals: [], weights: [], children: [], page };
}

test("storage steps: canceled and failed reads leave existing cache entries intact", async () => {
  const pool = new BufferPool(1);
  const original = leaf(1);
  pool.getOrLoad(1, () => original);
  let canceled = false;
  const pending = pool.getOrLoadSteps(2, () =>
    ioSteps(
      () => assert.fail("expected asynchronous provider"),
      async () => {
        canceled = true;
        return leaf(2);
      },
    ),
  );
  await assert.rejects(
    runAsync(pending, { check: () => canceled }),
    (error: unknown) =>
      typeof error === "object" &&
      error !== null &&
      "code" in error &&
      typeof error.code === "function" &&
      error.code() === "57014",
  );
  assert.equal(
    pool.getOrLoad(1, () => assert.fail("cached entry was evicted")),
    original,
  );

  const failure = new Error("delayed read failed");
  await assert.rejects(
    runAsync(
      pool.getOrLoadSteps(2, () =>
        ioSteps(
          () => assert.fail("expected asynchronous provider"),
          async () => {
            throw failure;
          },
        ),
      ),
    ),
    failure,
  );
  assert.equal(
    pool.getOrLoad(1, () => assert.fail("cached entry was evicted")),
    original,
  );
});

test("storage steps: invalidation during a suspended load prevents stale cache installation", async () => {
  const pool = new BufferPool(1);
  const old = leaf(2);
  const loaded = await runAsync(
    pool.getOrLoadSteps(2, () =>
      ioSteps(
        () => assert.fail("expected asynchronous provider"),
        async () => {
          pool.invalidate(2);
          return old;
        },
      ),
    ),
  );
  assert.equal(loaded, old);
  assert.equal(pool.resident(), 0);
  const fresh = leaf(3);
  assert.equal(
    pool.getOrLoad(2, () => fresh),
    fresh,
  );
});

test("storage steps: resident traversal yields and cancels without restarting rows", async () => {
  const map = new PMap();
  for (let i = 0; i < 1000; i++) {
    const key = new Uint8Array(8);
    new DataView(key.buffer).setBigUint64(0, BigInt(i));
    map.insert(key, [intValue(BigInt(i))], 16, 240, { fixed: 1, var: 0 }, null);
  }
  const iterator = map.scanRowsStepsIter(unboundedBound(), null, false);
  let canceled = false;
  let visited = 0;
  function* consume(): Steps<void> {
    try {
      for (;;) {
        const item = yield* iterator.nextSteps();
        if (item.done) return;
        assert.deepEqual(item.value, [intValue(BigInt(visited++))]);
      }
    } finally {
      if (iterator.returnSteps !== undefined) yield* iterator.returnSteps();
    }
  }
  await assert.rejects(
    runAsync(consume(), {
      quantum: 64,
      check: () => canceled,
      schedule: (resume) => {
        canceled = true;
        setImmediate(resume);
      },
    }),
  );
  assert.equal(visited, 63);
  assert.equal(runSync(iterator.nextSteps()).done, true);
});

function key(value: number): Uint8Array {
  const bytes = new Uint8Array(8);
  new DataView(bytes.buffer).setBigUint64(0, BigInt(value));
  return bytes;
}

function mutationTree(onRead: (page: number) => void = () => {}) {
  const leaves = new Map<number, PNode>();
  for (const [page, first] of [
    [2, 1],
    [3, 4],
  ]) {
    leaves.set(page, {
      keys: [first, first + 1, first + 2].map(key),
      vals: [first, first + 1, first + 2].map((value) => [intValue(BigInt(value))]),
      weights: [16, 16, 16],
      children: [],
      page,
    });
  }
  const root: PNode = {
    keys: [key(4)],
    vals: [],
    weights: [],
    children: [onDiskRef(2), onDiskRef(3)],
    page: 1,
  };
  const source: LeafSource = {
    loadLeaf: (page) => leaves.get(page)!,
    loadLeafSteps: (page) =>
      ioSteps(
        () => leaves.get(page)!,
        async () => {
          await new Promise<void>((resolve) => setImmediate(resolve));
          onRead(page);
          return leaves.get(page)!;
        },
      ),
  };
  return { map: new PMap(root, 6n), source };
}

test("storage steps: mutations retain split/merge results and snapshot ownership across page awaits", async () => {
  const sync = mutationTree();
  const async = mutationTree();
  const original = async.map.clone();
  const shape = { fixed: 1, var: 0 };
  for (let value = 7; value < 40; value++) {
    const row = [intValue(BigInt(value * 2))];
    assert.deepEqual(
      await runAsync(async.map.insertSteps(key(value), row, 16, 240, shape, async.source)),
      sync.map.insert(key(value), row, 16, 240, shape, sync.source),
    );
  }
  for (let value = 1; value < 40; value += 2) {
    assert.deepEqual(
      await runAsync(async.map.removeSteps(key(value), 240, shape, async.source)),
      sync.map.remove(key(value), 240, shape, sync.source),
    );
  }
  assert.deepEqual(async.map.inorder(async.source), sync.map.inorder(sync.source));
  assert.equal(async.map.getCount(), sync.map.getCount());
  assert.equal(async.map.nodeCount(), sync.map.nodeCount());
  assert.equal(original.getCount(), 6n);
  assert.equal(original.get(key(7), async.source), undefined);
  assert.deepEqual(original.get(key(1), async.source), [intValue(1n)]);
});

test("storage steps: clone during a write page await freezes the captured mutation generation", async () => {
  let snapshot: PMap | undefined;
  let capture = false;
  const tree = mutationTree(() => {
    if (capture) snapshot = tree.map.clone();
  });
  const shape = { fixed: 1, var: 0 };
  // Make the root owned by the active generation while its right child remains on disk.
  tree.map.insert(key(0), [intValue(0n)], 16, 240, shape, tree.source);
  capture = true;
  await runAsync(tree.map.insertSteps(key(7), [intValue(7n)], 16, 240, shape, tree.source));
  assert.ok(snapshot);
  assert.notEqual(snapshot.rootNode(), tree.map.rootNode());
  assert.equal(snapshot.get(key(7), tree.source), undefined);
  assert.deepEqual(tree.map.get(key(7), tree.source), [intValue(7n)]);
});

test("storage steps: cancellation while loading a delete sibling publishes no partial tree", async () => {
  const reads: number[] = [];
  let canceled = false;
  const tree = mutationTree((page) => {
    reads.push(page);
    if (reads.length === 2) canceled = true;
  });
  const root = tree.map.rootNode();
  await assert.rejects(
    runAsync(tree.map.removeSteps(key(1), 240, { fixed: 1, var: 0 }, tree.source), {
      check: () => canceled,
    }),
  );
  assert.deepEqual(
    reads,
    [2, 3],
    "delete suspends at its untouched sibling after rebuilding the target",
  );
  assert.equal(tree.map.rootNode(), root);
  assert.equal(tree.map.getCount(), 6n);
  assert.deepEqual(tree.map.get(key(1), tree.source), [intValue(1n)]);
});

test("storage steps: table mutations preserve file bytes and cancel without staging a row", async () => {
  const engine = new Engine();
  engine.pageSize = 256;
  execute(engine, "CREATE TABLE t (id i32 PRIMARY KEY, body text)");
  execute(engine, "BEGIN");
  for (let i = 0; i < 40; i++) execute(engine, `INSERT INTO t VALUES (${i}, 'old')`);
  execute(engine, "COMMIT");
  const image = toImage(engine.committed, 256, engine.committed.txid);
  const sync = paged(image);
  const async = paged(image);
  const entries = sync.store.entriesInKeyOrder();
  for (let i = 0; i < entries.length; i++) {
    const entry = entries[i]!;
    if (i % 3 === 0) {
      assert.equal(
        await runAsync(async.store.removeSteps(entry.key)),
        sync.store.remove(entry.key),
      );
    } else {
      const row: Row = [entry.row[0]!, { kind: "text", text: fillerText(700 + i) }];
      sync.store.replace(entry.key, row);
      await runAsync(async.store.replaceSteps(entry.key, row));
    }
  }
  assert.deepEqual(
    toImage(async.engine.committed, 256, engine.committed.txid),
    toImage(sync.engine.committed, 256, engine.committed.txid),
  );
  assert.equal(async.store.stagedBytes(), sync.store.stagedBytes());

  const canceled = paged(image);
  canceled.store.setNetStaging(true);
  const root = canceled.store.treeRoot();
  let abort = false;
  canceled.device.afterRead = () => {
    abort = true;
  };
  await assert.rejects(
    runAsync(
      canceled.store.replaceSteps(entries[0]!.key, [intValue(0n), { kind: "text", text: "new" }]),
      { check: () => abort },
    ),
  );
  assert.equal(canceled.store.treeRoot(), root);
  assert.equal(canceled.store.stagedBytes(), 0);
  assert.equal(canceled.store.hasStaging(), false);
});
