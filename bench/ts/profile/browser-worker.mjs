import { RuntimeServer } from "@jed/impl/ts/src/async/server.ts";
import { buildInMemory } from "@jed/impl/ts/src/shared.ts";
import * as stages from "@jed/impl/ts/src/__profile.mjs";
import { profileWorker } from "./shared.mjs";
const turns = new MessageChannel();
let pending;
turns.port1.onmessage = () => { const callback = pending; pending = undefined; callback?.(); };
const receive = profileWorker({ RuntimeServer, open: () => buildInMemory(8192), stages,
  schedule(callback) { pending = callback; turns.port2.postMessage(null); },
  send: (reply, transfer) => postMessage(reply, { transfer }) });
addEventListener("message", (event) => receive(event.data));
