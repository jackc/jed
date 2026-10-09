// Playwright config for the gated real-browser OPFS test (spec/design/hosts.md §5). Playwright is a
// DEV-ONLY test dependency (CLAUDE.md §14): it drives headless Chromium against the Vite-served demo to
// verify the OPFS host end to end (real FileSystemSyncAccessHandle, in a real Worker). NOT part of the
// node unit suite (`npm run test`) or `mise run ci` — run explicitly with `npm run test:browser`, which
// needs the Chromium binary (`npx playwright install chromium`, a heavy one-time download).

import { defineConfig, devices } from "@playwright/test";

// Must match impl/ts/vite.config.ts — this worktree's allocated port, or the historical default.
const PORT = Number(process.env.TS_BROWSER_PORT) || 5173;

export default defineConfig({
  testDir: "e2e",
  fullyParallel: false, // OPFS files are origin-scoped; keep runs from racing the exclusive handle
  use: { baseURL: `http://localhost:${PORT}` },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  // Vite serves the demo page + the engine Worker; Playwright starts it and waits for the port.
  webServer: {
    command: "npm run dev:browser",
    url: `http://localhost:${PORT}`,
    reuseExistingServer: true,
    timeout: 60_000,
  },
});
