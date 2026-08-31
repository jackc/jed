// Vite config for the Browser/OPFS host demo + e2e harness (spec/design/hosts.md §5). Vite is a
// DEV-ONLY tool (CLAUDE.md §14, the bench-harness-style test-tooling carve-out): it builds/serves the
// browser modules + the engine Worker for the browser, and never touches the engine cores, the
// conformance corpus, the byte contracts, or how the TS core runs under Node (zero-build type-stripping
// is unchanged). Root is the demo dir; the Worker is emitted as an ES module chunk so
// `new Worker(new URL("./worker.ts", import.meta.url), { type: "module" })` works in dev and build.

import { defineConfig } from "vite";

// This worktree's allocated port for the browser-host demo (port-tamer.toml -> .dev/ports.env).
// Distinct from the website's dev port so the two e2e suites cannot collide; falls back to the
// historical 5173 when there is no allocation.
const PORT = Number(process.env.TS_BROWSER_PORT) || 5173;

export default defineConfig({
  root: "browser",
  worker: { format: "es" },
  server: { port: PORT, strictPort: true },
  preview: { port: PORT, strictPort: true },
  build: { outDir: "../dist-browser", emptyOutDir: true, target: "esnext" },
});
