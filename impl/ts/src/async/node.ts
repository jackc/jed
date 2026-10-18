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
export { PrivilegeSet } from "../privileges.ts";
export type { JsParam, Row, RunResult } from "../ergonomic.ts";
export type { SessionOptions } from "../snapshot.ts";

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
  mode: "create" | "open",
  options: AsyncCreateOptions | (AsyncOpenOptions & { path: string }),
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
