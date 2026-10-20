/// <reference lib="webworker" />
// Real-OPFS proof of the asynchronous write side (spec/design/ts-runtime.md §11): the engine runs in
// this dedicated worker over a real FileSystemSyncAccessHandle, with an identity page codec that
// suspends on a genuine WebCrypto call for every page it encodes or decodes. Commit, recovery and
// maintenance must await it and produce the same rows and file bytes as a run without a codec.
import { runAsync, type Steps } from "../../src/execution.ts";
import { closeOpfs, createOpfsWithHandle, openOpfsWithHandleSteps } from "../../src/opfs.ts";
import type { SyncAccessHandle } from "../../src/opfsblockstore.ts";
import type { PageTransform } from "../../src/pager.ts";
import { Database, type Session } from "../../src/shared.ts";
import { render } from "../../src/value.ts";

// Identity bytes, but each page waits on SubtleCrypto: no synchronous form exists.
class WebCryptoIdentity implements PageTransform {
  encoded = 0;
  decoded = 0;
  private async touch(page: Uint8Array): Promise<void> {
    await crypto.subtle.digest("SHA-256", page.slice());
  }
  async encodeAsync(_index: number, page: Uint8Array): Promise<Uint8Array> {
    await this.touch(page);
    this.encoded++;
    return page.slice();
  }
  async decodeAsync(_index: number, stored: Uint8Array): Promise<Uint8Array> {
    await this.touch(stored);
    this.decoded++;
    return stored.slice();
  }
}

const filler = (n: number, seed: number) => {
  let x = seed;
  let out = "";
  for (let i = 0; i < n; i++) {
    x = (x * 1103515245 + 12345) >>> 0;
    out += String.fromCharCode(97 + ((x >>> 16) % 26));
  }
  return out;
};

const WORKLOAD = [
  "CREATE TABLE t (k i32 PRIMARY KEY, v text)",
  "BEGIN",
  ...Array.from({ length: 40 }, (_, i) => `INSERT INTO t VALUES (${i}, '${filler(120 + i, i)}')`),
  "COMMIT",
  ...Array.from({ length: 10 }, (_, i) => `INSERT INTO t VALUES (${100 + i}, '${filler(900, i)}')`),
  "UPDATE t SET v = 'short' WHERE k < 20",
  "DELETE FROM t WHERE k >= 100",
  "DELETE FROM t WHERE k >= 30",
];
const CHECK = "SELECT k, length(v) FROM t ORDER BY k";

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

async function openAsync(
  dir: FileSystemDirectoryHandle,
  name: string,
  codec: PageTransform | null,
) {
  const engine = await runAsync(openOpfsWithHandleSteps(await handle(dir, name, false), {}, codec));
  return Database.fromEngine(engine);
}

async function run(dir: FileSystemDirectoryHandle, name: string, codec: WebCryptoIdentity | null) {
  try {
    await dir.removeEntry(name);
  } catch {
    // absent
  }
  closeOpfs(createOpfsWithHandle(await handle(dir, name, true), { pageSize: 512 }));
  const db = await openAsync(dir, name, codec);
  const session = db.session();
  for (const sql of WORKLOAD) await runAsync(session.executeSteps(sql));
  const rows = await runAsync(rowsOf(session, CHECK));
  session.close();
  await runAsync(db.closeSteps());

  const reader = await handle(dir, name, false);
  const bytes = new Uint8Array(reader.getSize());
  reader.read(bytes, { at: 0 });
  reader.close();

  // Recovery reads the last commit's manifest dependencies through the codec.
  const reopened = await openAsync(dir, name, codec === null ? null : new WebCryptoIdentity());
  const again = reopened.session();
  const reread = await runAsync(rowsOf(again, CHECK));
  again.close();
  await runAsync(reopened.closeSteps());
  return { rows, reread, bytes };
}

addEventListener("message", async () => {
  try {
    const dir = await navigator.storage.getDirectory();
    const control = await run(dir, "async-codec-control.jed", null);
    const codec = new WebCryptoIdentity();
    const actual = await run(dir, "async-codec.jed", codec);
    const sameBytes =
      actual.bytes.length === control.bytes.length &&
      actual.bytes.every((b, i) => b === control.bytes[i]);
    postMessage({
      rows: actual.rows.length,
      sameRows: JSON.stringify(actual.rows) === JSON.stringify(control.rows),
      sameReread: JSON.stringify(actual.reread) === JSON.stringify(control.rows),
      sameBytes,
      encoded: codec.encoded > 0,
      decoded: codec.decoded > 0,
    });
  } catch (error) {
    postMessage({ error: String(error) });
  }
});
