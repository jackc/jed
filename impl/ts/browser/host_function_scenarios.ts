/// <reference types="vite/client" />
// Application code exercises the package exports and Vite's normal worker bundling pattern.
import { createDatabase, openDatabase, TransportError } from "jed-ts/browser";
import customWorkerUrl from "./workers/custom_worker.ts?worker&url";

function customWorker(): Worker {
  return new Worker(new URL("./workers/custom_worker.ts", import.meta.url), { type: "module" });
}

function trackTermination(worker: Worker): () => number {
  const terminate = worker.terminate.bind(worker);
  let calls = 0;
  worker.terminate = () => {
    calls++;
    terminate();
  };
  return () => calls;
}

export async function isolatedFunctionsScenario() {
  const a = await createDatabase({ workerFactory: customWorker });
  const b = await createDatabase({ workerFactory: customWorker });
  try {
    const exact = await a.get("SELECT app_add($1::i64) AS n", 9007199254740993n);
    const first = await a.get("SELECT app_next() AS n");
    const second = await a.get("SELECT app_next() AS n");
    const independent = await b.get("SELECT app_next() AS n");
    const strict = await a.get("SELECT app_add(NULL::i64) AS n");
    return {
      exact: String(exact?.n),
      counters: [first?.n, second?.n, independent?.n].map(String),
      strict: strict?.n,
    };
  } finally {
    await a.close();
    await b.close();
  }
}

export async function persistentFunctionsScenario(name: string) {
  const db = await createDatabase({ path: name, workerFactory: customWorker });
  try {
    await db.execute("CREATE TABLE t (n i64)");
    await db.execute("INSERT INTO t VALUES (app_add(1::i64))");
  } finally {
    await db.close();
  }
  const reopened = await openDatabase(name, { workerFactory: customWorker });
  try {
    const result = await reopened.get("SELECT n, app_add(n) AS added FROM t");
    return { persisted: String(result?.n), registeredAgain: String(result?.added) };
  } finally {
    await reopened.close();
  }
}

export async function failedConfigurationScenario(name: string) {
  const seed = await createDatabase({ path: name });
  try {
    await seed.execute("CREATE TABLE t (n i64)");
    await seed.execute("INSERT INTO t VALUES (7)");
  } finally {
    await seed.close();
  }
  let terminations = () => 0;
  let code = "";
  let message = "";
  try {
    const unexpected = await createDatabase({
      path: name,
      runtime: { maxWorkers: 1 },
      workerFactory() {
        const worker = new Worker(new URL("./workers/failing_worker.ts", import.meta.url), {
          type: "module",
        });
        terminations = trackTermination(worker);
        return worker;
      },
    });
    await unexpected.close();
    throw new Error("failed configuration unexpectedly opened a database");
  } catch (error) {
    if (!(error instanceof TransportError)) throw error;
    code = error.code;
    message = error.message;
  }
  // Failed create must not truncate the existing file or leak its worker quota/OPFS handle.
  const recovered = await openDatabase(name, {
    workerFactory: customWorker,
    runtime: { maxWorkers: 1 },
  });
  try {
    const result = await recovered.get("SELECT app_add(n) AS n FROM t");
    return { code, message, terminations: terminations(), recovered: String(result?.n) };
  } finally {
    await recovered.close();
  }
}

export async function workerOwnershipScenario() {
  const factoryError = new Error("worker construction failed");
  let preservedError = false;
  try {
    await createDatabase({
      runtime: { maxWorkers: 1 },
      workerFactory() {
        throw factoryError;
      },
    });
  } catch (error) {
    preservedError = error === factoryError;
  }
  const worker = customWorker();
  const terminations = trackTermination(worker);
  // This limit also proves that the throwing factory released its reservation.
  const a = await createDatabase({ workerFactory: () => worker, runtime: { maxWorkers: 1 } });
  let reused = "";
  try {
    try {
      await createDatabase({ workerFactory: () => worker, runtime: { maxWorkers: 2 } });
    } catch (error) {
      if (!(error instanceof TransportError)) throw error;
      reused = error.code;
    }
    // Rejection must preserve the first owner's connection and release the failed reservation.
    const b = await createDatabase({
      workerFactory: customWorker,
      runtime: { maxWorkers: 2 },
    });
    try {
      const alive = await a.get("SELECT app_next() AS n");
      await Promise.all([a.close(), a.close()]);
      let reusedClosed = "";
      try {
        await createDatabase({ workerFactory: () => worker, runtime: { maxWorkers: 2 } });
      } catch (error) {
        if (!(error instanceof TransportError)) throw error;
        reusedClosed = error.code;
      }
      return {
        preservedError,
        reused,
        reusedClosed,
        alive: String(alive?.n),
        terminations: terminations(),
      };
    } finally {
      await b.close();
    }
  } finally {
    await a.close();
  }
}

export async function crossRealmWorkerScenario() {
  const frame = document.createElement("iframe");
  frame.hidden = true;
  document.body.append(frame);
  const frameWindow = frame.contentWindow as Window & typeof globalThis;
  const worker = new frameWindow.Worker(new URL(customWorkerUrl, location.href), {
    type: "module",
  });
  let db: Awaited<ReturnType<typeof createDatabase>> | undefined;
  try {
    db = await createDatabase({ workerFactory: () => worker });
    const result = await db.get("SELECT app_add(1::i64) AS n");
    return { foreignRealm: !(worker instanceof Worker), value: String(result?.n) };
  } finally {
    if (db !== undefined) await db.close();
    else worker.terminate();
    frame.remove();
  }
}
