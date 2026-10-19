import { buildInMemory } from "@jed/impl/ts/src/shared.ts";
import { AsyncDatabase, RuntimeConnection } from "@jed/impl/ts/src/async/client.ts";
import * as stages from "@jed/impl/ts/src/__profile.mjs";
import * as heavy from "@jed/bench/ts/src/async-heavy.ts";
import { profileClient, createSuite, LIMITS } from "./shared.mjs";
let suite;
window.prepareProfileSuite = async (settings) => {
  const worker = new Worker(new URL("./browser-worker.mjs", import.meta.url), { type: "module" });
  const core = { buildInMemory, AsyncDatabase, RuntimeConnection, stages };
  const endpoint = profileClient({
    postMessage: (request) => worker.postMessage(request),
    onMessage: (receive) => worker.addEventListener("message", (event) => receive(event.data)),
    onFailure: (fail) => { worker.addEventListener("error", fail); worker.addEventListener("messageerror", fail); },
    terminate: () => worker.terminate(),
  }, core, crossOriginIsolated && typeof SharedArrayBuffer !== "undefined");
  suite = await createSuite(core, endpoint, heavy, settings);
  return { config: suite.config, cases: suite.cases, limits: LIMITS,
    environment: { userAgent: navigator.userAgent, crossOriginIsolated,
      sharedArrayBuffer: typeof SharedArrayBuffer !== "undefined", visibilityState: document.visibilityState } };
};
window.warmProfileCase = (id, warmups) => suite.warm(id, warmups);
window.runProfileCase = (...args) => suite.run(...args);
window.closeProfileSuite = () => suite.close();
