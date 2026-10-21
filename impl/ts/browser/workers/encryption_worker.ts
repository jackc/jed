/// <reference lib="webworker" />
// Real-browser proof of the WebCrypto page codec (spec/design/encryption.md §9): in a dedicated
// worker over a real FileSystemSyncAccessHandle, an encrypted database's every seal and open awaits
// Chromium's WebCrypto. It must write the bytes the Node host's node:crypto codec writes — the
// encrypted_* goldens open with the key and read the Node host's rows, and a seeded workload's whole
// file equals the Node file host's byte for byte — and its per-page cost is measured
// against the same database in plaintext (cold full scans after a reopen, and bulk commits).
import { installWebCrypto } from "../../src/crypto_web.ts";
import { Environment } from "../../src/environment.ts";
import { runAsync as runAsyncDefault, type Steps } from "../../src/execution.ts";
import { createOpfsWithHandleSteps, openOpfsWithHandleSteps } from "../../src/opfs.ts";
import type { SyncAccessHandle } from "../../src/opfsblockstore.ts";
import { Database, type Session } from "../../src/shared.ts";
import { render } from "../../src/value.ts";

installWebCrypto();

// Cooperative turns through a MessageChannel, as the production worker schedules them
// (src/browser/worker-runtime.ts), not the clamped default timer — so the timings measure the work.
const turns = new MessageChannel();
let pendingTurn: (() => void) | undefined;
turns.port1.onmessage = () => {
  const callback = pendingTurn;
  pendingTurn = undefined;
  callback?.();
};
function runAsync<T>(steps: Steps<T>): Promise<T> {
  return runAsyncDefault(steps, {
    schedule(callback) {
      pendingTurn = callback;
      turns.port2.postMessage(null);
    },
  });
}

export type EncryptionRequest = {
  seed: string; // bigint, as text
  key: number[]; // the host key's 32 bytes
  pageSize: number;
  goldens: { name: string; bytes: number[]; probe: string; want: string[] }[];
  workload: string[];
  expected: number[]; // the Node file host's bytes for workload
  timing: { rows: number; pageSize: number };
};

function* rowsOf(session: Session, sql: string): Steps<string[]> {
  const rows = yield* session.querySteps(sql);
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

async function handle(dir: FileSystemDirectoryHandle, name: string, create: boolean) {
  const file = await dir.getFileHandle(name, { create });
  return (await file.createSyncAccessHandle()) as unknown as SyncAccessHandle;
}

async function fresh(dir: FileSystemDirectoryHandle, name: string): Promise<SyncAccessHandle> {
  try {
    await dir.removeEntry(name);
  } catch {
    // absent
  }
  return handle(dir, name, true);
}

function same(a: Uint8Array, b: ArrayLike<number>): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}

function fileBytes(h: SyncAccessHandle): Uint8Array {
  const bytes = new Uint8Array(h.getSize());
  h.read(bytes, { at: 0 });
  return bytes;
}

// timed builds the timing table in name (encrypted with key, or plaintext), then reopens it and
// times a cold full scan; returns the milliseconds of the bulk commit and the scan, and the file's
// page count.
async function timed(
  dir: FileSystemDirectoryHandle,
  name: string,
  key: Uint8Array | undefined,
  rows: number,
  pageSize: number,
): Promise<{ write: number; scan: number; pages: number }> {
  const credential = key === undefined ? undefined : { key };
  const engine = await runAsync(
    createOpfsWithHandleSteps(await fresh(dir, name), { pageSize, credential }),
  );
  const db = Database.fromEngine(engine);
  const s = db.session();
  await runAsync(s.executeSteps("CREATE TABLE t (id i64 PRIMARY KEY, v text)"));
  const t0 = performance.now();
  await runAsync(
    s.executeSteps(
      `INSERT INTO t SELECT g, md5(g::text) || md5((g * 7)::text) FROM generate_series(1, ${rows}) g`,
    ),
  );
  const write = performance.now() - t0;
  s.close();
  await runAsync(db.closeSteps());
  const h = await handle(dir, name, false);
  const pages = Math.floor(h.getSize() / pageSize);
  const reopened = Database.fromEngine(
    await runAsync(openOpfsWithHandleSteps(h, { credential, cacheBytes: 1 << 30 })),
  );
  const r = reopened.session();
  const t1 = performance.now();
  await runAsync(rowsOf(r, "SELECT count(*), sum(length(v)) FROM t"));
  const scan = performance.now() - t1;
  r.close();
  await runAsync(reopened.closeSteps());
  return { write, scan, pages };
}

addEventListener("message", async (event: MessageEvent<EncryptionRequest>) => {
  const req = event.data;
  try {
    const seed = BigInt(req.seed);
    const key = new Uint8Array(req.key);
    const credential = { key };
    const env = () => Environment.builder().insecureSeededKeyEntropy(seed).build();

    // The goldens, as files: each opens with the key and reads what the Node host reads.
    const dir = await navigator.storage.getDirectory();
    const goldens: Record<string, boolean> = {};
    for (const g of req.goldens) {
      const h = await fresh(dir, g.name);
      h.write(new Uint8Array(g.bytes), { at: 0 });
      h.flush();
      const gdb = Database.fromEngine(
        await runAsync(openOpfsWithHandleSteps(h, { credential, readOnly: true })),
      );
      const gs = gdb.session();
      const got = await runAsync(rowsOf(gs, g.probe));
      gs.close();
      await runAsync(gdb.closeSteps());
      goldens[g.name] = JSON.stringify(got) === JSON.stringify(g.want);
    }

    // The seeded workload through the OPFS host, every statement an awaited continuation.
    const h = await fresh(dir, "encrypted-parity.jed");
    const engine = await runAsync(
      createOpfsWithHandleSteps(h, { pageSize: req.pageSize, credential, environment: env() }),
    );
    const db = Database.fromEngine(engine);
    const s = db.session();
    for (const sql of req.workload) await runAsync(s.executeSteps(sql));
    s.close();
    await runAsync(db.closeSteps());
    const reader = await handle(dir, "encrypted-parity.jed", false);
    const bytes = fileBytes(reader);
    reader.close();

    // Per-page cost: the same table plaintext and encrypted.
    const plain = await timed(
      dir,
      "timing-plain.jed",
      undefined,
      req.timing.rows,
      req.timing.pageSize,
    );
    const sealed = await timed(dir, "timing-sealed.jed", key, req.timing.rows, req.timing.pageSize);
    postMessage({
      goldens,
      sameBytes: same(bytes, req.expected),
      timing: { plain, sealed },
    });
  } catch (error) {
    postMessage({ error: String(error) });
  }
});
