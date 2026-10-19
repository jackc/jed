import { createServer } from "node:http";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync, readdirSync, statSync } from "node:fs";
import { resolve, join, extname, sep } from "node:path";
import { gzipSync } from "node:zlib";
import { chromium } from "../../../impl/ts/node_modules/playwright-core/index.mjs";
import {
  assertSourcesUnchanged,
  environmentMetadata,
  sourceManifest,
  writeCompactResult,
} from "./metadata.mjs";

if (!process.env.JED_COMPARE_ROOT) throw new Error("JED_COMPARE_ROOT is required");
const root = resolve(process.env.JED_COMPARE_ROOT);
const mode = process.env.JED_COMPARE_MODE;
if (mode !== "direct" && mode !== "worker") throw new Error("Invalid JED_COMPARE_MODE");
const revision = process.env.JED_COMPARE_REVISION;
if (!revision) throw new Error("JED_COMPARE_REVISION is required");
const manifestOnly = process.argv[2] === "--manifest";
const directoryArgument = process.argv[manifestOnly ? 3 : 2];
if (!directoryArgument) throw new Error("Result directory is required");
const directory = resolve(directoryArgument);
const manifestPath = join(directory, "build-source-manifest.json");
if (manifestOnly) {
  writeFileSync(manifestPath, JSON.stringify(sourceManifest(root), null, 2) + "\n");
  process.exit(0);
}

// This was recorded before Vite ran, not after compilation. Comparing again before launch
// and after measurement ties the selected frozen source to the actual measured bundles.
const manifest = JSON.parse(readFileSync(manifestPath, "utf8"));
assertSourcesUnchanged(root, manifest);
const site = join(directory, "site");
const isolated = process.env.JED_COMPARE_BROWSER_ISOLATED === "1";
const timeoutMs = Number(process.env.JED_COMPARE_TIMEOUT_MS ?? 1_800_000);
if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 1) throw new Error("Invalid browser timeout");
const settings = Object.fromEntries(
  Object.entries(process.env).filter(([key]) =>
    key.startsWith("JED_ASYNC_HEAVY_") || key.startsWith("JED_COMPARE_")),
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
    if (!file.startsWith(`${site}${sep}`) || !statSync(file).isFile()) throw new Error("Not found");
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
const digest = (bytes) => createHash("sha256").update(bytes).digest("hex");
function bundleManifest() {
  return Object.fromEntries(
    readdirSync(site, { recursive: true })
      .filter((name) => statSync(join(site, name)).isFile())
      .sort()
      .map((name) => {
        const bytes = readFileSync(join(site, name));
        return [name, { bytes: bytes.length, gzipBytes: gzipSync(bytes).length, sha256: digest(bytes) }];
      }),
  );
}
const bundles = bundleManifest();
try {
  browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  page.on("pageerror", (error) => pageErrors.push(String(error.stack ?? error)));
  page.on("console", (message) => {
    if (message.text().startsWith("JED_COMPARE_PROGRESS ")) console.log(message.text());
  });
  const address = server.address();
  await page.goto(`http://127.0.0.1:${address.port}`, { waitUntil: "load" });
  await page.bringToFront();
  const cdp = await page.context().newCDPSession(page);
  const browserVersion = await cdp.send("Browser.getVersion");
  await cdp.detach();
  await page.waitForFunction(() => typeof window.runComparisonBenchmark === "function");
  const result = await Promise.race([
    page.evaluate((config) => window.runComparisonBenchmark(config), settings),
    new Promise((_, reject) => {
      timeout = setTimeout(() => reject(new Error("Browser comparison timed out")), timeoutMs);
    }),
  ]);
  if (pageErrors.length > 0) throw new Error(pageErrors.join("\n"));
  if (result.mode !== mode) throw new Error("Browser bundle mode differs from requested mode");
  assertSourcesUnchanged(root, manifest);
  if (JSON.stringify(bundleManifest()) !== JSON.stringify(bundles))
    throw new Error("Browser bundles changed during measurement");
  writeCompactResult(directory, {
    schemaVersion: 1,
    recordedAt: new Date().toISOString(),
    revision,
    host: "browser",
    sourceManifest: manifest,
    environment: {
      ...environmentMetadata(),
      browser: browser.version(),
      jsVersion: browserVersion.jsVersion,
      headless: true,
      isolationHeaders: isolated,
      build: { mode: "production", target: "esnext", minified: true, workerFormat: "es" },
      bundles,
    },
    ...result,
  });
  console.log(`Browser comparison results: ${join(directory, "results.json.gz")}`);
} finally {
  clearTimeout(timeout);
  await browser?.close();
  await new Promise((resolveClose) => server.close(resolveClose));
}
