import { fileURLToPath } from 'node:url';
import tailwindcss from '@tailwindcss/vite';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';
import { jedExamples } from './plugins/vite-examples.ts';
import { jedSpec } from './plugins/vite-spec.ts';

// Vite config for the jed website. SvelteKit config (adapter, mdsvex preprocess, $jed alias,
// base path) lives in svelte.config.js; this file adds:
//   - the Tailwind v4 plugin;
//   - jedExamples — build-time Shiki highlighting of the per-language API examples, exposed as the
//     virtual module `virtual:jed-examples` (so Shiki never reaches the client bundle);
//   - `worker.format: 'es'` so `new Worker(new URL('./worker.ts', import.meta.url), {type:'module'})`
//     emits an ES-module worker chunk (the proven impl/ts pattern);
//   - `server.fs.allow: ['..']` so the dev server may serve the TS core source ($jed → ../impl/ts/src).
// Example sources live OUTSIDE src/ (pure build-time data read by the plugin, not app code that
// svelte-check type-checks): web/examples/<topic>/{rust.rs, go.go, ts.ts}.
const examplesDir = fileURLToPath(new URL('./examples', import.meta.url));
// The canonical spec data tables live at the repo root (../spec), outside /web.
const specDir = fileURLToPath(new URL('../spec', import.meta.url));

export default defineConfig({
  plugins: [tailwindcss(), jedExamples(examplesDir), jedSpec(specDir), sveltekit()],
  worker: { format: 'es' },
  // `host: true` binds to 0.0.0.0 so the dev/preview server is reachable through a devcontainer's
  // (or any container's) forwarded port from the host — the default localhost-only bind is not.
  // strictPort: fail loudly if the port is taken rather than silently moving to 5174 (which the
  // devcontainer wouldn't be forwarding), so the forwarded port is deterministic.
  server: { host: true, port: 5173, strictPort: true, fs: { allow: ['..'] } },
  preview: { host: true, port: 4173, strictPort: true }
});
