# jed website

The static [SvelteKit](https://svelte.dev/docs/kit) + [Tailwind](https://tailwindcss.com) site for
jed: a home page with a **live in-browser database**, **documentation** (an embedding-API section
with a Rust/Go/TS language switcher and a SQL section with runnable examples), and a full
**database tool** that reads and writes local `.jed` files via OPFS.

A non-core tooling module (CLAUDE.md §6/§14): its dependencies never touch an engine core. It runs
the **TypeScript core** (`impl/ts`) in a browser Web Worker — in-memory for the docs/home demos, and
over an `OpfsBlockStore` for the tool's persistent databases.

## Develop

```sh
npm install
npm run dev            # dev server
npm run build          # static build into ./build (+ Pagefind search index)
npm run preview        # SvelteKit preview (note: does NOT serve the post-build Pagefind files)
npm run test:browser   # build + serve build/ wholesale + Playwright interactive-feature tests
npm run check          # svelte-check (website + browser-consumed TS core modules)
```

The deploy target is a GitHub Pages project page (`.github/workflows/web-deploy.yml`), which gates
on `test:browser` and builds with `BASE_PATH=/<repo>`.

## How it's wired

- **Engine bridge** — `src/lib/jed/`: a site-owned Web Worker (`worker.ts`) holding many databases
  keyed by id (in-memory + OPFS), and an async client (`session.ts`, the `JedSession`/`JedDb` API)
  that every page uses. The `$jed` Vite alias points at the node-clean core modules in `impl/ts/src`
  (never `lib.ts`/`file.ts`, which pull `node:fs`). One shared worker loads the engine once, off the
  main thread; demo databases carry a cost ceiling so a runaway query trips `54P01` instead of
  hanging.
- **Docs** are Markdown (`src/routes/docs/**/+page.md`, via MDsveX) with inline components:
  `<CodeTabs topic="…">` (API examples, switched by language) and `<LiveSql>` (runnable SQL panels).
- **Per-language examples** are real source files in `examples/<topic>/{rust.rs,go.go,ts.ts}`,
  syntax-highlighted with Shiki **at build time** (`plugins/vite-examples.ts` → `virtual:jed-examples`)
  so no highlighter JS ships to the client.
- **Reference pages** (`docs/reference/*`) are generated from the spec's canonical TOML
  (`plugins/vite-spec.ts` → `virtual:jed-spec`), so they can't drift from the engine.
- **Search** is [Pagefind](https://pagefind.app), indexed over the built HTML.

## Adding docs (and the CLAUDE.md §10 obligation)

When a user-facing SQL feature or the host API changes, update the matching page here in the same
change: an `api/` page (and its `examples/<topic>` sources) or a `sql/` page (and its `<LiveSql>`),
and add to `src/lib/content/docs-nav.ts`. The type/function/error reference regenerates from the
spec automatically. Run `npm run test:browser` when touching the bridge or a documented behavior.
