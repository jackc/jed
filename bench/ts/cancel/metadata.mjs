import { createHash } from "node:crypto";
import { readFileSync, readdirSync, statSync } from "node:fs";
import { arch, cpus, platform, release } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";

export const digest = (data) => createHash("sha256").update(data).digest("hex");

export function sourceHashes(root) {
  const engine = join(root, "impl/ts/src");
  const sources = readdirSync(engine, { recursive: true })
    .filter((name) => name.endsWith(".ts") && statSync(join(engine, name)).isFile())
    .sort();
  const engineSourceSha256 = Object.fromEntries(
    sources.map((name) => [`impl/ts/src/${name}`, digest(readFileSync(join(engine, name)))]),
  );
  engineSourceSha256["bench/ts/src/async-heavy.ts"] = digest(
    readFileSync(join(root, "bench/ts/src/async-heavy.ts")),
  );
  const probeDirectory = fileURLToPath(new URL(".", import.meta.url));
  const probeSourceSha256 = Object.fromEntries(
    readdirSync(probeDirectory)
      .filter((name) => /\.(mjs|html)$/.test(name) && !name.includes(".timestamp-"))
      .sort()
      .map((name) => [name, digest(readFileSync(join(probeDirectory, name)))]),
  );
  probeSourceSha256["mise-tasks/bench/ts_async_cancel"] = digest(
    readFileSync(
      fileURLToPath(new URL("../../../mise-tasks/bench/ts_async_cancel", import.meta.url)),
    ),
  );
  return { engineSourceSha256, probeSourceSha256 };
}

export function assertSourcesUnchanged(root, before) {
  if (JSON.stringify(sourceHashes(root)) !== JSON.stringify(before))
    throw new Error("Cancellation probe sources changed during measurement");
}

export function environmentMetadata(root) {
  let affinity;
  try {
    affinity = readFileSync("/proc/self/status", "utf8").match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1];
  } catch {
    /* Affinity is platform metadata, not a prerequisite. */
  }
  return {
    root,
    revision: process.env.JED_BENCH_REVISION,
    dirty: process.env.JED_BENCH_DIRTY === "true",
    node: process.version,
    v8: process.versions.v8,
    platform: platform(),
    release: release(),
    architecture: arch(),
    cpu: cpus()[0]?.model,
    logicalCpus: cpus().length,
    affinity,
  };
}
