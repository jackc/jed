import { defineMDSveXConfig as defineConfig } from 'mdsvex';

/**
 * MDsveX config for the docs (CLAUDE.md §6). Markdown `.md`/`.svx` become Svelte components, so a
 * doc page mixes prose with the bespoke interactive widgets (CodeTabs / LiveSql).
 *
 * `layout` wraps every doc in the shared docs chrome (TOC, prose container). Build-time Shiki
 * syntax highlighting is wired in Phase 5 via the `highlight` hook (all language variants rendered
 * at build time, the language selector toggles which is shown — zero highlighter JS shipped).
 */
const config = defineConfig({
	extensions: ['.svx', '.md'],
	smartypants: { dashes: 'oldschool' }
});

export default config;
