// Demo + e2e entry for the Browser/OPFS host (spec/design/hosts.md §5). Loaded by index.html through
// Vite; exposes a tiny scenario API on `window.jed` that the Playwright e2e (e2e/opfs.spec.ts) drives,
// and that a human can poke at by hand (the page renders the last result). Everything here runs on the
// main thread and talks to the engine-in-Worker via the async client (src/browser/client.ts).

import { OpfsDatabase, OpfsError } from "../src/browser/client.ts";

const SELECT = "SELECT k, v FROM t ORDER BY k";

// writeScenario creates a fresh OPFS database `name`, builds a table, inserts rows, reads them back, and
// closes (releasing the exclusive handle). Returns the rows it wrote — durably committed under autocommit.
async function writeScenario(name: string): Promise<string[][]> {
  const db = await OpfsDatabase.create(name);
  try {
    await db.execute("CREATE TABLE t (k int32 PRIMARY KEY, v text)");
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

declare global {
  interface Window {
    jed: {
      writeScenario: (name: string) => Promise<string[][]>;
      readScenario: (name: string) => Promise<string[][]>;
      errorScenario: (name: string) => Promise<string>;
    };
  }
}

window.jed = { writeScenario, readScenario, errorScenario };

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
