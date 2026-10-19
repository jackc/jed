/// <reference lib="webworker" />
// Public bootstrap for application-bundled database workers. Importing this module
// has no worker-global effects, so registration/value helpers can also be used by Node.
import { RuntimeServer } from "../async/server.ts";
import { Environment, type EnvironmentBuilder } from "../environment.ts";
import { engineError } from "../errors.ts";
import { DEFAULT_PAGE_SIZE } from "../executor.ts";
import { createOpfs, openOpfs } from "../opfs.ts";
import { buildInMemory, Database } from "../shared.ts";
import type { BrowserDatabaseOptions, BrowserOpenOptions } from "./async.ts";

export type { EnvironmentBuilder } from "../environment.ts";
export type { HostFunctionSpec, HostKernel, HostBatchKernel, Volatility } from "../extension.ts";
export { EngineError } from "../errors.ts";
export { Decimal } from "../decimal.ts";
export * from "../value.ts";

export type BrowserWorkerOptions = {
  /** Runs once during open, before building the environment or acquiring storage. */
  configure?: (builder: EnvironmentBuilder) => void | Promise<void>;
};

let started = false;

/** Call synchronously at module startup in a fresh dedicated worker; configure may await. */
export function startDatabaseWorker(options: BrowserWorkerOptions = {}): void {
  if (
    typeof DedicatedWorkerGlobalScope === "undefined" ||
    !(globalThis instanceof DedicatedWorkerGlobalScope)
  ) {
    throw new Error("startDatabaseWorker must run in a dedicated worker");
  }
  if (started) throw new Error("database worker already started");
  const configure = options.configure;
  if (configure !== undefined && typeof configure !== "function") {
    throw new TypeError("configure must be a function");
  }
  started = true;

  // One channel belongs to the worker's lifetime. Message tasks yield to control
  // requests without the nested-timer clamp that setTimeout adds to every batch.
  const turns = new MessageChannel();
  let pendingTurn: (() => void) | undefined;
  turns.port1.onmessage = () => {
    const callback = pendingTurn;
    pendingTurn = undefined;
    callback?.();
  };

  const runtime = new RuntimeServer({
    schedule(callback) {
      // RuntimeServer keeps at most one dispatch task queued.
      pendingTurn = callback;
      turns.port2.postMessage(null);
    },
    async open(args) {
      const settings = args.options as Omit<
        BrowserDatabaseOptions & BrowserOpenOptions,
        "workerFactory" | "runtime"
      >;
      if (settings.backend !== undefined && settings.backend !== "typescript") {
        throw engineError(
          "feature_not_supported",
          "the browser runtime supports the typescript backend",
        );
      }
      const builder = Environment.builder();
      if (settings.unicodeData !== undefined) builder.loadUnicodeData(settings.unicodeData);
      if (settings.timeZoneData !== undefined) builder.loadTimeZoneData(settings.timeZoneData);
      if (configure) await configure(builder);
      const environment = builder.build();
      if (settings.path === undefined) {
        if (args.mode !== "create")
          throw engineError("undefined_file", "opening OPFS requires a path");
        return buildInMemory(
          settings.pageSize ?? DEFAULT_PAGE_SIZE,
          environment,
          settings.maxStorageBytes,
        );
      }
      const engine =
        args.mode === "create"
          ? await createOpfs(settings.path, { ...settings, environment })
          : await openOpfs(settings.path, { ...settings, environment });
      if (settings.maxStorageBytes !== undefined)
        engine.storageBudget.limit = settings.maxStorageBytes;
      return Database.fromEngine(engine);
    },
    send(reply, transfer) {
      postMessage(reply, { transfer });
    },
  });

  // Register immediately. Async configuration runs inside open so early requests
  // use the normal readiness/error handshake and never race listener installation.
  addEventListener("message", (event) => runtime.receive(event.data));
}
