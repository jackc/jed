// Node driver for the integrated codec experiment (README.md): serve the built site, then run
// JED_AEAD_CODEC_RUNS process runs, each in a freshly launched headless Chromium with its own
// profile (fresh OPFS), and write every raw sample to <dir>/results.json.
import { createServer } from "node:http";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync, readdirSync, statSync } from "node:fs";
import { cpus, platform, release, arch } from "node:os";
import { resolve, join, extname, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";
import { chromium } from "../../../impl/ts/node_modules/playwright-core/index.mjs";

const directory = resolve(process.argv[2]);
const site = join(directory, "site");
const repository = fileURLToPath(new URL("../../..", import.meta.url));
const isolated = process.env.JED_AEAD_CODEC_ISOLATED !== "0";
const runs = Number(process.env.JED_AEAD_CODEC_RUNS ?? 5);
const timeoutMs = Number(process.env.JED_AEAD_CODEC_TIMEOUT_MS ?? 1_800_000);
if (!Number.isSafeInteger(runs) || runs < 1) throw new Error("invalid JED_AEAD_CODEC_RUNS");
if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 1) throw new Error("invalid timeout");
const settings = Object.fromEntries(
  Object.entries(process.env).filter(([key]) => key.startsWith("JED_AEAD_CODEC_")),
);
const contentTypes = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
};
const server = createServer((request, response) => {
  if (isolated) {
    // Cross-origin isolation gives performance.now() 5 µs rather than 100 µs resolution.
    response.setHeader("Cross-Origin-Opener-Policy", "same-origin");
    response.setHeader("Cross-Origin-Embedder-Policy", "require-corp");
  }
  response.setHeader("Cache-Control", "no-store");
  try {
    const pathname = decodeURIComponent(new URL(request.url, "http://localhost").pathname);
    const file = resolve(site, `.${pathname === "/" ? "/index.html" : pathname}`);
    if (!file.startsWith(`${site}${sep}`) || !statSync(file).isFile()) throw new Error("not found");
    response.setHeader("Content-Type", contentTypes[extname(file)] ?? "application/octet-stream");
    response.end(readFileSync(file));
  } catch {
    response.statusCode = 404;
    response.end("Not found");
  }
});
await new Promise((resolveListen, reject) => {
  server.once("error", reject);
  server.listen(0, "127.0.0.1", resolveListen);
});

const processRuns = [];
let browserVersion;
let jsVersion;
try {
  for (let runIndex = 0; runIndex < runs; runIndex++) {
    const browser = await chromium.launch({ headless: true });
    let timeout;
    try {
      browserVersion = browser.version();
      const context = await browser.newContext({ viewport: { width: 1280, height: 800 } });
      const page = await context.newPage();
      const pageErrors = [];
      page.on("pageerror", (error) => pageErrors.push(String(error.stack ?? error)));
      page.on("console", (message) => {
        if (message.text().startsWith("JED_CODEC_PROGRESS ")) console.log(message.text());
      });
      await page.goto(`http://127.0.0.1:${server.address().port}`, { waitUntil: "load" });
      await page.bringToFront();
      const cdp = await context.newCDPSession(page);
      jsVersion = (await cdp.send("Browser.getVersion")).jsVersion;
      await cdp.detach();
      await page.waitForFunction(() => typeof window.runCodecBenchmark === "function");
      const result = await Promise.race([
        page.evaluate(
          (config) => window.runCodecBenchmark(config),
          { ...settings, JED_AEAD_CODEC_RUN_INDEX: String(runIndex) },
        ),
        new Promise((_, reject) => {
          timeout = setTimeout(() => reject(new Error("browser benchmark timed out")), timeoutMs);
        }),
      ]);
      if (pageErrors.length > 0) throw new Error(pageErrors.join("\n"));
      processRuns.push(result);
    } finally {
      clearTimeout(timeout);
      await browser.close();
    }
  }
} finally {
  await new Promise((resolveClose) => server.close(resolveClose));
}

const sourceFiles = [
  "bench/aead/codec/index.html",
  "bench/aead/codec/main.ts",
  "bench/aead/codec/worker.ts",
  "bench/aead/codec/codecs.ts",
  "bench/aead/codec/run.mjs",
  "bench/aead/codec/summarize.rb",
  "bench/aead/codec/vite.config.ts",
  "bench/aead/ts/package-lock.json",
  "impl/ts/src/pager.ts",
  "impl/ts/src/commit_manifest.ts",
  "mise-tasks/bench/aead_codec_browser",
];
const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");
const artifacts = Object.fromEntries(
  readdirSync(site, { recursive: true })
    .filter((name) => statSync(join(site, name)).isFile())
    .sort()
    .map((name) => {
      const bytes = readFileSync(join(site, name));
      return [name, { bytes: bytes.length, gzipBytes: gzipSync(bytes).length, sha256: digest(bytes) }];
    }),
);
let affinity;
try {
  affinity = readFileSync("/proc/self/status", "utf8").match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1];
} catch {
  // CPU affinity is platform metadata, not a prerequisite.
}
const output = {
  metadata: {
    dateUtc: new Date().toISOString(),
    revision: process.env.JED_BENCH_REVISION,
    dirty: process.env.JED_BENCH_DIRTY === "true",
    browser: browserVersion,
    jsVersion,
    headless: true,
    isolationHeaders: isolated,
    processRuns: runs,
    node: process.version,
    platform: platform(),
    release: release(),
    architecture: arch(),
    cpu: cpus()[0]?.model,
    logicalCpus: cpus().length,
    affinity,
    noble: JSON.parse(
      readFileSync(join(repository, "bench/aead/ts/node_modules/@noble/ciphers/package.json"), "utf8"),
    ).version,
    build: { mode: "production", target: "esnext", minified: true, workerFormat: "es" },
    sourceSha256: Object.fromEntries(
      sourceFiles.map((name) => [name, digest(readFileSync(join(repository, name)))]),
    ),
    artifacts,
  },
  runs: processRuns,
};
writeFileSync(join(directory, "results.json"), JSON.stringify(output) + "\n");
console.log(`Codec benchmark results: ${join(directory, "results.json")}`);
