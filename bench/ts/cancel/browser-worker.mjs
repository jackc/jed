import { RuntimeServer } from "@jed/impl/ts/src/async/server.ts";
import { buildInMemory } from "@jed/impl/ts/src/shared.ts";
import { createProbeServer } from "./worker-common.mjs";
const turns = new MessageChannel();
let pending;
turns.port1.onmessage = () => {
  const callback = pending;
  pending = undefined;
  callback?.();
};
const receive = createProbeServer({
  RuntimeServer,
  open: () => buildInMemory(8192),
  schedule(callback) {
    pending = callback;
    turns.port2.postMessage(null);
  },
  send: (reply, transfer) => postMessage(reply, { transfer }),
});
addEventListener("message", (event) => receive(event.data));
