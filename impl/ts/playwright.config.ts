// Playwright config for the gated real-browser OPFS test (spec/design/hosts.md §5). Playwright is a
// DEV-ONLY test dependency (CLAUDE.md §14): it drives headless Chromium against the Vite-served demo to
// verify the OPFS host end to end (real FileSystemSyncAccessHandle, in a real Worker). NOT part of the
// node unit suite (`npm run test`); `mise run ci` runs it as the `e2e:ts` step (`mise run e2e:ts`, or
// `npm run test:browser` here). Needs the Chromium binary (`mise run dev:browsers`, a one-time download).

import { defineConfig, devices } from "@playwright/test";

// Must match impl/ts/vite.config.ts — this worktree's allocated port, or the historical default.
const PORT = Number(process.env.TS_BROWSER_PORT) || 5173;
const BUILT = process.env.JED_TS_BROWSER_BUILT === "1";

export default defineConfig({
  testDir: "e2e",
  fullyParallel: false, // OPFS files are origin-scoped; keep runs from racing the exclusive handle
  use: { baseURL: `http://localhost:${PORT}` },
  projects: [{ name: "chromium", use: { ...devices["Desktop Chrome"] } }],
  // The built mode exercises package exports and application-owned worker bundling in emitted JS.
  // Use JED_TS_BROWSER_BUILT=1 mise run e2e:ts to run the same contracts against production output.
  webServer: {
    command: BUILT ? "npm run build:browser && npx vite preview" : "npm run dev:browser",
    url: `http://localhost:${PORT}`,
    reuseExistingServer: !BUILT,
    timeout: 60_000,
  },
});
