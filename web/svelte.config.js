import adapter from '@sveltejs/adapter-static';
import { vitePreprocess } from '@sveltejs/vite-plugin-svelte';
import { mdsvex } from 'mdsvex';
import mdsvexConfig from './mdsvex.config.js';

/**
 * SvelteKit config for the jed website (CLAUDE.md §6). Entirely static: adapter-static
 * prerenders every route (see src/routes/+layout.ts), so the site needs no server.
 *
 * - `extensions` + the mdsvex preprocessor make `.md`/`.svx` files first-class routes, so the
 *   docs are authored in Markdown with inline Svelte components (the bespoke language selector +
 *   live-SQL widgets) — the "MDsveX, not a turnkey docs framework" decision.
 * - `paths.base` comes from BASE_PATH so a GitHub Pages project page (served from /<repo>) works;
 *   it is '' in dev and for a root/custom-domain deploy.
 * - `$jed` aliases the TS source (impl/ts/src). The UI uses browser/async.ts plus browser-safe
 *   value/error helpers; SQL execution stays in the runtime's dedicated database workers.
 *   Node host modules must not enter the browser bundle.
 */
const config = {
  extensions: ['.svelte', '.svx', '.md'],
  preprocess: [vitePreprocess(), mdsvex(mdsvexConfig)],
  kit: {
    adapter: adapter({ strict: true }),
    paths: { base: process.env.BASE_PATH || '' },
    alias: {
      $jed: '../impl/ts/src'
    }
  }
};

export default config;
