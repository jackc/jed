# spec/conformance/ — the contract between implementations

This is the spine of the project (CLAUDE.md §7). A feature is implemented as "make these
corpus entries pass"; the corpus *is* the contract, not an afterthought.

**The format, error-matching, three-axis taxonomy, and determinism rules are specified in
[../design/conformance.md](../design/conformance.md). Read that first.**

- **Format: sqllogictest-style** — plain-text, declarative (`statement ok`,
  `statement error <sqlstate>`, `query <coltypes> <sortmode>` + expected rows, with hashing
  for large result sets). Invented by SQLite to run identical tests across independent
  engines — our exact problem.
- **Structured-error matching** — `statement error <sqlstate>` matches on the error's
  SQLSTATE code (from [../errors/registry.toml](../errors/registry.toml)), never on prose.
- **Bootstrap via differential testing** — predominantly hand-authored, with two Phase-8 tools
  (see [../design/conformance.md](../design/conformance.md) §5/§8): **oracle-import**
  (`rake corpus:import/check[file]`) fills/re-checks a `.test`'s expected output from the live
  `db` PostgreSQL service — never the source checkout, so no §12 trip — and records intentional
  jed-vs-PG divergences in [oracle_overrides.toml](oracle_overrides.toml); and the
  **metamorphic generator** (`rake corpus:norec_sweep`) generates self-checking NoREC + TLP
  tests run on all three cores, with an automatic test reducer (`rake corpus:reduce`) to
  minimize any failure. The *source* checkouts and bulk imports stay deferred and
  user-initiated (never auto-run — CLAUDE.md §12).
- **Three-axis taxonomy** — **suites** (this directory tree) organize tests by feature
  area; **capabilities** (dotted flags an impl declares + a test `# requires:`) gate which
  tests run; **profiles** (named capability bundles) are the conformance levels an impl
  targets. A test runs for an impl iff the impl declares every capability the test requires,
  so one core can run ahead of another without the suite reading as broken.

## Layout

| Path | Contents |
|---|---|
| [manifest.toml](manifest.toml) | Capability + profile definitions (data). |
| [verify.rb](verify.rb) | Taxonomy checker (run via `rake verify`): validates manifest ↔ corpus coherence. |
| [oracle_overrides.toml](oracle_overrides.toml) | Machine-checked ledger of intentional jed-vs-PostgreSQL divergences (consumed by `corpus:check`). |
| [../../scripts/oracle_import.rb](../../scripts/oracle_import.rb) | Oracle-import harness — fills/checks expected output from the live `db` (`rake corpus:import/check`). |
| [../../scripts/norec_gen.rb](../../scripts/norec_gen.rb) | Metamorphic NoREC + TLP generator + sweep (`rake corpus:norec[_sweep]`); writes a transient `suites/metamorphic/` tier it cleans up. |
| [suites/](suites/) | 15 feature-area suites: `aggregates` `cast` `compare` `ddl` `dml` `expr` `joins` `mutation` `null` `query` `resource` `setops` `subquery` `transactions` `types`. |

Each implementation under [../../impl/](../../impl/) ships a thin harness that reads the
manifest, runs each `.test` whose `# requires:` capabilities it declares, and reports the
profiles it meets. All three cores (Rust, Go, TS) ship this harness today.

> Status: format + taxonomy + corpus authored across all 15 feature-area suites; the manifest
> defines 100 capabilities and 18 profiles. All three cores pass the corpus byte- and
> cost-identically. Phase-8 tooling landed: oracle-import (`corpus:check/import`) + override
> ledger, the metamorphic NoREC + TLP sweep (`corpus:norec_sweep`, in `rake ci`), and the
> automatic reducer. See [../design/conformance.md](../design/conformance.md) §5/§8.
