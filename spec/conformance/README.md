# spec/conformance/ — the contract between implementations

This is the spine of the project (CLAUDE.md §7). A feature is implemented as "make these
corpus entries pass"; the corpus *is* the contract, not an afterthought.

- **Format: sqllogictest-style** — plain-text, declarative (`statement ok`,
  `statement error <pattern>`, `query <coltypes> <sortmode>` + expected rows, with hashing
  for large result sets). Invented by SQLite to run identical tests across independent
  engines — our exact problem.
- **Bootstrap via differential testing** — use real PostgreSQL/SQLite as oracles over our
  supported subset; override and document where we intentionally diverge.
- **Tier the corpus** — each implementation declares a conformance level so one core can
  run ahead of another without the whole suite reading as broken. `skipif`/`onlyif`
  handle per-engine quirks; tiers handle different speeds.

Each implementation under [../../impl/](../../impl/) ships a thin harness that runs this
corpus.

> Status: empty. Format + first tier defined at CLAUDE.md §11 step 3.
