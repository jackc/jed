/// <reference lib="webworker" />
// The platform edge owns OPFS acquisition; scheduling, sessions, typed values, and
// lifecycle are shared with Node. This import graph contains no Node modules.
import { RuntimeServer } from "../async/server.ts";
import { Environment } from "../environment.ts";
import { engineError } from "../errors.ts";
import { DEFAULT_PAGE_SIZE } from "../executor.ts";
import { createOpfs, openOpfs } from "../opfs.ts";
import { buildInMemory, Database } from "../shared.ts";
import type { BrowserDatabaseOptions, BrowserOpenOptions } from "./async.ts";

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
    const options = args.options as BrowserDatabaseOptions & BrowserOpenOptions;
    if (options.backend !== undefined && options.backend !== "typescript") {
      throw engineError(
        "feature_not_supported",
        "the browser runtime supports the typescript backend",
      );
    }
    const builder = Environment.builder();
    if (options.unicodeData !== undefined) builder.loadUnicodeData(options.unicodeData);
    if (options.timeZoneData !== undefined) builder.loadTimeZoneData(options.timeZoneData);
    const environment = builder.build();
    if (options.path === undefined) {
      if (args.mode !== "create")
        throw engineError("undefined_file", "opening OPFS requires a path");
      return buildInMemory(
        options.pageSize ?? DEFAULT_PAGE_SIZE,
        environment,
        options.maxStorageBytes,
      );
    }
    const engine =
      args.mode === "create"
        ? await createOpfs(options.path, { ...options, environment })
        : await openOpfs(options.path, { ...options, environment });
    if (options.maxStorageBytes !== undefined) engine.storageBudget.limit = options.maxStorageBytes;
    return Database.fromEngine(engine);
  },
  send(reply, transfer) {
    postMessage(reply, { transfer });
  },
});

addEventListener("message", (event) => runtime.receive(event.data));
