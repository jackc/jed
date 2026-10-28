// Node-only execution adapter. All engine and synchronous storage work runs in this worker;
// the application thread owns only typed transport proxies.
import { parentPort } from "node:worker_threads";
import { Database } from "../shared.ts";
import {
  createDatabase,
  type CreateOptions,
  openDatabase,
  type OpenOptions,
  openRemoteDatabase,
} from "../file.ts";
import { TransportError } from "./errors.ts";
import { Environment } from "../environment.ts";
import type { RuntimeRequest } from "./protocol.ts";
import { RuntimeServer } from "./server.ts";

const port = parentPort;
if (port === null) throw new Error("jed execution worker requires a parent message port");

const server = new RuntimeServer({
  schedule: (run) => {
    setImmediate(run);
  },
  async open(args) {
    const options = (args.options ?? {}) as Record<string, unknown>;
    if (options.environment !== undefined) {
      throw new TransportError(
        "UNSUPPORTED",
        "host environments must be registered inside the execution worker",
      );
    }
    if (options.backend !== undefined && options.backend !== "typescript") {
      throw new TransportError("UNSUPPORTED", "this package provides the typescript backend");
    }
    const builder = Environment.builder();
    if (options.unicodeData !== undefined)
      builder.loadUnicodeData(options.unicodeData as Uint8Array);
    if (options.timeZoneData !== undefined)
      builder.loadTimeZoneData(options.timeZoneData as Uint8Array);
    if (options.extensionModule !== undefined) {
      const url = new URL(options.extensionModule as string);
      if (url.protocol !== "file:")
        throw new TransportError("UNSUPPORTED", "extensionModule must be a trusted file URL");
      const extension = await import(url.href);
      if (typeof extension.configure !== "function")
        throw new TransportError("PROTOCOL", "extension module must export configure(builder)");
      await extension.configure(builder);
    }
    const configured = { ...options, environment: builder.build() };
    if (args.mode === "create") return createDatabase(configured as CreateOptions);
    if (args.mode === "deserialize") {
      const { image, maxStorageBytes, environment } = configured as Record<string, unknown>;
      if (!(image instanceof Uint8Array)) {
        throw new TransportError("PROTOCOL", "deserializing a database requires image bytes");
      }
      return Database.deserialize(image, {
        environment: environment as Environment,
        maxStorageBytes: maxStorageBytes as bigint | undefined,
      });
    }
    if (args.mode === "remote") {
      const { command, name, ...rest } = configured as Record<string, unknown>;
      if (
        !Array.isArray(command) ||
        !command.every((part) => typeof part === "string") ||
        typeof name !== "string"
      ) {
        throw new TransportError(
          "PROTOCOL",
          "opening a remote database requires a command and a name",
        );
      }
      return openRemoteDatabase(command as string[], name, rest as OpenOptions);
    }
    if (args.mode !== "open" || typeof options.path !== "string") {
      throw new TransportError("PROTOCOL", "opening a database requires a file path");
    }
    return openDatabase(options.path, configured as OpenOptions);
  },
  send(reply, transfer) {
    port.postMessage(reply, transfer);
  },
});

port.on("message", (request: RuntimeRequest) => server.receive(request));
