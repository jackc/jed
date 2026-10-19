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
const isolated = process.env.JED_ASYNC_BROWSER_ISOLATED === "1";
const timeoutMs = Number(process.env.JED_ASYNC_BROWSER_TIMEOUT_MS ?? 1_800_000);
if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 1) throw new Error("invalid browser timeout");
const settings = Object.fromEntries(
  Object.entries(process.env).filter(([key]) => key.startsWith("JED_ASYNC_HEAVY_")),
);
const contentTypes = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json",
  ".wasm": "application/wasm",
};
const server = createServer((request, response) => {
  if (isolated) {
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

let browser;
let timeout;
const pageErrors = [];
try {
  browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  page.on("pageerror", (error) => pageErrors.push(String(error.stack ?? error)));
  page.on("console", (message) => {
    if (message.text().startsWith("JED_HEAVY_PROGRESS ")) console.log(message.text());
  });
  const address = server.address();
  await page.goto(`http://127.0.0.1:${address.port}`, { waitUntil: "load" });
  await page.bringToFront();
  const cdp = await page.context().newCDPSession(page);
  const browserVersion = await cdp.send("Browser.getVersion");
  await cdp.detach();
  await page.waitForFunction(() => typeof window.runHeavyBenchmark === "function");
  const result = await Promise.race([
    page.evaluate((config) => window.runHeavyBenchmark(config), settings),
    new Promise((_, reject) => {
      timeout = setTimeout(() => reject(new Error("browser benchmark timed out")), timeoutMs);
    }),
  ]);
  if (pageErrors.length > 0) throw new Error(pageErrors.join("\n"));
  const sourceFiles = [
    "bench/ts/browser/index.html",
    "bench/ts/browser/heavy.ts",
    "bench/ts/browser/run.mjs",
    "bench/ts/browser/vite.config.ts",
    "bench/ts/src/async-heavy.ts",
    "mise-tasks/bench/ts_async_browser",
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
  } catch { /* CPU affinity is platform metadata, not a benchmark prerequisite. */ }
  const output = {
    metadata: {
      dateUtc: new Date().toISOString(),
      revision: process.env.JED_BENCH_REVISION,
      dirty: process.env.JED_BENCH_DIRTY === "true",
      browser: browser.version(),
      jsVersion: browserVersion.jsVersion,
      headless: true,
      isolationHeaders: isolated,
      node: process.version,
      platform: platform(),
      release: release(),
      architecture: arch(),
      cpu: cpus()[0]?.model,
      logicalCpus: cpus().length,
      affinity,
      build: { mode: "production", target: "esnext", minified: true, workerFormat: "es" },
      sourceSha256: Object.fromEntries(
        sourceFiles.map((name) => [name, digest(readFileSync(join(repository, name)))]),
      ),
      artifacts,
    },
    ...result,
  };
  writeFileSync(
    join(directory, "results.json"),
    JSON.stringify(output, (_, value) => typeof value === "bigint" ? value.toString() : value, 2) + "\n",
  );
  console.log(`Browser heavy results: ${join(directory, "results.json")}`);
} finally {
  clearTimeout(timeout);
  await browser?.close();
  await new Promise((resolveClose) => server.close(resolveClose));
}
