# jed website

The static [SvelteKit](https://svelte.dev/docs/kit) + [Tailwind](https://tailwindcss.com) site for
jed: a home page with a **live in-browser database**, **documentation** (an embedding-API section
with a Rust/Go/TS language switcher and a SQL section with runnable examples), and a full
**database tool** that reads and writes local `.jed` files via OPFS.

A non-core tooling module (CLAUDE.md §6/§14): its dependencies never touch an engine core. It runs
the **TypeScript core** (`impl/ts`) through its async browser runtime, with a dedicated execution
worker per database — in-memory for the docs/home demos, and over an `OpfsBlockStore` for the
tool's persistent databases.

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
on `test:browser` and builds with `BASE_PATH=/<repo>`. Deployment runs when a GitHub Release is
published or the workflow is dispatched manually. A push to `master` does not publish the site.

## How it's wired

- **Engine bridge** — `src/lib/jed/session.ts` exposes the site's `JedDb` display facade over the
  shared async browser runtime. Each database owns an execution worker; reset and close release
  it. Queries consume typed async cursors and render values for the UI. The `$jed` Vite alias
  points at `impl/ts/src`; the bridge imports `browser/async.ts` and browser-safe value/error
  helpers. Demo databases carry a cost ceiling so a runaway query trips `54P01`. The separate
  site-owned `src/lib/jed/worker.ts` only lists, imports, exports, and deletes closed OPFS files.
- **Docs** are Markdown (`src/routes/docs/**/+page.md`, via MDsveX) with inline components:
  `<CodeTabs topic="…">` displays API source examples switched by language; those tabs do not run
  code in the page. `<LiveSql>` opens an independent database and runs editable SQL in its worker.
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
spec automatically. Run `npm run test:browser` when touching the bridge or a documented behavior;
the repo's `mise run ci` also runs it (the `e2e:web` step).

The website browser suite checks the built site's live SQL panels, structured errors, reset,
language tabs, and OPFS tool. Displayed TypeScript examples also run against the public Node API
in `impl/ts/tests/website_examples.test.ts`, through `mise run unit:ts` and `mise run ci`.
Actual browser custom-worker registration is exercised by `impl/ts/e2e/host_functions.spec.ts`
through `mise run e2e:ts`; the website's host-function API page displays the integration code.
