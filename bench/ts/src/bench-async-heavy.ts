// One reproducible Node run of the platform-neutral heavy async workload suite.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { arch, cpus, platform, release } from "node:os";
import { join, resolve } from "node:path";
import { createDatabase as createWorker } from "../../../impl/ts/src/async/node.ts";
import { runtimeLimits } from "../../../impl/ts/src/async/protocol.ts";
import { createDatabase as createDirect } from "../../../impl/ts/src/file.ts";
import {
  heavyConfigFrom,
  heavyRuntimeFrom,
  observeTimers,
  runHeavySuite,
  summarize,
} from "./async-heavy.ts";

const config = heavyConfigFrom(process.env);
const limits = runtimeLimits(heavyRuntimeFrom(process.env));
function cpuAffinity(): string | null {
  try {
    return (
      /^Cpus_allowed_list:\s*(.+)$/m.exec(readFileSync("/proc/self/status", "utf8"))?.[1] ?? null
    );
  } catch {
    return null;
  }
}
const directory = resolve(process.argv[2] ?? `bench/results/ts-async-heavy-${Date.now()}`);
mkdirSync(directory, { recursive: true });
const directDatabase = createDirect({ pageSize: 8192 });
const direct = directDatabase.session();
const workerDatabase = await createWorker({ pageSize: 8192, runtime: limits });
const worker = await workerDatabase.session();
const idle = () => observeTimers(() => new Promise<void>((resolve) => setTimeout(resolve, 250)));
try {
  const idleBefore = (await idle()).timer;
  const suite = await runHeavySuite({
    direct,
    worker,
    config,
    onProgress: (message) => console.log(message),
  });
  const idleAfter = (await idle()).timer;
  const report = {
    schemaVersion: 1,
    recordedAt: new Date().toISOString(),
    environment: {
      host: "Node",
      node: process.version,
      v8: process.versions.v8,
      platform: platform(),
      release: release(),
      architecture: arch(),
      cpu: cpus()[0]?.model,
      logicalCpus: cpus().length,
      cpuAffinity: cpuAffinity(),
      baseRevision: process.env.JED_BENCH_REVISION ?? "unknown",
      dirtyWorkingTree: process.env.JED_BENCH_DIRTY === "true",
      pageSize: 8192,
      runtimeLimits: limits,
      sharedAtomicCancellationAvailable: typeof SharedArrayBuffer !== "undefined",
      measuredQueriesPassAbortSignal: false,
      storage: "memory",
    },
    idle: { before: idleBefore, after: idleAfter },
    ...suite,
  };
  writeFileSync(join(directory, "results.json"), `${JSON.stringify(report, null, 2)}\n`);
  const number = (value: number) => value.toFixed(2);
  const lines = [
    "# TypeScript heavy async benchmark — Node",
    "",
    `${report.environment.node}; ${report.environment.cpu}; ${platform()} ${arch()}.`,
    `Base revision: ${report.environment.baseRevision}; dirty working tree: ${report.environment.dirtyWorkingTree}.`,
    "",
    `Each case: ${config.warmups} warmups, ${config.samples} latency samples and ${config.responseSamples} separate responsiveness samples per boundary.`,
    "",
    "| Workload | Direct median ms (range) | Worker median ms (range) | Worker/direct | Rows | SQL cost |",
    "| --- | ---: | ---: | ---: | ---: | ---: |",
    ...suite.cases.map((workload) => {
      const { direct: sync, worker: async } = workload.latencyMs;
      return `| ${workload.id} | ${number(sync.p50)} (${number(sync.min)}–${number(sync.max)}) | ${number(async.p50)} (${number(async.min)}–${number(async.max)}) | ${number(async.p50 / sync.p50)}× | ${workload.validated.rows} | ${workload.validated.cost} |`;
    }),
    "",
    "The following are medians of each responsiveness run’s largest overlapping 5 ms timer gap; delayed callbacks after query completion are included.",
    "",
    "| Workload | Direct largest gap ms | Worker largest gap ms | Direct ticks during work | Worker ticks during work |",
    "| --- | ---: | ---: | ---: | ---: |",
    ...suite.cases.map((workload) => {
      const { direct: sync, worker: async } = workload.responsiveness;
      return `| ${workload.id} | ${number(summarize(sync.map((sample) => sample.timer.gaps.max)).p50)} | ${number(summarize(async.map((sample) => sample.timer.gaps.max)).p50)} | ${number(summarize(sync.map((sample) => sample.timer.ticksDuringWork)).p50)} | ${number(summarize(async.map((sample) => sample.timer.ticksDuringWork)).p50)} |`;
    }),
    "",
    `Idle largest gap before/after: ${number(idleBefore.gaps.max)} / ${number(idleAfter.gaps.max)} ms.`,
    "",
    ...suite.methodology.map((note) => `- ${note}`),
    "",
    "Exact SQL, setup, raw timings, fingerprints, metadata, costs and timer callbacks are in results.json.",
    "",
  ];
  writeFileSync(join(directory, "report.md"), lines.join("\n"));
  console.log(lines.join("\n"));
  console.log(`Saved ${directory}`);
} finally {
  direct.close();
  directDatabase.close();
  await worker.close();
  await workerDatabase.close();
}
