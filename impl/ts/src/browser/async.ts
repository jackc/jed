// Browser adapter for the shared async runtime. SQL executes in a dedicated worker;
// the application thread only admits requests and consumes bounded typed batches.
import { acquireWorker, AsyncDatabase, RuntimeConnection } from "../async/client.ts";
import { TransportError } from "../async/errors.ts";
import { runtimeLimits, type RuntimeLimits, type RuntimeTransport } from "../async/protocol.ts";

export {
  AsyncDatabase as Database,
  AsyncDatabase,
  AsyncPreparedStatement,
  AsyncRows,
  AsyncSession,
  AsyncTransaction,
} from "../async/client.ts";
export type { OperationOptions, SessionSettings } from "../async/client.ts";
export type { RuntimeLimits } from "../async/protocol.ts";
export { TransportError } from "../async/errors.ts";
export { EngineError } from "../errors.ts";
export { Decimal } from "../decimal.ts";
export * from "../value.ts";
export type { JsParam, Row, RunResult } from "../ergonomic.ts";
export type { Argon2Params, Credential } from "../crypto.ts";
export { IntegrityKind, IntegritySkipReason } from "../integrity_kinds.ts";
export type {
  IntegrityCheckOptions,
  IntegrityFinding,
  IntegrityKindName,
  IntegrityLevel,
  IntegrityReport,
  IntegritySkip,
  IntegritySkipReasonName,
} from "../integrity.ts";
import type { Argon2Params, Credential } from "../crypto.ts";
export type { SessionOptions } from "../snapshot.ts";
export { PrivilegeSet } from "../privileges.ts";
export { splitStatements } from "../split.ts";
export { DEFAULT_FLUSH_INTERVAL_MS } from "../idle_flush.ts";

export type BrowserDatabaseOptions = {
  backend?: "typescript";
  path?: string;
  pageSize?: number;
  synchronous?: "on" | "off";
  /**
   * How long a synchronous "off" batch may stay pending, in milliseconds, before it is flushed
   * (spec/design/validated-cow.md §4.1). 0 turns the time-based flush off; default 1000.
   */
  flushIntervalMs?: number;
  maxStorageBytes?: bigint;
  unicodeData?: Uint8Array;
  timeZoneData?: Uint8Array;
  /**
   * Creates or opens the file encrypted (a passphrase or a 32-byte key; spec/fileformat/encrypted.md).
   * Pages are sealed and opened through WebCrypto in the worker. Needs a pageSize of at least 1024.
   */
  credential?: Credential;
  /** Argon2id parameters a passphrase credential is stretched with at create. */
  kdf?: Argon2Params;
  runtime?: Partial<RuntimeLimits>;
  /** Creates a fresh dedicated worker running startDatabaseWorker. Jed owns its lifetime. */
  workerFactory?: () => Worker;
};

export type BrowserDeserializeOptions = Pick<
  BrowserDatabaseOptions,
  "backend" | "maxStorageBytes" | "unicodeData" | "timeZoneData" | "runtime" | "workerFactory"
>;

export type BrowserOpenOptions = Omit<BrowserDatabaseOptions, "path" | "pageSize"> & {
  readOnly?: boolean;
  cacheBytes?: number;
  workMem?: number;
};

// A worker cannot serve two connections or be restarted after a database closes.
// Weak ownership tracking does not retain workers after application references disappear.
const ownedWorkers = new WeakSet<Worker>();

function browserTransport(worker: Worker): RuntimeTransport {
  return {
    postMessage(request) {
      worker.postMessage(request);
    },
    onMessage(receive) {
      worker.addEventListener("message", (event) => receive(event.data));
    },
    onFailure(fail) {
      worker.addEventListener("error", (event) => {
        event.preventDefault();
        fail(new Error(event.message || "database worker failed"));
      });
      worker.addEventListener("messageerror", () =>
        fail(new Error("database worker reply could not be decoded")),
      );
    },
    terminate() {
      worker.terminate();
    },
  };
}

async function connect(
  mode: "create" | "open" | "deserialize",
  options:
    | BrowserDatabaseOptions
    | (BrowserOpenOptions & { path: string })
    | (BrowserDeserializeOptions & { image: Uint8Array }),
): Promise<AsyncDatabase> {
  if (options.backend !== undefined && options.backend !== "typescript") {
    throw new TransportError("UNSUPPORTED", "this package provides the typescript backend");
  }
  const limits = runtimeLimits(options.runtime);
  const release = acquireWorker(limits);
  let worker: Worker;
  try {
    worker = options.workerFactory
      ? options.workerFactory()
      : new Worker(new URL("./worker.ts", import.meta.url), { type: "module" });
    // A valid Worker can come from another same-origin realm. Check the required
    // transport methods without relying on the calling realm's constructor identity.
    if (
      worker === null ||
      typeof worker !== "object" ||
      typeof worker.postMessage !== "function" ||
      typeof worker.addEventListener !== "function" ||
      typeof worker.terminate !== "function"
    ) {
      throw new TransportError("PROTOCOL", "workerFactory must return a dedicated Worker");
    }
    if (ownedWorkers.has(worker)) {
      throw new TransportError("PROTOCOL", "workerFactory must create a fresh Worker");
    }
    ownedWorkers.add(worker);
  } catch (error) {
    release();
    throw error;
  }
  let connection: RuntimeConnection;
  try {
    connection = new RuntimeConnection(
      browserTransport(worker),
      limits,
      globalThis.crossOriginIsolated && typeof SharedArrayBuffer !== "undefined",
      release,
    );
  } catch (error) {
    try {
      worker.terminate();
    } finally {
      release();
    }
    throw error;
  }
  try {
    const { runtime: _runtime, workerFactory: _workerFactory, ...settings } = options;
    await connection.call("open", { mode, options: settings, limits: connection.limits });
    return new AsyncDatabase(connection);
  } catch (error) {
    try {
      await connection.close();
    } catch {
      /* Preserve the open error. */
    }
    throw error;
  }
}

/** Create a worker-owned database. A path stores it in OPFS; no path creates an in-memory database. */
export function createDatabase(options: BrowserDatabaseOptions = {}): Promise<AsyncDatabase> {
  return connect("create", options);
}

/** Open an existing OPFS file. A handle owns its exclusive access until close resolves. */
export function openDatabase(
  path: string,
  options: BrowserOpenOptions = {},
): Promise<AsyncDatabase> {
  return connect("open", { ...options, path });
}

/**
 * Build an in-memory database from the bytes of a database image (spec/design/api.md §2.8): a jed
 * database file's bytes, or what `serialize` returned. The bytes are copied to the worker and count
 * against `runtime.maxQueuedBytes`. Later commits change only the in-memory copy. A malformed image
 * is `XX001`; an encrypted one is `55000`.
 */
export function deserializeDatabase(
  image: Uint8Array,
  options: BrowserDeserializeOptions = {},
): Promise<AsyncDatabase> {
  return connect("deserialize", { ...options, image });
}
