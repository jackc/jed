// Offline only. Usage: mise exec -- node bench/ts/profile/analyze.mjs --out OUTPUT RESULT_DIR...
// --wall-only skips CPU profile decompression/source mapping for a lightweight ledger pass.
import { createHash } from "node:crypto";
import { mkdir, readFile, readdir, writeFile } from "node:fs/promises";
import { basename, join, resolve } from "node:path";
import { isDeepStrictEqual, promisify } from "node:util";
import { gzip, gunzip } from "node:zlib";
import { summarizeCpuProfile } from "./cpu-summary.mjs";
import { loadSourceMaps } from "./source-maps.mjs";

const compress = promisify(gzip);
const decompress = promisify(gunzip);
const digest = (value) => createHash("sha256").update(value).digest("hex");
const sum = (values) => values.reduce((total, value) => total + value, 0);
const methodology = [
  "Wall ledgers are constructed per complete sequential drain, then arithmetic means are computed. Mean components add to mean total; component medians generally do not add to median total.",
  "Worker service is the sum of matched worker receipt-to-reply durations. Main RPC time is the sum of caller send-to-receipt durations. Their difference is a delivery/dispatch residual, not a measured message-queue or serialization stage.",
  "Only durations are compared across isolates; no timestamps from different clocks are subtracted. Browser timer precision and instrumentation can produce negative residuals; every signed result is retained.",
  "schedule_wait and io_wait are subtracted from worker service. io_wait includes the async provider's synchronous invocation prefix. GC and instrumentation inside intervals remain included, not separate additive stages.",
  "Detailed encode_measure is nested within encode_append. Encode writing is append minus measure plus finish; worker remainder subtracts append and finish once. Coarse worker remainder includes execution, encoding and runtime work together.",
  "Main remainder is total minus main RPC durations and packed decode validation/construction. It includes consumer work, Promise/iterator/runtime work and other untimed main-thread activity, not a pure consumer measurement.",
  "RPC IDs/op and zero worker pending are checked. Main pending is not persisted by the current harness; profileClient.stop asserts it before producing results. Exact transferred row and packet-byte totals come from worker records, not estimates.",
  "Plain/CPU samples have no stage collection and receive only an unpartitioned wall ledger. Direct samples remain direct execution plus consumption, without inferring worker stage costs.",
  "Stage instrumentation changes performance, especially detailed per-row timers. Compare plain/coarse/detailed runs separately to assess observer overhead; do not treat their differences as isolated kernel costs.",
  "CPU profiles use the existing interval-weighted sampled-stack analysis. Buckets partition sampled self-time, include idle samples, and are not exact CPU accounting or additive wall stages. Inclusive hotspots overlap and must not be summed.",
  "CPU files can span multiple drains plus inter-drain harness/control work; separate profiler start/stop commands introduce boundary skew. Profile duration is not assumed to equal summed query wall time, and samples are not automatically normalized per query.",
  "Groups pool raw samples only with matching host, source snapshot, workload, limits, mode, boundary and consumer. Each process median is also retained; medianOfProcessMediansMs is separate from pooled p50.",
];

function check(condition, message) {
  if (!condition) throw new Error(message);
}

function finite(value, name) {
  check(typeof value === "number" && Number.isFinite(value), `${name} must be finite`);
  return value;
}

function integer(value, name) {
  check(Number.isSafeInteger(value) && value >= 0, `${name} must be a non-negative safe integer`);
  return value;
}

function closeEnough(actual, expected, name) {
  check(Math.abs(actual - expected) <= 1e-7 * Math.max(1, Math.abs(actual), Math.abs(expected)),
    `${name}: ${actual} differs from ${expected}`);
}

function median(values) {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}

function statistics(values) {
  check(values.length > 0, "Cannot summarize empty samples");
  const total = sum(values);
  return { count: values.length, sum: total, mean: total / values.length,
    p50: median(values), min: Math.min(...values), max: Math.max(...values) };
}

function stage(stages, name) {
  return stages[name]?.sum ?? 0;
}

function validateStages(stages, where) {
  check(stages !== null && typeof stages === "object", `${where}: stages are missing`);
  for (const [name, value] of Object.entries(stages)) {
    finite(value.sum, `${where}.${name}.sum`);
    integer(value.count, `${where}.${name}.count`);
  }
}

function sampleLedger(sample, workload, mode, limits, sampleIndex) {
  const where = `${workload.id} sample ${sampleIndex}`;
  check(["direct", "worker"].includes(sample.boundary), `${where}: invalid boundary`);
  check(["fingerprint", "count"].includes(sample.consumer), `${where}: invalid consumer`);
  const expected = { ...workload.expected };
  if (sample.consumer === "count") delete expected.fingerprint;
  check(isDeepStrictEqual(sample.result, expected), `${where}: result/type/cost mismatch`);
  check(sample.result.rows === workload.expectedRows, `${where}: unexpected row count`);
  const totalMs = finite(sample.totalMs, `${where}.totalMs`);
  const phasesMs = Object.fromEntries(["queryMs", "firstPullMs", "restMs", "closeMs"]
    .map((name) => [name, finite(sample[name], `${where}.${name}`)]));
  closeEnough(sum(Object.values(phasesMs)), totalMs, `${where}: drain phases`);
  const diagnostic = sample.diagnostic;
  check(diagnostic?.worker?.pending === 0, `${where}: pending worker RPCs`);
  if (diagnostic.mainPending !== undefined) check(diagnostic.mainPending === 0, `${where}: pending main RPCs`);
  validateStages(diagnostic.mainStages, `${where}.mainStages`);
  validateStages(diagnostic.worker.stages, `${where}.worker.stages`);
  const mainRpcs = diagnostic.mainRpcs;
  const workerRpcs = diagnostic.worker.rpcs;
  check(Array.isArray(mainRpcs) && Array.isArray(workerRpcs), `${where}: missing RPC arrays`);
  check(mainRpcs.length === workerRpcs.length, `${where}: unmatched RPC counts`);
  const ids = new Set();
  const matchedRpcs = mainRpcs.map((main, index) => {
    integer(main.id, `${where}: RPC ID`);
    check(!ids.has(main.id), `${where}: duplicate RPC ${main.id}`);
    ids.add(main.id);
    const worker = workerRpcs[index];
    check(main.id === worker.id && main.op === worker.op, `${where}: RPC ID/op/order mismatch`);
    check(typeof main.op === "string", `${where}: invalid RPC op`);
    const mainMs = finite(main.ms, `${where}: main RPC duration`);
    const workerMs = finite(worker.ms, `${where}: worker RPC duration`);
    const rows = integer(worker.rows, `${where}: packet rows`);
    const packetBytes = integer(worker.packetBytes, `${where}: packet bytes`);
    if (main.op === "fetch") {
      check(rows <= limits.maxBatchRows, `${where}: batch row limit exceeded`);
      check(packetBytes >= 4 && packetBytes <= limits.maxBatchBytes, `${where}: invalid packet size`);
    } else {
      check(rows === 0 && packetBytes === 0, `${where}: non-fetch has packet data`);
    }
    return { id: main.id, op: main.op, mainMs, workerMs,
      deliveryDispatchResidualMs: mainMs - workerMs, rows, packetBytes };
  });
  const collected = ["coarse", "detailed"].includes(mode) && sample.boundary === "worker";
  if (!collected) {
    check(matchedRpcs.length === 0 && Object.keys(diagnostic.mainStages).length === 0 &&
      Object.keys(diagnostic.worker.stages).length === 0, `${where}: unexpected stage collection`);
    return { sampleIndex, boundary: sample.boundary, consumer: sample.consumer, totalMs, phasesMs,
      ledgerMs: { [sample.boundary === "direct" ? "directExecutionAndConsumerMs" : "unpartitionedMs"]: totalMs },
      totals: null, stages: diagnostic };
  }
  check(matchedRpcs.length > 0, `${where}: no collected RPCs`);
  const fetches = matchedRpcs.filter((rpc) => rpc.op === "fetch");
  const rows = sum(fetches.map((rpc) => rpc.rows));
  const packetBytes = sum(fetches.map((rpc) => rpc.packetBytes));
  integer(rows, `${where}: total rows`);
  integer(packetBytes, `${where}: total packet bytes`);
  check(rows === sample.result.rows, `${where}: transferred rows differ from drained rows`);
  const main = diagnostic.mainStages;
  const worker = diagnostic.worker.stages;
  for (const name of ["decode_validate", "decode_construct"]) {
    check((main[name]?.count ?? 0) === fetches.length, `${where}: ${name} count differs from fetch count`);
  }
  const mainRpcMs = sum(matchedRpcs.map((rpc) => rpc.mainMs));
  const workerServiceMs = sum(matchedRpcs.map((rpc) => rpc.workerMs));
  const scheduleMs = stage(worker, "schedule_wait");
  const ioMs = stage(worker, "io_wait");
  const validateMs = stage(main, "decode_validate");
  const constructMs = stage(main, "decode_construct");
  const ledgerMs = { workerScheduleWaitMs: scheduleMs, workerIoWaitMs: ioMs };
  if (mode === "detailed") {
    const appendCount = worker.encode_append?.count ?? 0;
    check(appendCount === (worker.encode_measure?.count ?? 0), `${where}: append/measure counts differ`);
    check(appendCount >= rows && appendCount <= rows + fetches.length, `${where}: invalid append attempt count`);
    check((worker.encode_finish?.count ?? 0) === fetches.length, `${where}: finish count differs from fetch count`);
    const appendMs = stage(worker, "encode_append");
    const measureMs = stage(worker, "encode_measure");
    const finishMs = stage(worker, "encode_finish");
    ledgerMs.workerEncodeMeasureMs = measureMs;
    ledgerMs.workerEncodeWriteMs = appendMs - measureMs + finishMs;
    ledgerMs.workerExecutionAndRuntimeMs = workerServiceMs - scheduleMs - ioMs - appendMs - finishMs;
  } else {
    check(["encode_append", "encode_measure", "encode_finish"].every((name) => worker[name] === undefined),
      `${where}: detailed encode timers in coarse run`);
    ledgerMs.workerExecutionEncodingAndRuntimeMs = workerServiceMs - scheduleMs - ioMs;
  }
  ledgerMs.deliveryDispatchResidualMs = mainRpcMs - workerServiceMs;
  ledgerMs.mainDecodeValidateMs = validateMs;
  ledgerMs.mainDecodeConstructMs = constructMs;
  ledgerMs.mainRemainderMs = totalMs - mainRpcMs - validateMs - constructMs;
  closeEnough(sum(Object.values(ledgerMs)), totalMs, `${where}: additive ledger`);
  return { sampleIndex, boundary: sample.boundary, consumer: sample.consumer, totalMs, phasesMs,
    ledgerMs, totals: { mainRpcMs, workerServiceMs, rpcCount: matchedRpcs.length,
      fetchCount: fetches.length, rows, packetBytes },
    rpcResiduals: { ...statistics(matchedRpcs.map((rpc) => rpc.deliveryDispatchResidualMs)),
      negativeCount: matchedRpcs.filter((rpc) => rpc.deliveryDispatchResidualMs < 0).length },
    stages: { main, worker },
    negativeComponents: Object.keys(ledgerMs).filter((name) => ledgerMs[name] < 0) };
}

function summarizeSamples(samples) {
  const keys = Object.keys(samples[0].ledgerMs);
  check(samples.every((sample) => isDeepStrictEqual(Object.keys(sample.ledgerMs), keys)), "Different ledgers in one group");
  const ledger = Object.fromEntries(keys.map((key) => [key, statistics(samples.map((sample) => sample.ledgerMs[key]))]));
  const totalMs = statistics(samples.map((sample) => sample.totalMs));
  closeEnough(sum(Object.values(ledger).map((entry) => entry.mean)), totalMs.mean, "Mean additive ledger");
  return { totalMs, ledger, meanLedgerMs: Object.fromEntries(keys.map((key) => [key, ledger[key].mean])),
    medianLedgerMs: Object.fromEntries(keys.map((key) => [key, ledger[key].p50])),
    phasesMs: Object.fromEntries(Object.keys(samples[0].phasesMs).map((key) =>
      [key, statistics(samples.map((sample) => sample.phasesMs[key]))])),
    totals: samples[0].totals === null ? null : Object.fromEntries(Object.keys(samples[0].totals)
      .map((key) => [key, statistics(samples.map((sample) => sample.totals[key]))])),
    negativeSampleCount: samples.filter((sample) => sample.negativeComponents?.length).length };
}

async function cpuSummaries(folder, destination, data, runId) {
  const names = (await readdir(folder)).filter((name) => name.endsWith(".cpuprofile.gz")).sort();
  if (data.mode === "cpu") check(names.length === data.cases.length * 4, `${runId}: expected four CPU profiles per case`);
  if (names.length === 0) return [];
  const browser = data.host?.host === "Chromium";
  const sourceMap = browser ? await loadSourceMaps(join(folder, "site")) : undefined;
  if (browser) check(sourceMap.maps.length > 0, `${runId}: browser CPU profiles require source maps`);
  const overview = [];
  for (const name of names) {
    const match = data.cases.flatMap((workload) => ["direct", "worker"].flatMap((boundary) =>
      ["main", "worker"].map((thread) => ({ case: workload.id, boundary, thread,
        name: `${workload.id}-${boundary}-${thread}.cpuprofile.gz` })))).find((entry) => entry.name === name);
    check(match !== undefined, `${runId}: unknown CPU profile filename ${name}`);
    const raw = await readFile(join(folder, name));
    const profile = JSON.parse((await decompress(raw)).toString("utf8"));
    const summary = summarizeCpuProfile(profile, { sourceMap, hotspotLimit: Infinity, stackLimit: Infinity });
    const summaryName = `${name.slice(0, -".cpuprofile.gz".length)}.summary.json.gz`;
    const packed = await compress(JSON.stringify(summary));
    await writeFile(join(destination, summaryName), packed);
    overview.push({ ...match, rawSha256: digest(raw), summaryFile: `${runId}/${summaryName}`,
      summarySha256: digest(packed), profileDurationMs: summary.profileDurationMs,
      sampledMs: summary.sampledMs, activeSampledMs: summary.activeSampledMs,
      unsampledTailMs: summary.unsampledTailMs, sampleOverrunMs: summary.sampleOverrunMs,
      sampleCount: summary.sampleCount, nodeCount: summary.nodeCount,
      mappedNodeCount: summary.mappedNodeCount, timeDeltaUs: summary.timeDeltaUs,
      buckets: summary.buckets,
      hotspots: summary.hotspots.slice(0, 20).map(({ frame, selfMs, inclusiveMs, samples, percent }) =>
        ({ frame, selfMs, inclusiveMs, samples, percent })) });
  }
  return overview;
}

async function analyze(directories, output, wallOnly) {
  const runs = [];
  const groups = new Map();
  const packets = new Map();
  await mkdir(output, { recursive: true });
  for (const [index, folder] of directories.entries()) {
    const runId = `${String(index + 1).padStart(3, "0")}-${basename(folder)}`;
    const raw = await readFile(join(folder, "results.json"));
    const data = JSON.parse(raw.toString("utf8"));
    check(["plain", "coarse", "detailed", "cpu"].includes(data.mode), `${runId}: invalid mode`);
    check(Array.isArray(data.cases) && data.cases.length > 0, `${runId}: missing cases`);
    const host = data.host ?? data.environment;
    check(host?.host, `${runId}: missing host metadata`);
    const sourceHash = digest(JSON.stringify(host.sourceManifest?.snapshotSha256 ?? {}));
    const identity = { host: host.host, node: host.node, browser: host.browser, v8: host.v8,
      platform: host.platform, arch: host.arch, cpu: host.cpu, affinity: host.affinity,
      crossOriginIsolated: data.environment?.crossOriginIsolated,
      sharedArrayBuffer: data.environment?.sharedArrayBuffer, sourceHash };
    const cases = [];
    for (const workload of data.cases) {
      check(Array.isArray(workload.samples) && workload.samples.length > 0, `${runId}: missing samples`);
      const samples = workload.samples.map((sample, sampleIndex) =>
        sampleLedger(sample, workload, data.mode, data.limits, sampleIndex));
      for (const sample of samples) {
        if (sample.totals) {
          const packetKey = JSON.stringify([workload.sql, workload.setup, workload.expected, data.limits,
            host.sourceManifest?.sourceSha256]);
          const actual = [sample.totals.fetchCount, sample.totals.rows, sample.totals.packetBytes];
          if (packets.has(packetKey)) check(isDeepStrictEqual(actual, packets.get(packetKey)),
            `${runId}/${workload.id}: exact packet totals changed across equivalent samples`);
          else packets.set(packetKey, actual);
        }
        const key = JSON.stringify([identity, data.mode, workload.id, workload.sql, workload.setup,
          workload.expected, data.limits, sample.boundary, sample.consumer]);
        let group = groups.get(key);
        if (!group) {
          group = { identity, mode: data.mode, case: workload.id, sql: workload.sql,
            expected: workload.expected, limits: data.limits, boundary: sample.boundary,
            consumer: sample.consumer, byRun: new Map(), samples: [] };
          groups.set(key, group);
        }
        group.samples.push(sample);
        if (!group.byRun.has(runId)) group.byRun.set(runId, []);
        group.byRun.get(runId).push(sample);
      }
      cases.push({ id: workload.id, sql: workload.sql, expected: workload.expected, samples });
    }
    const destination = join(output, runId);
    await mkdir(destination, { recursive: true });
    runs.push({ runId, directory: folder, resultsSha256: digest(raw), recordedAt: data.recordedAt,
      mode: data.mode, identity, config: data.config, profileConfig: data.profileConfig,
      limits: data.limits, cases, cpu: wallOnly ? [] : await cpuSummaries(folder, destination, data, runId) });
  }
  const summaries = [...groups.values()].map(({ byRun, samples, ...group }) => {
    const processes = [...byRun].map(([runId, values]) => ({ runId, ...summarizeSamples(values) }));
    return { ...group, pooledSamples: summarizeSamples(samples), processes,
      medianOfProcessMediansMs: median(processes.map((process) => process.totalMs.p50)) };
  });
  await writeFile(join(output, "analysis.json"), `${JSON.stringify({ schemaVersion: 1,
    analyzedAt: new Date().toISOString(), wallOnly, methodology, runs, groups: summaries }, null, 2)}\n`);
  console.log(JSON.stringify({ output: join(output, "analysis.json"), runs: runs.length,
    groups: summaries.length, cpuProfiles: sum(runs.map((run) => run.cpu.length)) }));
}

const args = process.argv.slice(2);
if (args.includes("--help")) {
  console.log("Usage: mise exec -- node bench/ts/profile/analyze.mjs --out OUTPUT [--wall-only] RESULT_DIR...");
  console.log("Reads results.json and optional *.cpuprofile.gz in each directory; browser profiles use site/*.map.");
  console.log("Writes analysis.json (raw additive sample ledgers, means/medians and CPU overview) plus compressed full CPU summaries.");
} else {
  let output;
  let wallOnly = false;
  const directories = [];
  for (let i = 0; i < args.length; i++) {
    if (args[i] === "--out") {
      check(output === undefined && args[i + 1] && !args[i + 1].startsWith("--"), "--out requires one directory");
      output = resolve(args[++i]);
    } else if (args[i] === "--wall-only") wallOnly = true;
    else {
      check(!args[i].startsWith("--"), `Unknown argument ${args[i]}`);
      directories.push(resolve(args[i]));
    }
  }
  check(output && directories.length, "Use --out OUTPUT and at least one RESULT_DIR (or --help)");
  check(new Set(directories).size === directories.length, "Duplicate input directories");
  check(!directories.includes(output), "Output must differ from input directories");
  await analyze(directories, output, wallOnly);
}
