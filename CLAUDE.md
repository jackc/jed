# jed — agent instructions

jed is an **embedded SQL database**: SQLite's footprint (a library, one database = one file),
PostgreSQL's behavior, and a strict static type system.

This file holds instructions for working here and the few commitments every change must respect.
Design decisions live in the spec: **[spec/design/README.md](spec/design/README.md)** indexes every
subsystem doc, and [TODO.md](TODO.md) is the roadmap. Do not restate a design decision here — link
to its doc. When a commitment below changes, update this file in the same change. Section numbers
are cited across the repo (`CLAUDE.md §8`), so keep them stable.

---

## 1. PostgreSQL is the behavioral default

When a decision has an option that matches PostgreSQL and no overriding reason against it, take
it. PostgreSQL is **not a compatibility target**: we choose which surface to implement, and for
that surface behavior tracks PG. An overriding reason is a genuine engineering tradeoff (simplicity,
determinism, the type system, single-writer, safety/cost bounds), documented where it is taken.
Record every deliberate divergence in the relevant spec doc and in the corpus override ledger.

## 2. The spec is the project; cores are in lockstep

There is **no reference implementation**. The language-neutral spec and conformance corpus are
canonical; every core is a downstream consumer. Never let one core's code define behavior — change
the spec, then the cores.

- **The differential set — Rust, Go, TypeScript (`impl/rust`, `impl/go`, `impl/ts`)** — must move
  together: a feature lands in all three, passing the same corpus and producing byte-identical files.
- **Go is pure Go: no cgo, no FFI.**
- Other languages (wrapped or native) and the rationale: [cores.md](spec/design/cores.md).
- **Frozen experiments** (`impl/ruby-native`, `impl/swift-native`, `impl/csharp-native`,
  `impl/java-native`) are opt-in: excluded from normal tests, benchmarks, CI and the website, and
  not updated by new features. **Touch one only when explicitly asked.** Each has its own
  `spec/design/*-native-experiment.md`. Never cite their measurements as general parity.

## 3. Load-bearing scope limits (do not quietly relax)

- **Single writer, not MVCC** — one committed version plus one writer's pending set
  ([transactions.md](spec/design/transactions.md)).
- **No users, roles or auth in the database** — authorization is a host-supplied session envelope
  ([session.md](spec/design/session.md)).

## 4. The type system is the product

Types are specified as data before code ([types.md](spec/design/types.md), `spec/types/`). Column
types are strict and static; values are never silently reinterpreted. New types and container
rules follow the existing docs (composite, array, ranges, json, decimal, float).

## 5. Data over code

Anything mechanical and data-shaped — type/compare matrices, the function catalog, error codes,
cost schedule, estimator facts — is **shared data** in `spec/`, codegen'd or cross-checked per core
([codegen.md](spec/design/codegen.md)). **Never codegen** the parser, planner, executor, storage
layer, or evaluator; those are hand-written per core. Don't hand-write per core what can be shared
data.

## 6. Repository layout

`spec/` is canonical (design docs, grammar, types, functions, errors, file format and key-encoding
fixtures, conformance corpus). `impl/` holds the cores. `cli/`, `bench/`, `migrate/` and `web/`
are non-core consumers whose dependencies never touch a core manifest. Tasks: `mise.toml` and
`mise-tasks/` (§10). Long-running dev services: `process-compose.yaml` (`mise run dev`).

## 7. Testing

- **The corpus is the contract** ([conformance.md](spec/design/conformance.md)). A feature is a
  vertical slice — one SQL construct parsed, planned, executed and tested — that makes corpus
  entries pass on every core.
- **Tests go in the corpus by default.** Write a per-core unit test **only** for what the corpus
  cannot express: deliberate PG divergences, catalog/host introspection, byte-level checks, cost
  values no cost suite pins, host-API surface, internal invariants. When unsure, write the corpus
  entry. (Model: per-core `foreign_key` tests vs. `ddl/foreign_key.test`.)
- On the PG-comparable surface, **oracle-check** rows (`mise run corpus:check <file>`) and ledger
  any divergence. **PostgreSQL is the only result oracle — never SQLite.**
- A query optimization adds a **NoREC** metamorphic relation.
- Concurrency behavior belongs in the shared schedule/process corpora, not mirrored per-core tests
  ([concurrency-testing.md](spec/design/concurrency-testing.md)).
- When a storage feature lands (page type, commit path, host operation, setting), **grow the
  simulation generator and checkers** with it ([simulation-testing.md](spec/design/simulation-testing.md)).
- Gate before merging: `mise run ci`.

## 8. Cross-core determinism

Results, values, types, errors, selected plan, estimates and cost must be **byte-identical across
cores** and reproducible ([determinism.md](spec/design/determinism.md)). Row order is defined only
by `ORDER BY`. No hashmap iteration order, wall clock, or library/locale behavior may leak.
Relaxations are default-deny, listed in `spec/conformance/determinism_exceptions.toml`. On-disk
bytes and key encodings are pinned by shared fixtures ([encoding.md](spec/design/encoding.md),
[format.md](spec/fileformat/format.md)). Decide any new divergence hotspot in the spec before coding.

## 9. Storage

Design: [storage.md](spec/design/storage.md) and the docs it links; bytes:
[format.md](spec/fileformat/format.md). Two standing rules for all code:

- **Never harden a full-residency assumption above the storage seam** — no "read the whole file",
  no operator that requires its whole input or output in memory. Larger-than-RAM must stay possible.
- A **format change** bumps the format version, updates `format.md`, the goldens, and every core
  in the same change.

## 10. How to work here

- **Spec first.** Read the subsystem's design doc and corpus before changing it; update the doc in
  the same change as the code.
- **Consult [TODO.md](TODO.md)** when planning a feature; update it when the plan moves.
- **Keep `/web` in sync**: a change to user-facing SQL or the host API updates the matching
  `web/src/routes/docs/` page and `web/examples/<topic>/` in the same change. Run
  `mise run e2e:web` when touching `web/src/lib/jed/` or documented behavior.
- **Benchmarks** ([benchmarks.md](spec/design/benchmarks.md)) are outside CI. A perf-relevant
  feature adds a benchmark; a perf-sensitive change reports before/after numbers
  (`mise run bench:diff`). Benchmarks never overlap other work.
- **Boring, explicit code** — no deep generics or macro magic in Rust, no over-interfacing in Go.
- **Tasks: `mise run <task>` is the only entry point** (no Rakefile, no Make). Prefer Ruby over
  bash. A composite or straight-line task is a shell `run` in `mise.toml`; a task with real logic
  is an executable Ruby script in `mise-tasks/` named by its path (helpers in
  `scripts/lib/tasks.rb`, larger tools in `scripts/`). `mise run <task> --help` shows arguments.
  mise runs `depends` **in parallel**, so sequence steps as `{ task = … }` entries in `run`.
- **Worktrees share one object store.** Claude and Codex sessions each work in a git worktree of
  one checkout; a commit is visible to all of them immediately, so committing (even WIP) is the
  hand-off. **Never use bare `git stash` / `stash pop`.** `master` lives in the main checkout:
  fast-forward there (`git -C <main> merge --ff-only <sha>`) after checking it is clean. Verify
  another session's claims against git, not its prose.
- **`origin` is the public GitHub repo** — pushing publishes. **Push only when the user asks.**
  Merge to `master` only when green. **`master` is strictly linear**: rebase then fast-forward,
  squash, or cherry-pick; never a merge commit; never `git pull` without `--ff-only`/`--rebase`.
- Memory notes about landed work say whether it is "pushed" or "local only, NOT pushed".

## 11. Roadmap

Forward work is in [TODO.md](TODO.md). Keep each step independently testable — no point where
progress blocks on one giant subsystem.

## 12. Reference sources and the PostgreSQL oracle

`references/` (gitignored) holds source checkouts of postgres, sqlite, duckdb, bbolt and
sqllogictest-rs for reading. CockroachDB is excluded (BSL).

> **Never provision references or start any large download or heavy operation on your own
> initiative** — work without it or ask the user. Exception: if every `<name>.git` mirror already
> exists with refs at the machine-level mirror path (`REFERENCES_MIRROR_DIR`, default
> `${XDG_DATA_HOME:-~/.local/share}/jed/references/` or `/persist/shared/references/` in a
> devcontainer), `mise run references:setup` only adds local worktrees and you may run it.
> `references:update` is never covered.

The live oracle is this checkout's own PostgreSQL cluster (`mise run db:init`, `mise run dev`),
reached over a Unix socket configured by `.dev/ports.env` / `.dev/derived.env`, which mise loads.
**Never set `PGHOST` and `PGPORT` independently.** Corpus probes use the `jed_oracle` database
(`mise run oracle:setup`, `psql -d jed_oracle`); its configuration is declared in
`spec/conformance/oracle_profile.toml` (`mise run oracle:status`).

## 13. Untrusted SQL must be safe to run

A standing requirement on the engine and its built-in surface
([session.md](spec/design/session.md), [cost.md](spec/design/cost.md),
[memory.md](spec/design/memory.md)):

- **Memory safety** — any `unsafe`, cgo or FFI path must be justified against this and §14.
- **No dangerous built-ins** — every built-in function is pure: no filesystem, network, process,
  environment, or clock access except the host-injected entropy/clock seam. Never add one that
  isn't.
- **Bounded resources** — new execution work is metered by the deterministic cost meter (identical
  in every core) and charged to the memory accounts. These limits are guardrails, not a
  whole-engine heap cap; never claim otherwise.

These guarantees bind **jed's own surface only**. A host extension may relax any of them and owns
the consequence; don't argue host extensions down on core guarantees
([extensibility.md](spec/design/extensibility.md) §2).

## 14. Third-party dependencies

The parser, planner, executor, storage, type system and evaluator are written from scratch, never
delegated to a library. A dependency at the edges is allowed only if (1) every core can match it
byte-for-byte, (2) a per-language library is *significantly* faster with identical output, or (3)
it is cryptography — never roll our own.

**Always get explicit human confirmation before adding any dependency**: name the clause and wait
for a yes. Dependencies must not break memory safety, leak nondeterminism, or make cores diverge;
the Go core stays free of cgo and external FFI. `bench/` modules are not cores, but new bench
dependencies still need confirmation.

Approved dependencies, with exact pins and scope, are recorded in
[dependencies.md](spec/design/dependencies.md); record any new approval there.
