import { readFileSync, writeFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { gunzipSync } from "node:zlib";

const directory = resolve(process.argv[2]);
const matrix = JSON.parse(readFileSync(join(directory, "matrix.json"), "utf8"));
const variants = ["master-direct", "branch-direct", "branch-worker"];
const median = (values) => {
  const sorted = [...values].sort((a, b) => a - b);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
};
const same = (actual, expected, context) => {
  if (JSON.stringify(actual) !== JSON.stringify(expected)) throw new Error(`${context}: mismatch`);
};
const runs = matrix.runs.map((entry) => {
  const result = JSON.parse(gunzipSync(readFileSync(join(directory, entry.path))));
  same([result.host, result.mode, result.revision], [entry.host, entry.mode, entry.revision], entry.path);
  return { ...entry, result };
});
if (runs.length !== matrix.hosts.length * matrix.repeats * variants.length)
  throw new Error("Incomplete matrix");
const semantics = (result) => ({
  config: result.suite.config,
  heavy: result.suite.heavyCases.map(({ id, sql, setup, expectedRows, validated }) =>
    ({ id, sql, setup, expectedRows, validated })),
  cheap: result.suite.cheapCases.map(({ id, sql, params, operations, validated, preparedQueryValidation }) =>
    ({ id, sql, params, operations, validated, preparedQueryValidation })),
});
for (const run of runs) {
  same(semantics(run.result), semantics(runs[0].result), `${run.path}/values-types-costs-config`);
  same(run.result.sourceManifest.harnessSourceSha256, runs[0].result.sourceManifest.harnessSourceSha256,
    `${run.path}/harness`);
  const sameRevision = runs.find((other) => other.revision === run.revision);
  same(run.result.sourceManifest.engineSourceSha256, sameRevision.result.sourceManifest.engineSourceSha256,
    `${run.path}/engine`);
}
const measurements = (cases, cheap) => {
  const processMedians = cases.map((item) => (cheap ? item.latencyUs : item.latencyMs).p50);
  const raw = cases.flatMap((item) => cheap ? item.rawSamplesUs : item.rawSamplesMs);
  const observations = cheap ? [] : cases.flatMap((item) => item.responsiveness);
  const timerMaxima = observations.map((item) => item.timer.gaps.max);
  const frameMaxima = observations.map((item) => item.extra?.animationFrames.summary.maxMs).filter((n) => n != null);
  return {
    median: median(processMedians), processMedians, pooledMedian: median(raw),
    pooledMean: raw.reduce((sum, value) => sum + value, 0) / raw.length,
    min: Math.min(...raw), max: Math.max(...raw), samples: raw.length,
    ...(cheap ? {} : {
      responsiveness: {
        observations: observations.length,
        timerMaximaMs: timerMaxima, medianTimerMaxMs: median(timerMaxima), worstTimerMaxMs: Math.max(...timerMaxima),
        ...(frameMaxima.length ? {
          frameMaximaMs: frameMaxima, medianFrameMaxMs: median(frameMaxima), worstFrameMaxMs: Math.max(...frameMaxima),
          longTasks: observations.reduce((sum, item) => sum + item.extra.longTasks.count, 0),
          longestTaskMs: Math.max(...observations.map((item) => item.extra.longTasks.maxDurationMs)),
        } : {}),
      },
    }),
  };
};
const hosts = Object.fromEntries(matrix.hosts.map((host) => {
  const groups = Object.fromEntries(variants.map((variant) => {
    const matching = runs.filter((run) => run.host === host && run.variant === variant);
    if (new Set(matching.map((run) => run.repeat)).size !== matrix.repeats)
      throw new Error(`Missing or duplicate repeats: ${host}/${variant}`);
    return [variant, matching.map((run) => run.result)];
  }));
  const cases = [false, true].flatMap((cheap) => {
    const key = cheap ? "cheapCases" : "heavyCases";
    return groups[variants[0]][0].suite[key].map(({ id }) => {
      const timings = Object.fromEntries(variants.map((variant) => [variant,
        measurements(groups[variant].map((result) => result.suite[key].find((item) => item.id === id)), cheap),
      ]));
      const base = timings["master-direct"].median;
      const direct = timings["branch-direct"].median;
      const worker = timings["branch-worker"].median;
      return { id, unit: cheap ? "us" : "ms", timings,
        directOverMasterPercent: (direct / base - 1) * 100,
        workerOverMasterPercent: (worker / base - 1) * 100,
        workerOverBranchDirectPercent: (worker / direct - 1) * 100,
      };
    });
  });
  return [host, { environment: groups[variants[0]][0].environment, cases }];
}));
const summary = {
  methodology: "Median of independent process medians. Rotating order; complete typed drains. Responsiveness measured separately. All variants agree on values, types, costs and configuration.",
  revisions: matrix.revisions, repeats: matrix.repeats, config: runs[0].result.suite.config, hosts,
};
writeFileSync(join(directory, "summary.json"), JSON.stringify(summary, null, 2) + "\n");
const format = (value) => value.toFixed(2);
const percent = (value) => `${value >= 0 ? "+" : ""}${value.toFixed(1)}%`;
const lines = [summary.methodology, ""];
for (const [host, { cases }] of Object.entries(hosts)) {
  lines.push(`${host}:`, "", "| Workload | Master direct | Branch direct | Branch worker | Direct/master | Worker/master |", "|---|---:|---:|---:|---:|---:|");
  for (const item of cases) lines.push(`| ${item.id} (${item.unit}) | ${variants.map((variant) => format(item.timings[variant].median)).join(" | ")} | ${percent(item.directOverMasterPercent)} | ${percent(item.workerOverMasterPercent)} |`);
  lines.push("", "| Heavy workload | Master timer gap | Worker timer gap | Worker frame gap | Worker long tasks |", "|---|---:|---:|---:|---:|");
  for (const item of cases.filter((item) => item.unit === "ms")) {
    const master = item.timings["master-direct"].responsiveness;
    const worker = item.timings["branch-worker"].responsiveness;
    lines.push(`| ${item.id} | ${format(master.worstTimerMaxMs)} ms | ${format(worker.worstTimerMaxMs)} ms | ${worker.worstFrameMaxMs == null ? "—" : `${format(worker.worstFrameMaxMs)} ms`} | ${worker.longTasks ?? "—"} |`);
  }
  lines.push("");
}
writeFileSync(join(directory, "summary.md"), lines.join("\n"));
console.log(lines.join("\n"));
