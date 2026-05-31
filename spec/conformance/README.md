# spec/conformance/ — the contract between implementations

This is the spine of the project (CLAUDE.md §7). A feature is implemented as "make these
corpus entries pass"; the corpus *is* the contract, not an afterthought.

**The format, error-matching, tier/flag system, and determinism rules are specified in
[../design/conformance.md](../design/conformance.md). Read that first.**

- **Format: sqllogictest-style** — plain-text, declarative (`statement ok`,
  `statement error <sqlstate>`, `query <coltypes> <sortmode>` + expected rows, with hashing
  for large result sets). Invented by SQLite to run identical tests across independent
  engines — our exact problem.
- **Structured-error matching** — `statement error <sqlstate>` matches on the error's
  SQLSTATE code (from [../errors/registry.toml](../errors/registry.toml)), never on prose.
- **Bootstrap via differential testing** — hand-authored for now; PostgreSQL/SQLite oracles
  are a deferred, user-initiated option (never auto-run — CLAUDE.md §12).
- **Tier the corpus** — each implementation declares the capability flags it supports; a
  tier runs only if all its required flags are present, so one core can run ahead of another
  without the whole suite reading as broken.

## Layout

| Path | Contents |
|---|---|
| [manifest.toml](manifest.toml) | Capability flags + tier definitions (data). |
| [tier1_core/](tier1_core/) | CREATE/INSERT/SELECT/`WHERE pk =`/`ORDER BY`/NULL/overflow — the §11 step-5 milestone. |
| [tier2_casts/](tier2_casts/) | Explicit `CAST` narrowing + overflow trap. |
| [tier3_comparison/](tier3_comparison/) | Cross-type comparison via the promotion tower. |

Each implementation under [../../impl/](../../impl/) ships a thin harness that reads the
manifest, filters tiers to its declared flags, and runs this corpus. Harnesses arrive with
the first vertical slice (CLAUDE.md §11 step 5).

> Status: format + first three tiers authored (integers only). Harnesses land at step 5.
