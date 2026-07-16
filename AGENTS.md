# Agent Instructions

This repository is **jed**, an embedded SQL database: SQLite's footprint,
PostgreSQL's behavior, and a real strict static type system.

`CLAUDE.md` is the canonical project brief and architectural record. Read it
before making design or implementation changes. This file is the agent-facing
working summary derived from it; if the standing decisions change, update both
files in the same change.

## North Star

- Build an embeddable, single-file SQL database library, not a server.
- Match PostgreSQL behavior for the SQL surface jed implements unless there is
  a documented overriding reason.
- PostgreSQL is the behavioral default, not a compatibility target. jed does
  not owe wire-protocol compatibility, `pg_catalog` fidelity, the whole PG
  coercion lattice, or arbitrary SQL coverage.
- The strict, static type system is the product. Design type behavior as spec
  data before executor code.

## Non-Negotiable Architecture

- The language-neutral spec and conformance corpus are canonical. No
  implementation is the reference implementation.
- Core implementations are native and evolve in lockstep. The differential set
  is Rust, Go, and TypeScript.
- Go core is pure Go: no cgo, no FFI.
- Rust wrappers are acceptable for later language packages when that gives the
  best user experience, but wrappers echo Rust and are not independent
  conformance voices.
- SQL is the primary access path and every built-in capability must be reachable
  through SQL, while storage must keep room for non-SQL access paths later.

## Scope Simplifications

- Single writer. Readers observe the last committed state and block only during
  the short commit window.
- This is not MVCC: there is one committed version plus one writer pending set.
- No users, roles, RBAC, or in-database auth. Host session capabilities live
  above the engine.
- Keep these simplifications load-bearing unless the spec explicitly revises
  them.

## Spec First

- Put subsystem decisions in `spec/design/` and shared mechanical facts in
  language-neutral data.
- Shared data belongs in the spec, not duplicated per language:
  comparison/coercion/promotion matrices, function/operator catalog, error-code
  registry, range/type facts, byte fixtures, and similar tables.
- Use codegen only for large mechanical surfaces such as generated stubs from
  shared catalogs.
- Do not codegen parsers, planners, executors, storage layers, expression
  evaluators, or recursive open-type behavior.
- Record deliberate PostgreSQL divergences in the relevant spec doc or ledger.
- Consult `TODO.md` when planning new feature work and update it when the plan
  moves.

## Conformance Is The Contract

- Implement features as vertical slices driven by shared corpus entries.
- Prefer conformance tests over per-core unit tests. A corpus entry tests every
  core at once.
- Add per-core unit tests only for behavior the corpus cannot express:
  byte-level fixtures, host API surfaces, internal invariants, cost-meter values,
  catalog/host introspection, or deliberate PG divergences.
- For PostgreSQL-comparable behavior, use PostgreSQL as the oracle and run the
  oracle check when relevant.
- SQLite is not a semantic oracle. It is the deployment-model inspiration and
  the origin of sqllogictest.
- Concurrency behavior belongs in the shared concurrency corpus, not mirrored
  hand tests.

## Determinism

- Results must be deterministic in values, types, errors, costs, and byte
  encodings.
- Row order is defined only when `ORDER BY` is present. Without `ORDER BY`, the
  multiset must be correct and conformance should compare order-insensitively.
- No hashmap or host iteration order may leak into observable values, types,
  names, errors, or costs.
- Entropy and clock access happen only through the sanctioned host-injected
  seam, so tests can inject fixed inputs.
- Cost is part of conformance: the same query and database state must accrue
  identical cost in every core.

## Type System Rules

- Columns have strict static types; values are not silently reinterpreted.
- Current integer naming uses `i16`, `i32`, `i64` and aliases PostgreSQL names
  where specified.
- Preserve PostgreSQL-compatible three-valued NULL logic where the spec says so.
- Exact decimal behavior follows the spec, including PostgreSQL-style
  round-half-away-from-zero.
- Open types are real database facts. Composite, array, and range behavior must
  be derived recursively from their element or field types and verified by
  conformance and golden fixtures.

## Storage And File Format

- One database is one file.
- Durable on-disk storage is the dominant mode; in-memory databases are valid
  but secondary.
- Datasets are expected to often fit in RAM, but no design above the storage
  seam may harden a full-residency assumption.
- Preserve the block/page storage seam. Hosts provide byte devices; the core
  owns host-independent policy above them.
- On-disk format and key encoding are byte-exact contracts. Cross-core
  round-trip and golden fixtures are required for file-format changes.
- Key encoding must preserve logical order in raw byte order.
- Do not assume on-disk page bytes are plaintext-comparable; leave room for the
  encryption-at-rest design.
- Shared multi-process file access is the decided first locking slice, not an
  exclusive-only precursor. Coordinate through the stable `<path>.lock/` OS-lock
  bundle in `spec/design/locking.md`: one global writer, meta freshness at
  transaction begin, append-only commits while co-resident, and reuse/compaction
  only when presence-exclusive proves aloneness. The one-process foreground path
  must retain zero coordination syscalls and zero per-transaction meta reads.
- Never use PID files, mtime leases, or automatic stale-lock stealing for database
  safety. A host without the required crash-clean OS lock fails closed.
- Node shared locking requires native OS-lock code. The current delivery proposal
  keeps the independent TypeScript engine and adds a minimal Node-API lock helper;
  its alone lease makes no foreground addon calls. A full Rust-core Node wrapper
  exists only as a reach experiment: it wins heavy/parallel reads but not cheap
  queries or writes uniformly, and it is not a TypeScript conformance voice.
- Treat the lock-bundle protocol version as a compatibility boundary. Pre-protocol
  binaries cannot overlap safely and must be drained once during first rollout.
- Replication, where relevant, is block-delta shipping at the block seam, not a
  WAL.
- The deterministic hash JOIN is currently in-memory; grace-hash partitioning is
  the remaining spill slice and must preserve probe/bucket order and cost.

## Safety And Resource Boundaries

- Untrusted SQL must be safe to run against the built-in surface.
- Core languages and dependencies must preserve memory safety.
- Built-ins must be pure and side-effect-free: no filesystem, network, process,
  environment, shell, or arbitrary host access.
- Host-defined functions and extensions are outside jed's built-in safety and
  determinism guarantees. Keep the boundary legible and fail closed where the
  spec requires it.
- Thread cost metering and parser nesting limits through relevant code paths so
  resource exhaustion can be bounded deterministically.

## Dependencies

- The parser, planner, executor, storage layer, type system, and expression
  evaluator are written from scratch in every core.
- Add third-party dependencies only for narrow edge utilities when they preserve
  deterministic cross-core behavior, provide a significant platform-specific
  speedup while remaining byte-identical, or are vetted cryptography.
- Always get explicit human confirmation before adding a dependency.
- Dependencies must not introduce unsafe, cgo, FFI, nondeterminism, locale or
  library-version-sensitive behavior, or divergence across cores.
- Bench modules are not cores, but new benchmark dependencies still require
  explicit confirmation.

## Local References And Heavy Operations

- Do not automatically provision or update `references/`.
- Never run `rake references:setup`, `rake references:update`, or any large
  download on your own initiative.
- If reference sources are missing, work without them or ask the user.
- PostgreSQL oracle access is via the preconfigured Unix socket. Do not override
  `PGHOST`.

## Coding Style

- Prefer boring, explicit code over clever abstraction.
- In Rust, avoid unnecessary macro magic and deep generics.
- In Go, avoid unnecessary interfaces and abstraction layers.
- Keep modules flat, well named, and single purpose.
- Prefer Ruby and Rake for scripts, task orchestration, codegen drivers, and
  automation. Use shell or Make only when clearly better for the job.

## Website And Docs

- `/web` is a downstream consumer of the user-facing SQL and embedding surface.
- When a change adds or alters user-facing SQL behavior or host APIs, update the
  relevant docs, examples, and live panels in the same change.
- Generated reference pages should continue to derive from spec data rather
  than hand-maintained duplicates.

## Git And Collaboration

- Multiple agents may work from separate containers. Sync through `origin`, not
  shared working-tree assumptions.
- Before continuing work reported by another instance, fetch and verify the
  referenced commit or branch locally.
- Feature branches should be pushed promptly to the private `origin` after the
  first commit and after subsequent commits.
- Merge to `master` only when verification is green.
- `master` history must stay linear. Use fast-forward-only integration, rebased
  branches, squash merges, or cherry-picks; do not create merge commits on
  `master`.
- A note that work is "landed" or committed at a hash should mean pushed unless
  it explicitly says branch-only or local-only.

## Typical Workflow

1. Read `CLAUDE.md`, relevant `spec/design/*` docs, shared spec data, and
   existing implementation patterns.
2. Add or update shared conformance entries for the behavior.
3. Implement the smallest vertical slice across the affected core or cores.
4. Add per-core unit tests only for behavior outside corpus reach.
5. Update specs, TODO, website docs, and examples when user-facing behavior or
   standing design changes.
6. Run the relevant verification. Prefer `rake ci` for broad confidence and
   narrower Rake tasks or per-core tests for scoped changes.
7. Report what changed, what was verified, and any remaining gaps.
