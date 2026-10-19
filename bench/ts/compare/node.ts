// Load one immutable engine snapshot. Runtime compilation, warmup and setup stay untimed.
import { resolve } from "node:path";
import { pathToFileURL } from "node:url";
import { observeTimers } from "../src/async-heavy.ts";
import {
  assertSourcesUnchanged,
  environmentMetadata,
  sourceManifest,
  writeCompactResult,
} from "./metadata.mjs";
import {
  type ComparisonDriver,
  comparisonConfigFrom,
  directDriver,
  runComparison,
  workerDriver,
} from "./suite.ts";

const [rootArg, mode, outputArg, revision, ...extra] = process.argv.slice(2);
if (!rootArg || !outputArg || !revision || extra.length || !["direct", "worker"].includes(mode))
  throw new Error("Usage: node node.ts ENGINE_ROOT direct|worker OUTPUT REVISION");
const root = resolve(rootArg);
const output = resolve(outputArg);
const before = sourceManifest(root);
const entry = mode === "direct" ? "impl/ts/src/file.ts" : "impl/ts/src/async/node.ts";
const engine = await import(pathToFileURL(resolve(root, entry)).href);
const limits =
  mode === "worker"
    ? (
        await import(pathToFileURL(resolve(root, "impl/ts/src/async/protocol.ts")).href)
      ).runtimeLimits()
    : null;
const config = comparisonConfigFrom(process.env);
const startupStart = performance.now();
const database = await engine.createDatabase({ pageSize: 8192 });
const databaseMs = performance.now() - startupStart;
const sessionStart = performance.now();
const session = await database.session();
const sessionMs = performance.now() - sessionStart;
const driver: ComparisonDriver = mode === "direct" ? directDriver(session) : workerDriver(session);
const idle = () =>
  observeTimers(() => new Promise<void>((resolveIdle) => setTimeout(resolveIdle, 250)));
try {
  const idleBefore = (await idle()).timer;
  const suite = await runComparison({
    driver,
    config,
    onProgress: (message) => console.log(message),
  });
  const idleAfter = (await idle()).timer;
  assertSourcesUnchanged(root, before);
  writeCompactResult(output, {
    schemaVersion: 1,
    recordedAt: new Date().toISOString(),
    revision,
    mode,
    host: "node",
    sourceManifest: before,
    environment: {
      ...environmentMetadata(),
      sharedAtomicCancellationAvailable: typeof SharedArrayBuffer !== "undefined",
      runtimeLimits: limits,
    },
    startup: { databaseMs, sessionMs },
    idle: { before: idleBefore, after: idleAfter },
    suite,
  });
  for (const result of suite.heavyCases)
    console.log(`${result.id}: ${result.latencyMs.p50.toFixed(2)} ms median (${mode})`);
  for (const result of suite.cheapCases)
    console.log(`${result.id}: ${result.latencyUs.p50.toFixed(2)} us/call median (${mode})`);
  console.log(`Saved ${output}/results.json.gz`);
} finally {
  await session.close();
  await database.close();
}
