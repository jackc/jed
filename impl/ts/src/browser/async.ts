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
export type { SessionOptions } from "../snapshot.ts";
export { PrivilegeSet } from "../privileges.ts";
export { splitStatements } from "../split.ts";

export type BrowserDatabaseOptions = {
  backend?: "typescript";
  path?: string;
  pageSize?: number;
  synchronous?: "on" | "off";
  maxStorageBytes?: bigint;
  unicodeData?: Uint8Array;
  timeZoneData?: Uint8Array;
  runtime?: Partial<RuntimeLimits>;
};

export type BrowserOpenOptions = Omit<BrowserDatabaseOptions, "path" | "pageSize"> & {
  readOnly?: boolean;
  cacheBytes?: number;
  workMem?: number;
};

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
  mode: "create" | "open",
  options: BrowserDatabaseOptions | (BrowserOpenOptions & { path: string }),
): Promise<AsyncDatabase> {
  if (options.backend !== undefined && options.backend !== "typescript") {
    throw new TransportError("UNSUPPORTED", "this package provides the typescript backend");
  }
  const release = acquireWorker(runtimeLimits(options.runtime));
  let worker: Worker;
  try {
    worker = new Worker(new URL("./worker.ts", import.meta.url), { type: "module" });
  } catch (error) {
    release();
    throw error;
  }
  const connection = new RuntimeConnection(
    browserTransport(worker),
    options.runtime,
    globalThis.crossOriginIsolated && typeof SharedArrayBuffer !== "undefined",
    release,
  );
  try {
    const { runtime: _runtime, ...settings } = options;
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
