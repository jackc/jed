import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { arch, cpus, platform, release } from "node:os";
import { dirname, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";

export const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");

function filesUnder(root, path) {
  const directory = join(root, path);
  return readdirSync(directory, { recursive: true })
    .map((name) => join(directory, name))
    .filter((name) => statSync(name).isFile())
    .map((name) => relative(root, name));
}

function hashes(root, files) {
  return Object.fromEntries(
    files.sort().map((name) => [name, digest(readFileSync(join(root, name)))]),
  );
}

export function sourceManifest(engineRoot, harnessRoot = repositoryRoot) {
  const engineSourceSha256 = hashes(engineRoot, [
    "impl/ts/package.json",
    "impl/ts/package-lock.json",
    ...filesUnder(engineRoot, "impl/ts/src"),
  ]);
  const harnessSourceSha256 = hashes(harnessRoot, [
    ...filesUnder(harnessRoot, "bench/ts/compare"),
    "bench/ts/src/async-heavy.ts",
    "bench/ts/browser/observe.ts",
    "mise-tasks/bench/ts_compare",
    "mise-tasks/bench/ts_compare_matrix",
  ]);
  return {
    engineRoot: resolve(engineRoot),
    harnessRoot: resolve(harnessRoot),
    engineSourceSha256,
    harnessSourceSha256,
    digest: digest(JSON.stringify({ engineSourceSha256, harnessSourceSha256 })),
  };
}

export function assertSourcesUnchanged(engineRoot, before, harnessRoot = repositoryRoot) {
  const after = sourceManifest(engineRoot, harnessRoot);
  if (before.digest !== after.digest) throw new Error("engine or harness source changed during measurement");
}

export function environmentMetadata() {
  let cpuAffinity = null;
  try {
    cpuAffinity = /^Cpus_allowed_list:\s*(.+)$/m.exec(readFileSync("/proc/self/status", "utf8"))?.[1] ?? null;
  } catch {
    // Affinity is metadata rather than a portability prerequisite.
  }
  return {
    node: process.version,
    v8: process.versions.v8,
    platform: platform(),
    release: release(),
    architecture: arch(),
    cpu: cpus()[0]?.model,
    logicalCpus: cpus().length,
    cpuAffinity,
    storage: "memory",
    pageSize: 8192,
    measuredQueriesPassAbortSignal: false,
  };
}

export function writeCompactResult(directory, result) {
  mkdirSync(directory, { recursive: true });
  const bytes = Buffer.from(
    JSON.stringify(result, (_, value) => (typeof value === "bigint" ? value.toString() : value)),
  );
  writeFileSync(join(directory, "results.json.gz"), gzipSync(bytes, { level: 9 }));
}
