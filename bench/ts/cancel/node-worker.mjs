import { parentPort, workerData } from "node:worker_threads";
import { pathToFileURL } from "node:url";
import { createProbeServer } from "./worker-common.mjs";
const root = workerData.root;
const { RuntimeServer } = await import(pathToFileURL(`${root}/impl/ts/src/async/server.ts`));
const { createDatabase } = await import(pathToFileURL(`${root}/impl/ts/src/file.ts`));
const receive = createProbeServer({
  RuntimeServer,
  open: () => createDatabase({ pageSize: 8192 }),
  schedule: (callback) => setImmediate(callback),
  send: (reply, transfer) => parentPort.postMessage(reply, transfer),
});
parentPort.on("message", receive);
