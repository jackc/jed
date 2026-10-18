// Playwright config for the jed website's interactive-feature tests (the plan's "browser testing is
// first-class" contract). Like impl/ts, Playwright is a DEV-ONLY tool (CLAUDE.md §14): it drives
// headless Chromium against the BUILT static site (vite preview) — so the suite also catches
// prerender / base-path / worker-404 regressions, not just component behavior. Run with
// `npm run test:browser`, or `mise run e2e:web` (a `mise run ci` step); needs the Chromium binary
// (`mise run dev:browsers`; already in PLAYWRIGHT_BROWSERS_PATH in the devcontainer).

import { defineConfig, devices } from '@playwright/test';

// This worktree's allocated preview port (port-tamer.toml -> .dev/ports.env), or the historical
// default when there is no allocation.
const PORT = Number(process.env.WEB_PREVIEW_PORT) || 4173;

export default defineConfig({
  testDir: 'e2e',
  // OPFS files are origin-scoped; keep specs from racing the exclusive handle.
  fullyParallel: false,
  workers: 1,
  // Every live panel on a page shares one engine Web Worker and autoruns in turn, so an assertion on
  // a panel far down the page (the select page has 28, types 37) waits for the engine to load and
  // every earlier panel to run: up to ~3 s on an idle machine, past Playwright's 5 s default when
  // the gate loads the machine. These bound how long a correct page may take; a wrong answer still
  // fails, just later. No retries: a flake stays visible.
  timeout: 60_000,
  expect: { timeout: 20_000 },
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
