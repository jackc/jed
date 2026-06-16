// Site-wide rendering policy: the whole site is static (CLAUDE.md §6). Every route is prerendered
// to HTML at build time by adapter-static; there is no server. All engine work (the in-browser
// jed database, OPFS, Web Workers) is therefore strictly client-only — it runs inside onMount /
// dynamic imports, never during this prerender pass.
export const prerender = true;
// Trailing-slash 'always' keeps GitHub Pages deep-link reloads working (it serves <route>/index.html).
export const trailingSlash = 'always';
