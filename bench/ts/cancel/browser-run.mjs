import { createServer } from "node:http";
import { readFileSync, writeFileSync, readdirSync, statSync } from "node:fs";
import { resolve, join, extname, sep } from "node:path";
import { pathToFileURL } from "node:url";
import { assertSourcesUnchanged, digest, environmentMetadata, sourceHashes } from "./metadata.mjs";
const root = resolve(process.env.JED_PROBE_ROOT ?? process.cwd());
const hashes = sourceHashes(root);
const toolsRoot = resolve(process.env.JED_PROBE_TOOLS_ROOT ?? process.cwd());
const { chromium } = await import(
  pathToFileURL(`${toolsRoot}/impl/ts/node_modules/playwright-core/index.mjs`)
);
const directory = resolve(process.argv[2]);
const site = join(directory, "site");
const buildHashes = JSON.parse(readFileSync(join(site, "probe-sources.json"), "utf8"));
if (JSON.stringify(buildHashes) !== JSON.stringify(hashes))
  throw new Error("Cancellation probe sources no longer match the browser build");
const timeoutMs = Number(process.env.JED_CANCEL_RUN_TIMEOUT_MS ?? 600000);
if (!Number.isSafeInteger(timeoutMs) || timeoutMs <= 0) throw new Error("invalid run timeout");
const settings = Object.fromEntries(
  Object.entries(process.env).filter(
    ([key]) => key.startsWith("JED_ASYNC_HEAVY_") || key.startsWith("JED_CANCEL_"),
  ),
);
const types = { ".html": "text/html", ".js": "text/javascript", ".mjs": "text/javascript" };
const server = createServer((request, response) => {
  // Deliberately no isolation headers: ordinary Chromium must have no SharedArrayBuffer.
  response.setHeader("Cache-Control", "no-store");
  try {
    const pathname = decodeURIComponent(new URL(request.url, "http://localhost").pathname);
    const file = resolve(site, `.${pathname === "/" ? "/index.html" : pathname}`);
    if (!file.startsWith(`${site}${sep}`) || !statSync(file).isFile()) throw new Error("not found");
    response.setHeader("Content-Type", types[extname(file)] ?? "application/octet-stream");
    response.end(readFileSync(file));
  } catch {
    response.statusCode = 404;
    response.end("Not found");
  }
});
await new Promise((done, reject) => {
  server.once("error", reject);
  server.listen(0, "127.0.0.1", done);
});
let browser;
let timeout;
try {
  browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  page.setDefaultTimeout(600000);
  const errors = [];
  page.on("pageerror", (error) => errors.push(String(error)));
  await page.goto(`http://127.0.0.1:${server.address().port}`, { waitUntil: "load" });
  await page.bringToFront();
  const cdp = await page.context().newCDPSession(page);
  const browserVersion = await cdp.send("Browser.getVersion");
  await cdp.detach();
  await page.waitForFunction(() => typeof window.runCancellationProbe === "function");
  const result = await Promise.race([
    page.evaluate((config) => window.runCancellationProbe(config), settings),
    new Promise((_, reject) => {
      timeout = setTimeout(() => reject(new Error("Cancellation probe timed out")), timeoutMs);
    }),
  ]);
  if (errors.length) throw new Error(errors.join("\n"));
  assertSourcesUnchanged(root, hashes);
  const artifacts = Object.fromEntries(
    readdirSync(site, { recursive: true })
      .filter((name) => statSync(join(site, name)).isFile())
      .map((name) => {
        const bytes = readFileSync(join(site, name));
        return [name, { bytes: bytes.length, sha256: digest(bytes) }];
      }),
  );
  const output = join(directory, "results.json");
  writeFileSync(
    output,
    JSON.stringify(
      {
        recordedAt: new Date().toISOString(),
        metadata: {
          ...environmentMetadata(root),
          browser: browser.version(),
          jsVersion: browserVersion.jsVersion,
          headless: true,
          isolationHeaders: false,
          ...hashes,
          artifacts,
          build: { mode: "production", target: "esnext", minified: true, workerFormat: "es" },
        },
        ...result,
      },
      null,
      2,
    ) + "\n",
  );
  console.log(output);
} finally {
  clearTimeout(timeout);
  await browser?.close();
  await new Promise((done) => server.close(done));
}
