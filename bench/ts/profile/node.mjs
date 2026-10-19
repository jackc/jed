import { Worker } from "node:worker_threads";
import { pathToFileURL } from "node:url";
import { mkdirSync, writeFileSync, readFileSync } from "node:fs";
import { resolve, join } from "node:path";
import { gzipSync } from "node:zlib";
import { cpus, arch, platform } from "node:os";
import { profileClient, createSuite, LIMITS } from "./shared.mjs";
import { runCases } from "./runner.mjs";
import * as cpu from "./cpu-node.mjs";
const root = resolve(process.env.JED_PROFILE_ROOT);
const heavy = await import(pathToFileURL(`${root}/bench/ts/src/async-heavy.ts`));
const out = resolve(process.argv[2]);
mkdirSync(out, { recursive: true });
const { buildInMemory } = await import(pathToFileURL(`${root}/impl/ts/src/shared.ts`));
const { AsyncDatabase, RuntimeConnection } = await import(pathToFileURL(`${root}/impl/ts/src/async/client.ts`));
const stages = await import(pathToFileURL(`${root}/impl/ts/src/__profile.mjs`));
const worker = new Worker(new URL("./node-worker.mjs", import.meta.url), { workerData: { root } });
const core = { buildInMemory, AsyncDatabase, RuntimeConnection, stages };
const endpoint = profileClient({
  postMessage: (request) => worker.postMessage(request),
  onMessage: (receive) => worker.on("message", receive),
  onFailure: (fail) => { worker.on("error", fail); worker.on("messageerror", fail); },
  terminate: () => worker.terminate(),
}, core, true);
const suite = await createSuite(core, endpoint, heavy, process.env);
let cpuHandle;
try {
  const result = await runCases({
    metadata: { config: suite.config, cases: suite.cases, limits: LIMITS },
    warm: (...args) => suite.warm(...args), run: (...args) => suite.run(...args),
    async startCpu() { await endpoint.control("cpu.start"); cpuHandle = await cpu.startCpuProfile(); },
    async stopCpu() { const main = await cpu.stopCpuProfile(cpuHandle); const worker = await endpoint.control("cpu.stop"); return { main, worker }; },
    async saveCpu(label, profiles) {
      for (const [thread, profile] of Object.entries(profiles))
        writeFileSync(join(out, `${label}-${thread}.cpuprofile.gz`), gzipSync(JSON.stringify(profile)));
    },
    settings: process.env, progress: console.log,
  });
  writeFileSync(join(out, "results.json"), JSON.stringify({ recordedAt: new Date().toISOString(),
    environment: { host: "Node", node: process.version, v8: process.versions.v8, platform: platform(),
      arch: arch(), cpu: cpus()[0].model, affinity: readFileSync("/proc/self/status", "utf8").match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1],
      root, sourceManifest: JSON.parse(readFileSync(join(root, "profile-manifest.json"), "utf8")),
      signalProvided: false, storage: "memory", pageSize: 8192 }, ...result }, null, 2) + "\n");
} finally { await suite.close(); }
