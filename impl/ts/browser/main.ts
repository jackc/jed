// Demo + e2e entry for the Browser/OPFS host (spec/design/hosts.md §5). Loaded by index.html through
// Vite; exposes a tiny scenario API on `window.jed` that the Playwright e2e (e2e/opfs.spec.ts) drives,
// and that a human can poke at by hand (the page renders the last result). Everything here runs on the
// main thread and talks to the engine-in-Worker via the async client (src/browser/client.ts).

import { OpfsDatabase, OpfsError } from "../src/browser/client.ts";
import { EngineError, openDatabase } from "../src/browser/async.ts";
import * as runtime from "./runtime_scenarios.ts";
import * as hostFunctions from "./host_function_scenarios.ts";

const SELECT = "SELECT k, v FROM t ORDER BY k";

// writeScenario creates a fresh OPFS database `name`, builds a table, inserts rows, reads them back, and
// closes (releasing the exclusive handle). Returns the rows it wrote — durably committed under autocommit.
async function writeScenario(name: string): Promise<string[][]> {
  const db = await OpfsDatabase.create(name);
  try {
    await db.execute("CREATE TABLE t (k i32 PRIMARY KEY, v text)");
    await db.execute("INSERT INTO t VALUES (1, 'one')");
    await db.execute("INSERT INTO t VALUES (2, 'two')");
    await db.execute("INSERT INTO t VALUES (3, 'three')");
    return (await db.query(SELECT)).rows;
  } finally {
    await db.close();
  }
}

// readScenario opens an EXISTING OPFS database `name` (a separate handle, after writeScenario closed its
// own) and reads the rows back — proving the commit was durable on real OPFS, across handles and (when
// the page is reloaded between calls) across a full page load.
async function readScenario(name: string): Promise<string[][]> {
  const db = await OpfsDatabase.open(name);
  try {
    return (await db.query(SELECT)).rows;
  } finally {
    await db.close();
  }
}

// errorScenario returns the SQLSTATE the host raises for a given misuse, so the e2e can assert the
// structured-error contract survives the worker boundary (api.md §5/§7). open of an absent file → 58P01.
async function errorScenario(name: string): Promise<string> {
  try {
    const db = await OpfsDatabase.open(name);
    await db.close();
    return "<no error>";
  } catch (e) {
    return e instanceof OpfsError ? e.code : `<${String(e)}>`;
  }
}

// asyncCodecScenario runs the asynchronous write side over real OPFS with a WebCrypto-suspending identity
// page codec in a dedicated worker (spec/design/ts-runtime.md §11), comparing it with a run without one.
async function asyncCodecScenario(): Promise<Record<string, unknown>> {
  const worker = new Worker(new URL("./workers/async_codec_worker.ts", import.meta.url), {
    type: "module",
  });
  try {
    return await new Promise((resolve) => {
      worker.onmessage = (event) => resolve(event.data as Record<string, unknown>);
      worker.postMessage(null);
    });
  } finally {
    worker.terminate();
  }
}

// encryptedScenario runs an encrypted database through the public async API (encryption.md §9): create
// with a host key, write, close; reopen with the key and read; reopen without one (28P02) and with a
// wrong one (28P01); change the credential to a passphrase (compaction and rekey are 0A000 on OPFS),
// after which only the passphrase opens it. The worker seals and opens every page through WebCrypto.
async function encryptedScenario(name: string): Promise<Record<string, unknown>> {
  const credential = { key: new Uint8Array(32).fill(0x4a) };
  const db = await OpfsDatabase.create(name, { credential });
  let written: string[][];
  try {
    await db.execute("CREATE TABLE t (k i32 PRIMARY KEY, v text)");
    await db.execute("INSERT INTO t VALUES (1, 'one')");
    await db.execute("INSERT INTO t VALUES (2, 'two')");
    await db.execute("INSERT INTO t VALUES (3, 'three')");
    written = (await db.query(SELECT)).rows;
  } finally {
    await db.close();
  }
  const reopened = await OpfsDatabase.open(name, { credential });
  let read: string[][];
  try {
    read = (await reopened.query(SELECT)).rows;
  } finally {
    await reopened.close();
  }
  const code = async (key?: Uint8Array): Promise<string> => {
    try {
      const db = await OpfsDatabase.open(name, key === undefined ? {} : { credential: { key } });
      await db.close();
      return "<no error>";
    } catch (e) {
      return e instanceof OpfsError ? e.code : `<${String(e)}>`;
    }
  };
  const missing = await code();
  const wrong = await code(new Uint8Array(32).fill(1));
  const sqlstate = async (operation: () => Promise<void>): Promise<string> => {
    try {
      await operation();
      return "<no error>";
    } catch (e) {
      return e instanceof EngineError ? e.code() : `<${String(e)}>`;
    }
  };
  const passphrase = { passphrase: "changed in the browser" };
  const db2 = await openDatabase(name, { credential });
  let compact: string;
  let rekey: string;
  try {
    compact = await sqlstate(() => db2.compact("main"));
    rekey = await sqlstate(() => db2.rekey("main", null));
    await db2.changeCredential("main", passphrase, {
      memoryKib: 64,
      iterations: 1,
      parallelism: 1,
    });
  } finally {
    await db2.close();
  }
  const changed = await OpfsDatabase.open(name, { credential: passphrase });
  let afterChange: string[][];
  try {
    afterChange = (await changed.query(SELECT)).rows;
  } finally {
    await changed.close();
  }
  return {
    written,
    read,
    missing,
    wrong,
    compact,
    rekey,
    oldKey: await code(credential.key),
    afterChange,
  };
}

// encryptionScenario hands the e2e's request (e2e/encryption_expected.ts) to a dedicated worker that
// checks WebCrypto's bytes against the Node host's and times the per-page cost.
async function encryptionScenario(request: unknown): Promise<Record<string, unknown>> {
  const worker = new Worker(new URL("./workers/encryption_worker.ts", import.meta.url), {
    type: "module",
  });
  try {
    return await new Promise((resolve) => {
      worker.onmessage = (event) => resolve(event.data as Record<string, unknown>);
      worker.postMessage(request);
    });
  } finally {
    worker.terminate();
  }
}

declare global {
  interface Window {
    jed: {
      writeScenario: (name: string) => Promise<string[][]>;
      readScenario: (name: string) => Promise<string[][]>;
      errorScenario: (name: string) => Promise<string>;
      asyncCodecScenario: () => Promise<Record<string, unknown>>;
      encryptedScenario: (name: string) => Promise<Record<string, unknown>>;
      encryptionScenario: (request: unknown) => Promise<Record<string, unknown>>;
    };
    jedRuntime: typeof runtime;
    jedHostFunctions: typeof hostFunctions;
  }
}

window.jed = {
  writeScenario,
  readScenario,
  errorScenario,
  asyncCodecScenario,
  encryptedScenario,
  encryptionScenario,
};
window.jedRuntime = runtime;
window.jedHostFunctions = hostFunctions;

// A by-hand smoke for the demo page: write then read, render the rows. The e2e ignores this and calls
// window.jed.* directly.
const out = document.getElementById("out");
if (out !== null) {
  (async () => {
    try {
      const name = "demo.jed";
      const written = await writeScenario(name);
      const read = await readScenario(name);
      out.textContent = `wrote:\n${JSON.stringify(written)}\n\nread back (new handle):\n${JSON.stringify(read)}`;
    } catch (e) {
      out.textContent = `error: ${String(e)}`;
    }
  })();
}
