// Playwright config for the jed website's interactive-feature tests (the plan's "browser testing is
// first-class" contract). Like impl/ts, Playwright is a DEV-ONLY tool (CLAUDE.md §14): it drives
// headless Chromium against the BUILT static site (vite preview) — so the suite also catches
// prerender / base-path / worker-404 regressions, not just component behavior. Run with
// `npm run test:browser`; needs the Chromium binary (already in PLAYWRIGHT_BROWSERS_PATH here).

import { defineConfig, devices } from '@playwright/test';

const PORT = 4173;

export default defineConfig({
  testDir: 'e2e',
  // OPFS files are origin-scoped; keep specs from racing the exclusive handle.
  fullyParallel: false,
  workers: 1,
  use: { baseURL: `http://localhost:${PORT}` },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
  // Build (incl. the Pagefind index), then serve the `build/` artifact WHOLESALE via a tiny static
  // server (e2e/serve.mjs) — faithful to GitHub Pages, and unlike `vite preview` it serves the
  // post-build Pagefind files, so search is testable. reuseExistingServer keeps local iteration
  // fast; CI always builds fresh.
  webServer: {
    command: `npm run build && PORT=${PORT} node e2e/serve.mjs`,
    url: `http://localhost:${PORT}`,
    reuseExistingServer: !process.env.CI,
    timeout: 120_000
  }
});
