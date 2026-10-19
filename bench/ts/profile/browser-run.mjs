import { createServer } from "node:http";
import { readFileSync, writeFileSync, readdirSync, statSync } from "node:fs";
import { resolve, join, extname, sep } from "node:path";
import { gzipSync } from "node:zlib";
import { createHash } from "node:crypto";
import { cpus, platform, arch } from "node:os";
import { chromium } from "../../../impl/ts/node_modules/playwright-core/index.mjs";
import { attachBrowserProfilers } from "./cpu-browser.mjs";
import { runCases } from "./runner.mjs";
const out = resolve(process.argv[2]);
const site = join(out, "site");
const root = resolve(process.env.JED_PROFILE_ROOT);
const settings = Object.fromEntries(Object.entries(process.env).filter(([k]) => k.startsWith("JED_PROFILE_") || k.startsWith("JED_ASYNC_HEAVY_")));
const server = createServer((request, response) => {
  response.setHeader("Cache-Control", "no-store");
  try {
    const pathname = decodeURIComponent(new URL(request.url, "http://localhost").pathname);
    const file = resolve(site, `.${pathname === "/" ? "/index.html" : pathname}`);
    if (!file.startsWith(site + sep) || !statSync(file).isFile()) throw new Error("not found");
    response.setHeader("Content-Type", extname(file) === ".html" ? "text/html" : "text/javascript");
    response.end(readFileSync(file));
  } catch { response.writeHead(404); response.end(); }
});
await new Promise((done, reject) => { server.once("error", reject); server.listen(0, "127.0.0.1", done); });
let browser, profiler;
try {
  browser = await chromium.launch({ headless: true });
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  const errors = [];
  page.on("pageerror", (error) => errors.push(String(error)));
  await page.goto(`http://127.0.0.1:${server.address().port}`);
  await page.bringToFront();
  await page.waitForFunction(() => typeof window.prepareProfileSuite === "function");
  const metadata = await page.evaluate((s) => window.prepareProfileSuite(s), settings);
  if (settings.JED_PROFILE_MODE === "cpu") profiler = await attachBrowserProfilers(browser, page);
  const result = await runCases({ metadata,
    warm: (id, warmups) => page.evaluate(([i, n]) => window.warmProfileCase(i, n), [id, warmups]),
    run: (...args) => page.evaluate((a) => window.runProfileCase(...a), args),
    startCpu: () => profiler.start(), stopCpu: () => profiler.stop(),
    async saveCpu(label, profiles) {
      for (const [thread, profile] of Object.entries(profiles))
        writeFileSync(join(out, `${label}-${thread}.cpuprofile.gz`), gzipSync(JSON.stringify(profile)));
    },
    settings, progress: console.log,
  });
  await page.evaluate(() => window.closeProfileSuite());
  if (errors.length) throw new Error(errors.join("\n"));
  const cdp = await page.context().newCDPSession(page);
  const version = await cdp.send("Browser.getVersion");
  await cdp.detach();
  const artifacts = Object.fromEntries(readdirSync(site, { recursive: true }).filter((p) => statSync(join(site, p)).isFile()).sort().map((p) => {
    const b = readFileSync(join(site, p)); return [p, { bytes: b.length, sha256: createHash("sha256").update(b).digest("hex") }];
  }));
  writeFileSync(join(out, "results.json"), JSON.stringify({ recordedAt: new Date().toISOString(),
    host: { host: "Chromium", browser: browser.version(), v8: version.jsVersion, platform: platform(), arch: arch(),
      cpu: cpus()[0].model, affinity: readFileSync("/proc/self/status", "utf8").match(/^Cpus_allowed_list:\s*(.+)$/m)?.[1],
      sourceManifest: JSON.parse(readFileSync(join(root, "profile-manifest.json"), "utf8")),
      build: { mode: "production", minified: true, sourcemaps: true }, artifacts,
      cpuProfiler: profiler?.metadata, signalProvided: false, storage: "memory", pageSize: 8192 }, ...result }, null, 2) + "\n");
} finally {
  await profiler?.close();
  await browser?.close();
  await new Promise((done) => server.close(done));
}
