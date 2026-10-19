import { parentPort, workerData } from "node:worker_threads";
import { pathToFileURL } from "node:url";
import { profileWorker } from "./shared.mjs";
import * as cpu from "./cpu-node.mjs";
const root = workerData.root;
const { RuntimeServer } = await import(pathToFileURL(`${root}/impl/ts/src/async/server.ts`));
const { buildInMemory } = await import(pathToFileURL(`${root}/impl/ts/src/shared.ts`));
const stages = await import(pathToFileURL(`${root}/impl/ts/src/__profile.mjs`));
const receive = profileWorker({ RuntimeServer, open: () => buildInMemory(8192),
  schedule: setImmediate, stages, cpu,
  send: (reply, transfer) => parentPort.postMessage(reply, transfer) });
parentPort.on("message", receive);
