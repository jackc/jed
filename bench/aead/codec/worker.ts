/// <reference lib="webworker" />
// The engine for the integrated codec experiment (README.md). It drives the TS core directly in this
// dedicated worker over a real OPFS FileSystemSyncAccessHandle, through the same async driver and
// MessageChannel task scheduler as the production execution worker (src/browser/worker-runtime.ts),
// without the public transport, so measured differences are the storage stack's. The main thread
// sends commands; a second port echoes pings so the main thread can see when this worker's event
// loop is free to admit a message (the runtime admits cancel/close requests the same way).
import { runAsync, type Steps } from "../../../impl/ts/src/execution.ts";
import { DEFAULT_PAGE_SIZE } from "../../../impl/ts/src/executor.ts";
import {
  closeOpfs,
  createOpfsWithHandle,
  openOpfsWithHandleSteps,
} from "../../../impl/ts/src/opfs.ts";
import type { SyncAccessHandle } from "../../../impl/ts/src/opfsblockstore.ts";
import { Database, type Session } from "../../../impl/ts/src/shared.ts";
import { render } from "../../../impl/ts/src/value.ts";
import { makeCodec, type MeasurementCodec, type VariantName } from "./codecs.ts";

export type Config = {
  singleWarmups: number;
  singleSamples: number;
  tx64Warmups: number;
  tx64Samples: number;
  largeWarmups: number;
  largeSamples: number;
  largeRows: number;
  scanRows: number;
  coldOpens: number;
  warmScansPerOpen: number;
  burstCommits: number;
};

type Sample = {
  ms: number;
  encoded: number;
  decoded: number;
  cryptoMs: number;
  extra?: Record<string, number>;
};

const turns = new MessageChannel();
const turnQueue: (() => void)[] = [];
turns.port1.onmessage = () => turnQueue.shift()?.();
const options = {
  schedule(callback: () => void) {
    turnQueue.push(callback);
    turns.port2.postMessage(null);
  },
};

let dir: FileSystemDirectoryHandle;
let variant: VariantName = "none";
let codec: MeasurementCodec = null;
let fileName = "";
let db: Database | null = null;
let session: Session | null = null;
let largeTables = 0;
let singleKey = 1_000_000;
let txKey = 2_000_000;

const counts = () => ({
  encoded: codec?.encoded ?? 0,
  decoded: codec?.decoded ?? 0,
  cryptoMs: codec?.cryptoMs ?? 0,
});

async function timed(work: () => Promise<unknown>): Promise<Sample> {
  const before = counts();
  const started = performance.now();
  await work();
  const ms = performance.now() - started;
  const after = counts();
  return {
    ms,
    encoded: after.encoded - before.encoded,
    decoded: after.decoded - before.decoded,
    cryptoMs: after.cryptoMs - before.cryptoMs,
  };
}

function* rowsOf(s: Session, sql: string): Steps<string[]> {
  const rows = yield* s.querySteps(sql);
  const out: string[] = [];
  try {
    for (;;) {
      const step = yield* rows.nextSteps();
      if (step.done) return out;
      out.push(step.value.map(render).join("|"));
    }
  } finally {
    rows.close();
  }
}

const exec = (sql: string) => runAsync(session!.executeSteps(sql), options);
const query = (sql: string) => runAsync(rowsOf(session!, sql), options);

async function syncHandle(create: boolean): Promise<SyncAccessHandle> {
  const file = await dir.getFileHandle(fileName, { create });
  return (await file.createSyncAccessHandle()) as unknown as SyncAccessHandle;
}

// open is the measured engine open: Pager header, both metas, the last commit's manifest and every
// dependency it lists (validated COW), catalog and interior skeletons. Handle acquisition is not timed.
async function open(): Promise<Sample> {
  const handle = await syncHandle(false);
  const sample = await timed(async () => {
    const engine = await runAsync(openOpfsWithHandleSteps(handle, {}, codec), options);
    db = Database.fromEngine(engine);
  });
  session = db!.session();
  return sample;
}

async function close(): Promise<void> {
  session?.close();
  session = null;
  if (db !== null) await runAsync(db.closeSteps(), options);
  db = null;
}

async function setup(name: VariantName, config: Config) {
  await close();
  variant = name;
  fileName = `codec-bench-${name}.jed`;
  try {
    await dir.removeEntry(fileName);
  } catch {
    // absent
  }
  // create's bootstrap image is synchronous and unencoded (ts-runtime.md §11.1); every later page
  // crosses the codec.
  closeOpfs(createOpfsWithHandle(await syncHandle(true), { pageSize: DEFAULT_PAGE_SIZE }));
  codec = await makeCodec(name, DEFAULT_PAGE_SIZE);
  await open();
  largeTables = 0;
  singleKey = 1_000_000;
  txKey = 2_000_000;
  await exec("CREATE TABLE t (k i64 PRIMARY KEY, v text)");
  await exec(
    "INSERT INTO t SELECT g, repeat(chr(97 + (g % 26)::i32), 100) FROM generate_series(1, 2000) g",
  );
  await exec("CREATE TABLE s (k i64 PRIMARY KEY, v text)");
  await exec(
    `INSERT INTO s SELECT g, repeat(chr(97 + (g % 26)::i32), 200) FROM generate_series(1, ${config.scanRows}) g`,
  );
  return { variant: name, pageSize: DEFAULT_PAGE_SIZE, writeSideAsync: codec !== null && "encodeAsync" in codec };
}

const singleCommit = () =>
  timed(() => exec(`INSERT INTO t VALUES (${singleKey++}, '${"x".repeat(100)}')`));

async function tx64(): Promise<{ commit: Sample; total: Sample }> {
  const base = txKey;
  txKey += 64;
  const started = performance.now();
  await exec("BEGIN");
  await exec(
    `INSERT INTO t SELECT g, repeat('y', 100) FROM generate_series(${base}, ${base + 63}) g`,
  );
  const commit = await timed(() => exec("COMMIT"));
  return { commit, total: { ...commit, ms: performance.now() - started } };
}

async function large(rows: number): Promise<{ commit: Sample; insertMs: number }> {
  const name = `big_${largeTables++}`;
  await exec(`CREATE TABLE ${name} (k i64 PRIMARY KEY, v text)`);
  await exec("BEGIN");
  const started = performance.now();
  await exec(
    `INSERT INTO ${name} SELECT g, repeat(chr(97 + (g % 26)::i32), 200) FROM generate_series(1, ${rows}) g`,
  );
  const insertMs = performance.now() - started;
  const commit = await timed(() => exec("COMMIT"));
  return { commit, insertMs };
}

const SCAN = "SELECT sum(length(v)) FROM s";
const scan = () => timed(() => query(SCAN));

async function latency(config: Config) {
  const single: Sample[] = [];
  for (let i = 0; i < config.singleWarmups + config.singleSamples; i++) {
    const sample = await singleCommit();
    if (i >= config.singleWarmups) single.push(sample);
  }
  const tx64Commit: Sample[] = [];
  const tx64Total: Sample[] = [];
  for (let i = 0; i < config.tx64Warmups + config.tx64Samples; i++) {
    const { commit, total } = await tx64();
    if (i >= config.tx64Warmups) {
      tx64Commit.push(commit);
      tx64Total.push(total);
    }
  }
  // Large commits come last, so the database's last commit is large when it is reopened below.
  const largeCommit: Sample[] = [];
  const largeInsertMs: number[] = [];
  for (let i = 0; i < config.largeWarmups + config.largeSamples; i++) {
    const { commit, insertMs } = await large(config.largeRows);
    if (i >= config.largeWarmups) {
      largeCommit.push(commit);
      largeInsertMs.push(insertMs);
    }
  }
  const coldOpen: Sample[] = [];
  const coldScan: Sample[] = [];
  const warmScan: Sample[] = [];
  for (let i = 0; i < config.coldOpens; i++) {
    await close();
    coldOpen.push(await open());
    coldScan.push(await scan());
    for (let j = 0; j < config.warmScansPerOpen; j++) warmScan.push(await scan());
  }
  return { single, tx64Commit, tx64Total, largeCommit, largeInsertMs, coldOpen, coldScan, warmScan };
}

// webCryptoChain is the engine-free scheduling probe: a chain of awaited page-sized AES-GCM seals,
// recording how much of each operation's time is spent inside the encrypt() call itself. If that is
// nearly all of it, the browser does the work on this thread and the await yields nothing.
async function webCryptoChain(): Promise<Sample & { extra: Record<string, number> }> {
  const key = await crypto.subtle.importKey("raw", new Uint8Array(32), "AES-GCM", false, ["encrypt"]);
  const page = new Uint8Array(DEFAULT_PAGE_SIZE);
  const operations = 2000;
  let inCallMs = 0;
  const started = performance.now();
  for (let i = 0; i < operations; i++) {
    const iv = new Uint8Array(12);
    new DataView(iv.buffer).setUint32(8, i);
    const before = performance.now();
    const promise = crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, page);
    inCallMs += performance.now() - before;
    await promise;
  }
  const ms = performance.now() - started;
  return {
    ms,
    encoded: 0,
    decoded: 0,
    cryptoMs: 0,
    extra: { operations, inCallMs, inCallFraction: inCallMs / ms },
  };
}

// One responsiveness observation; the main thread pings while it runs.
async function observed(name: string, config: Config): Promise<Sample> {
  switch (name) {
    case "webcrypto-chain":
      return webCryptoChain();
    case "single-burst":
      return timed(async () => {
        for (let i = 0; i < config.burstCommits; i++) await singleCommit();
      });
    case "tx64":
      return (await tx64()).commit;
    case "large":
      return (await large(config.largeRows)).commit;
    case "open":
      return open();
    case "scan-cold":
    case "scan-warm":
      return scan();
    default:
      throw new Error(`unknown operation ${name}`);
  }
}

async function finish() {
  const checks = {
    t: await query("SELECT count(*), sum(k), sum(length(v)) FROM t"),
    s: await query(SCAN),
    big: await query(`SELECT count(*) FROM big_${largeTables - 1}`),
  };
  await close();
  const handle = await syncHandle(false);
  const bytes = new Uint8Array(handle.getSize());
  handle.read(bytes, { at: 0 });
  handle.close();
  const digest = new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));
  await dir.removeEntry(fileName);
  return {
    variant,
    fileBytes: bytes.length,
    fileSha256: Array.from(digest, (b) => b.toString(16).padStart(2, "0")).join(""),
    checks,
    encodedTotal: codec?.encoded ?? 0,
    decodedTotal: codec?.decoded ?? 0,
  };
}

addEventListener("message", async (event: MessageEvent) => {
  const { id, cmd, args } = event.data;
  try {
    let result: unknown;
    switch (cmd) {
      case "init": {
        const port = args.port as MessagePort;
        port.onmessage = (ping) => port.postMessage(ping.data);
        dir = await navigator.storage.getDirectory();
        result = { crossOriginIsolated, hardwareConcurrency: navigator.hardwareConcurrency };
        break;
      }
      case "setup":
        result = await setup(args.variant, args.config);
        break;
      case "latency":
        result = await latency(args.config);
        break;
      case "close":
        await close();
        result = null;
        break;
      case "observed":
        result = await observed(args.name, args.config);
        break;
      case "finish":
        result = await finish();
        break;
      default:
        throw new Error(`unknown command ${cmd}`);
    }
    postMessage({ id, result });
  } catch (error) {
    postMessage({ id, error: String((error as Error)?.stack ?? error) });
  }
});
