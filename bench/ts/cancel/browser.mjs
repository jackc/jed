import { AsyncDatabase, RuntimeConnection } from "@jed/impl/ts/src/async/client.ts";
import { heavyConfigFrom, heavyCases, runWorkerQuery } from "@jed/bench/ts/src/async-heavy.ts";
import { controlChannel, FIXED_LIMITS, runCancellationSuite } from "./shared.mjs";
window.runCancellationProbe = async (settings) => {
  if (crossOriginIsolated || typeof SharedArrayBuffer !== "undefined")
    throw new Error("This probe requires ordinary Chromium without shared memory");
  const worker = new Worker(new URL("./browser-worker.mjs", import.meta.url), { type: "module" });
  const control = controlChannel((request) => worker.postMessage(request));
  const connection = new RuntimeConnection(
    {
      postMessage: (request) => control.post(request),
      onMessage(receive) {
        worker.addEventListener("message", (event) => {
          if (!control.receive(event.data)) receive(event.data);
        });
      },
      onFailure(fail) {
        worker.addEventListener("error", (event) => fail(new Error(event.message)));
      },
      terminate: () => worker.terminate(),
    },
    FIXED_LIMITS,
    false,
  );
  let db;
  try {
    await connection.call("open", {
      mode: "create",
      options: { pageSize: 8192 },
      limits: connection.limits,
    });
    db = new AsyncDatabase(connection);
    const result = await runCancellationSuite({
      db,
      control,
      workloads: heavyCases(heavyConfigFrom(settings)),
      validateQuery: runWorkerQuery,
      settings,
    });
    for (const c of result.results)
      for (const sample of c.samples)
        if (sample.worker?.sharedFlagPresent !== false)
          throw new Error("Browser sample unexpectedly has an atomic flag");
    return {
      environment: {
        userAgent: navigator.userAgent,
        crossOriginIsolated,
        sharedArrayBuffer: typeof SharedArrayBuffer !== "undefined",
        visibilityState: document.visibilityState,
        hardwareConcurrency: navigator.hardwareConcurrency,
        pageSize: 8192,
        viewport: { width: innerWidth, height: innerHeight, devicePixelRatio },
      },
      ...result,
    };
  } finally {
    if (db) await db.close();
    else await connection.close();
  }
};
