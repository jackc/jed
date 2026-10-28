// Node entry point: application code never imports the engine or Node storage host.
import { Worker } from "node:worker_threads";
import type { CreateOptions, OpenOptions } from "../file.ts";
import { acquireWorker, AsyncDatabase, RuntimeConnection } from "./client.ts";
import type { RuntimeLimits, RuntimeTransport } from "./protocol.ts";
import { TransportError } from "./errors.ts";

export {
  AsyncDatabase as Database,
  AsyncDatabase,
  AsyncPreparedStatement,
  AsyncRows,
  AsyncSession,
  AsyncTransaction,
} from "./client.ts";
export type { OperationOptions, SessionSettings } from "./client.ts";
export type { RuntimeLimits } from "./protocol.ts";
export { TransportError } from "./errors.ts";
export { EngineError } from "../errors.ts";
export { Decimal } from "../decimal.ts";
export * from "../value.ts";
export { splitStatements } from "../split.ts";
export { DEFAULT_FLUSH_INTERVAL_MS } from "../idle_flush.ts";
export { PrivilegeSet } from "../privileges.ts";
export type { JsParam, Row, RunResult } from "../ergonomic.ts";
export type { SessionOptions } from "../snapshot.ts";
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

type WorkerOptions = {
  backend?: "typescript";
  runtime?: Partial<RuntimeLimits>;
  unicodeData?: Uint8Array;
  timeZoneData?: Uint8Array;
  /** Trusted host module URL exporting configure(EnvironmentBuilder); runs only in the worker. */
  extensionModule?: string;
};
export type AsyncCreateOptions = Omit<CreateOptions, "environment"> & WorkerOptions;
export type AsyncOpenOptions = Omit<OpenOptions, "environment"> & WorkerOptions;
export type AsyncDeserializeOptions = { maxStorageBytes?: bigint } & WorkerOptions;

function nodeTransport(worker: Worker): RuntimeTransport {
  return {
    postMessage: (request) => worker.postMessage(request),
    onMessage: (receive) => {
      worker.on("message", receive);
    },
    onFailure: (fail) => {
      worker.on("error", fail);
      worker.on("messageerror", fail);
      worker.on("exit", (code) => fail(new Error(`database execution worker exited (${code})`)));
    },
    terminate: async () => {
      await worker.terminate();
    },
  };
}

async function connect(
  mode: "create" | "open" | "remote" | "deserialize",
  options:
    | AsyncCreateOptions
    | (AsyncDeserializeOptions & { image: Uint8Array })
    | (AsyncOpenOptions & { path: string })
    | (AsyncOpenOptions & { command: string[]; name: string }),
): Promise<AsyncDatabase> {
  if (options.backend !== undefined && options.backend !== "typescript") {
    throw new TransportError("UNSUPPORTED", "this package provides the typescript backend");
  }
  const release = acquireWorker(options.runtime);
  let worker: Worker;
  try {
    worker = new Worker(new URL("./node-worker.ts", import.meta.url), {
      // Let Node inherit only worker-compatible flags normally. An eval/STDIN parent's
      // --input-type cannot apply to this file; the worker needs no custom loader.
      execArgv: process.execArgv.some((arg) => arg.startsWith("--input-type")) ? [] : undefined,
    });
  } catch (error) {
    release();
    throw error;
  }
  const connection = new RuntimeConnection(nodeTransport(worker), options.runtime, true, release);
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

export function createDatabase(options: AsyncCreateOptions = {}): Promise<AsyncDatabase> {
  return connect("create", options);
}
export function openDatabase(path: string, options: AsyncOpenOptions = {}): Promise<AsyncDatabase> {
  return connect("open", { ...options, path });
}

/**
 * Build an in-memory database from the bytes of a database image (spec/design/api.md §2.8): a jed
 * database file's bytes, or what `serialize` returned. The bytes are copied to the execution worker
 * and count against `runtime.maxQueuedBytes`. Later commits change only the in-memory copy. A
 * malformed image is `XX001`; an encrypted one is `55000`.
 */
export function deserializeDatabase(
  image: Uint8Array,
  options: AsyncDeserializeOptions = {},
): Promise<AsyncDatabase> {
  return connect("deserialize", { ...options, image });
}

/**
 * Open a database another machine (or process) serves over a stream (spec/pageservice/README.md
 * §5.3): a page client whose connection is the stdin and stdout of `command`, run once per
 * connection — `["ssh", "--", "db1", "jed pages '/srv/app.jed'"]` reaches `/srv/app.jed` on `db1`.
 * `name` identifies the database in this process (one handle per name, like a path). The handle runs
 * its own engine over pages fetched through the command, commits through it, reconnects by running
 * the command again, and never owns the database. `credential` unlocks an encrypted database and is
 * sent to the serving end, which needs it only when it must open the file itself. An open that
 * reaches nothing fails `08006` with the command's standard error. Unix only.
 */
export function openRemoteDatabase(
  command: string[],
  name: string,
  options: AsyncOpenOptions = {},
): Promise<AsyncDatabase> {
  return connect("remote", { ...options, command: [...command], name });
}
