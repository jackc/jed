import { Worker } from "node:worker_threads";
import { pathToFileURL } from "node:url";
import { resolve } from "node:path";
import { mkdirSync, writeFileSync } from "node:fs";
import { controlChannel, FIXED_LIMITS, runCancellationSuite } from "./shared.mjs";
import { assertSourcesUnchanged, environmentMetadata, sourceHashes } from "./metadata.mjs";
const root = resolve(process.env.JED_PROBE_ROOT ?? process.cwd());
const output = resolve(process.argv[2]);
const hashes = sourceHashes(root);
const timeoutMs = Number(process.env.JED_CANCEL_RUN_TIMEOUT_MS ?? 600000);
if (!Number.isSafeInteger(timeoutMs) || timeoutMs <= 0) throw new Error("invalid run timeout");
const { AsyncDatabase, RuntimeConnection } = await import(
  pathToFileURL(`${root}/impl/ts/src/async/client.ts`)
);
const { heavyConfigFrom, heavyCases, runWorkerQuery } = await import(
  pathToFileURL(`${root}/bench/ts/src/async-heavy.ts`)
);
const worker = new Worker(new URL("./node-worker.mjs", import.meta.url), { workerData: { root } });
const control = controlChannel((request) => worker.postMessage(request));
const connection = new RuntimeConnection(
  {
    postMessage: (request) => control.post(request),
    onMessage(receive) {
      worker.on("message", (reply) => {
        if (!control.receive(reply)) receive(reply);
      });
    },
    onFailure(fail) {
      worker.on("error", fail);
      worker.on("messageerror", fail);
      worker.on("exit", (code) => fail(new Error(`Cancellation probe worker exited (${code})`)));
    },
    terminate: () => worker.terminate(),
  },
  FIXED_LIMITS,
  true,
);
let db;
const watchdog = setTimeout(() => {
  void worker.terminate();
}, timeoutMs);
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
    workloads: heavyCases(heavyConfigFrom(process.env)),
    validateQuery: runWorkerQuery,
    settings: process.env,
  });
  for (const c of result.results)
    for (const sample of c.samples)
      if (sample.worker?.sharedFlagPresent !== true)
        throw new Error("Node sample lacks atomic cancellation flag");
  assertSourcesUnchanged(root, hashes);
  mkdirSync(resolve(output, ".."), { recursive: true });
  writeFileSync(
    output,
    JSON.stringify(
      {
        recordedAt: new Date().toISOString(),
        metadata: {
          ...environmentMetadata(root),
          ...hashes,
        },
        environment: {
          root,
          host: "Node",
          node: process.version,
          sharedAtomicCancellation: true,
        },
        ...result,
      },
      null,
      2,
    ) + "\n",
  );
  console.log(output);
} finally {
  clearTimeout(watchdog);
  if (db) await db.close();
  else await connection.close();
}
