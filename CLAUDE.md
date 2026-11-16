# Project Design Brief

> Standing context for all work in this repository: the load-bearing record of architectural
> decisions. Read it before making changes; when a decision here is revised, update this file in
> the same change. It states *decisions and rules*; the detail and status of each subsystem live in
> `spec/design/` (linked below) and the roadmap in [TODO.md](TODO.md). Keep it that way — add a
> pointer here, not a changelog.

The project is **jed** (also called "the engine").

---

## 1. What we are building

An **embedded SQL database**: **SQLite's footprint, PostgreSQL's behavior, and a real (strict,
static) type system.**

- **Embeddable** — a library linked into a host program, not a server.
- **Single-file storage** — one database = one file.
- **A strict, static type system** — this is the product (§4).
- **PostgreSQL behavior** — NULL logic, comparisons, ordering, exact numerics, errors match PG.
- **SQL-first, not SQL-only** — everything is reachable through SQL, but storage does not assume
  SQL is the only access path (§9).

**PostgreSQL is the behavioral default:** when an option matches PG and there is no overriding
reason against it, take it. PG is **not a compatibility target** — no wire protocol, no
`pg_catalog` fidelity, no full coercion lattice, no arbitrary-SQL coverage. **We own our surface**;
for the surface we implement, behavior tracks PG. An **overriding reason** is a genuine engineering
tradeoff — simplicity, determinism (§10), the strict type system (§4), single-writer (§3),
memory-safety / cost bounds (§13) — documented where taken. Deliberate divergences are recorded in
the relevant spec doc.

---

## 2. Multiple native cores, no reference implementation

The engine is implemented **natively in multiple languages**, from scratch, in **lockstep**, with
**no reference implementation**. Two maximally-different implementations under a shared contract
turn every spec ambiguity into a failing test; one implementation resolves ambiguities silently.

**The spec is the project.** The language-neutral specification and conformance corpus is the
canonical artifact; every implementation, including the first, is a downstream consumer. Never let
one core lead and write the spec from it.

**The differential set** (the cores whose disagreement hardens the spec — kept in lockstep, all in
CI, byte-exact on-disk format across them):

1. **Rust** (`impl/rust`) — manual ownership, no GC, no runtime.
2. **Go** (`impl/go`) — GC, goroutines, the maintainer's daily driver. **Pure Go: no cgo, no FFI**
   (§14 does not relax this). Host API conventions (context-first, cancelable writer waits keeping
   SQLSTATE `57014`, rollback on panic, `time.Duration` options, the `jedtooling` build tag,
   `MapRows`): `spec/design/api.md` §11.
3. **TypeScript** (`impl/ts`) — native core at full parity; stresses the axes Rust and Go share
   (exact `bigint` i64, UTF-16 strings, `DataView` big-endian). Runs on modern Node by native
   type-stripping (erasable TS subset, no build step). Hosts: Node file host and the
   **Browser/OPFS** host in a Web Worker (`spec/design/hosts.md` §5). The Node file host uses the
   narrow Node-API lock helper `impl/ts/native-lock` for OS file locks only (§14). The public API
   is **async-only** over dedicated execution workers (`spec/design/ts-runtime.md`); the
   synchronous core API is internal (conformance/tooling). `impl/node` (Node-API wrap of the Rust
   core) is a reach/benchmark artifact, never a conformance voice.

**Beyond the differential set — best experience per language.** Further languages are not argued
on "how much divergence they surface"; the question is what gives that language's users the best
experience. Two first-class options:

- **Wrap the safe Rust core** (§13) when performance and byte-exact-for-free dominate (host calls
  still pay marshalling).
- **Write a native core** when clean integration dominates (no FFI, idiomatic in-process host
  functions, single-language packaging).

Either way the conformance contract binds; a wrapped core inherits it from Rust and is never an
independent conformance voice. Decide per language and record it in `spec/design/cores.md`
(current leanings: C# native, Swift wrap, Java conflicted). The pivots are host-defined functions
(hot per-row → native; coarse → wrap) and parallelism (wrapping gives Rayon-grade fan-out). Design
the host-function API **vectorized/batched** so wrapping stays viable.

**Opt-in frozen experiments.** These are native cores pinned to a recorded supported-core revision.
They are **excluded** from normal tests, benchmarks, CI, the website and the lockstep obligation;
run or refresh one **only when explicitly asked**; future features do not update them; promotion or
production delivery is a separate decision. Their scope, baselines, approvals, verification and
measurements live in their docs:

| Experiment | Doc |
|---|---|
| `impl/ruby-native` (pure Ruby; distinct from the Rust binding `impl/ruby`) | [ruby-native-experiment.md](spec/design/ruby-native-experiment.md), [ruby-runtime-compilation.md](spec/design/ruby-runtime-compilation.md) |
| `impl/swift-native` | [swift-native-experiment.md](spec/design/swift-native-experiment.md), [swift-native-performance.md](spec/design/swift-native-performance.md), `impl/swift-native/VERIFICATION.md` |
| `impl/csharp-native` | [csharp-native-experiment.md](spec/design/csharp-native-experiment.md), `impl/csharp-native/VERIFICATION.md` |
| `impl/java-native` | [java-native-experiment.md](spec/design/java-native-experiment.md), `impl/java-native/VERIFICATION.md` |

Experiment performance results apply only to their recorded workloads and hosts; never cite them as
general parity.

---

## 3. Scope simplifications (load-bearing — do not quietly relax)

- **Single writer; readers block only during commit.** A writer stages all changes in a private
  pending write set; readers see the last committed state without blocking; commit is a short
  atomic root swap. **NOT MVCC** — no version chains, visibility timestamps, or vacuum: exactly one
  committed version plus one writer's pending set (`spec/design/transactions.md`).
- **No users, roles, RBAC, or auth in the database.** Authorization is a per-**session** capability
  envelope supplied by the host (table privileges, `EXECUTE`, `allow_ddl`, cost budgets; engine
  enforces with `42501`) — `spec/design/session.md`.
- **PG is aspirational, not strict** (§1).

---

## 4. The type system is the product

Designed on paper, as data (§5), before the executor; it is the spec everything tests against.

- **Strict static column types**; values are never silently reinterpreted.
- **Scalars** (`spec/design/types.md`, `spec/types/`): `i16`/`i32`/`i64`, `f32`/`f64`, exact
  `decimal`, `text`, `boolean`, `date`/`timestamp`/`timestamptz`/`interval`, `bytea`, `uuid`,
  `json`/`jsonb`/`jsonpath` (`spec/design/json*.md`). Canonical names state width in **bits**
  under the `i`/`f` prefix, lexically disjoint from PG's byte names, so SQL-standard words and PG's
  `int2`/`int4`/`int8`/`float4`/`float8` are accepted as aliases with no collision. Integer overflow
  **traps**.
- **The type system is open**: `Type { Scalar | Composite | Array | Range | Record }`, threaded
  through every core; the closed `ScalarType` enum never gains user variants.
  - **Composite** (`spec/design/composite.md`) — nominal, catalog-defined (`CREATE TYPE … AS`).
    Only named composites are stored; anonymous `record` is a query value only. No implicit
    per-table row types (PG divergence).
  - **Array** (`spec/design/array.md`) — structural `Array(Box<Type>)`. Matching PG, **shape is a
    property of the value, not the type**; the element type stays strictly enforced. Comparison
    uses PG btree NULL semantics (definite boolean), not composite 3VL.
  - **Range** (`spec/design/ranges.md`) — structural, over PG's six built-in subtypes; discrete
    subtypes canonicalized to `[)`; total btree order.
  - Container keys (range, array of scalars, composite) are keyable (`spec/design/encoding.md`); an array
    of composites is the remaining `0A000` key.
- **Three-valued NULL logic.**
- **An explicit comparison / coercion / promotion matrix, as data.**

---

## 5. Data over code

Anything mechanical and data-shaped is **shared language-neutral data**, never per-language code:
the comparison/coercion/promotion matrix, the function/operator catalog, the error-code registry,
the runtime cost schedule, and the estimator's mechanical facts (`spec/design/estimator.md`).

**Codegen** is the middle path for large mechanical surfaces (the function catalog). **Do NOT
codegen** the parser, planner, executor, storage layer, expression evaluator, or the recursive
container codecs/comparators — those are hand-written per core. Container types have no fixed data
table, so their cross-core contract is golden fixtures + conformance entries; byte-identity holds
by construction from already-identical field/element types (`spec/design/extensibility.md` §4.1).

(The Ruby experiment's runtime-compiled query kernels are a bounded, explicitly authorized
exception: `spec/design/ruby-runtime-compilation.md`.)

---

## 6. Repository shape

```
/spec/          CANONICAL: design/ (prose "why"), grammar/ (EBNF; parsers hand-written),
                types/, functions/, errors/, fileformat/ (+ byte fixtures), encoding/ (+ vectors),
                conformance/ (corpus + oracle harness), pageservice/ (wire contract)
/impl/          rust/ go/ ts/ (the differential set); bindings and frozen experiments
/migrate/       jed-migrate: non-core consumer library (per-core packages), bundled as `jed migrate`
/cli/ /bench/   non-core consumers; their dependencies never touch a core manifest
/web/           static SvelteKit + Tailwind site and in-browser playground over the TS core
mise.toml       pinned tools, [env], composite/shell tasks; logic-bearing tasks in /mise-tasks/
process-compose.yaml   long-running dev services (the PostgreSQL oracle); `mise run dev`
```

Each implementation ships a thin harness that runs the shared conformance corpus.

---

## 7. Conformance — the contract between implementations

Details: `spec/design/conformance.md`.

- **sqllogictest-style corpus** (`statement ok`, `statement error`, `query … rowsort`, hashing).
- **Concurrency is in the contract** (`spec/design/concurrency-testing.md`): the `# format:
  concurrency` schedule corpus (deterministic total order over named sessions; runs in `mise run
  ci`, stepped-threaded under the race detector on Go/Rust); parallel stress (`mise run stress`,
  outside CI, cross-core confluent checksum); and the release-blocking multi-process corpus
  (`mise run concurrency:process`, real Rust/Go/Node actors over one file). Concurrency behavior
  must not live only in mirrored per-core tests.
- **Deterministic simulation testing** of the storage state machine
  (`spec/design/simulation-testing.md`): seeded multi-session histories over a simulated disk,
  filesystem, spill host, lock host and network with crashes, power loss, torn sectors and I/O
  errors; all cores must produce byte-identical transcripts and crash images. **When a storage
  feature lands (page type, commit path, host operation, setting), grow the generator and checkers
  with it.**
- **PostgreSQL is the result oracle**; deliberate divergences go in the override ledger.
  **SQLite is not a result oracle** (it diverges on exactly jed's product surface); its corpus may
  be mined for query shapes only.
- **Metamorphic testing** (NoREC): every query optimization adds a relation.
- **Tiered by capability flags** so cores may move at different speeds.

---

## 8. Divergence hotspots — decide in the spec before coding

Default tie-breaker: match PostgreSQL. These bind jed's own cores; a host extension may relax them
and owns the consequence (§13).

- **Floats** (`spec/design/float.md`): kept out of exact paths; text output is PG's exact
  `float8out` in every core; only transcendental values and float `SUM`/`AVG` fold order are
  ledgered (tolerance-compared). NaN/±Inf follow PG's total order.
- **Decimal** (`spec/design/decimal.md`): round half away from zero, PG scale rules, always finite
  (no NaN/∞ — PG divergence; `x/0` traps `22012`).
- **Collation**: baseline `C` (code-point order); linguistic collation is a jed-owned UCA executor
  over host-loaded Unicode data, not ICU (`spec/design/collation.md`).
- **Integer overflow**: trap.
- **Iteration-order leaks**: none may reach the result multiset, values, types, names, errors, or
  cost. Row sequence is defined **only** by `ORDER BY` (then fully deterministic, ties broken by
  primary key); without it order is unspecified and the harness uses `rowsort`.
- **Plan choice is specified**: same resolved query + visible estimator inputs ⇒ same selected
  plan, estimates and cost in every core (`spec/design/estimator.md`, `spec/design/planner.md`).
  Plan drift is a bug, never a ledger exception.
- **Byte fixtures**: order-preserving key encoding with `(value → bytes)` vectors
  (`spec/encoding/`); the file format round-trip (every core reads and writes byte-identical files
  against shared goldens) is a conformance test.
- Persisted expression keys and partial-index predicates follow
  `spec/design/index-dependencies.md`.

---

## 9. Storage

Format: `spec/fileformat/format.md` (and `encrypted.md`); design: `spec/design/storage.md`,
`pager.md`, `validated-cow.md`, `hosts.md`. The format version and per-version changes are recorded
there, not here.

- **One database = one file.** A host may **attach** other databases (file or in-memory) by name
  via the host API, never SQL; a query may span them in one consistent snapshot; a transaction
  writes at most one durable database (`spec/design/attached-databases.md`; multi-file atomic write
  is parked).
- **Design target: durable on-disk databases whose dataset is RAM-sized.** Durability is core
  (crash recovery, ordered writes, fsync at commit); in-memory is a minority mode; the resident
  representation is first-class; target SSDs.
- **Must not preclude larger-than-RAM (TB-scale).** Standing non-foreclosure rule: **no code above
  the storage seam may harden a full-residency assumption** — no "load = read whole file", no
  operator that requires its whole input/output in RAM. Keep load-bearing: page-structured format,
  the block/page seam, order-preserving keys, logical page-access cost units (a cache is invisible
  to cost). Built: incremental COW commit, B+tree interiors, free-list reclamation, bounded buffer
  pool, spilling sort/hash join/aggregate/DISTINCT under `work_mem` (`spec/design/spill.md`), lazy
  record decode (`spec/design/lazy-record.md`).
- **Storage seam**: a `BlockStore` byte device each host implements (file, OPFS, in-memory, temp
  domains) beneath a per-core `Pager` (`spec/design/hosts.md`). In-memory databases and session temp
  tables use the same pager/leaf path over RAM (`spec/design/temp-tables.md`).
- **Pluggable behind the relational layer**: do not foreclose alternate physical layouts or a
  low-level direct-access API (undecided whether either ships).
- **Commit**: validated copy-on-write — dirty pages written once, identities in a manifest, one
  final barrier, recovery validates before accepting a root; no WAL, no redo
  (`spec/design/validated-cow.md`). `synchronous = off` batches durability without weakening
  atomicity. Commits persist only changed database domains.
- **Encryption at rest** (built; `spec/design/encryption.md`, `spec/fileformat/encrypted.md`): a
  page codec above the block seam, envelope encryption (wrapped data key in a double-buffered key
  block), XAES-256-GCM per-page keys from fresh random salts, tag-bound pointers, Argon2id
  passphrases, key material from a dedicated CSPRNG (never the seeded SQL entropy seam). Remaining
  work fails closed (encrypted.md §10); the security review is pending.
- **Replication** (door kept open, not built): ship per-commit page deltas in txid order as a tee
  below the encryption codec — no WAL (`spec/design/replication.md`).
- **Large values**: overflow chains + a hand-rolled byte-pinned LZ4 codec — **no compression
  library**, since encoders aren't standardized (`spec/design/large-values.md`).
- **Compaction** is host-invoked, never waits (`55006`), results- and cost-neutral (`api.md` §2.6).
- **Stored SQL a binary cannot parse** is kept byte-for-byte; only statements needing it fail
  `XX003` (`spec/design/unparsable-definitions.md`).
- **Multi-process access: the page service** (`spec/design/page-service.md`, `locking.md`): one
  owner process touches the file; others are page clients over a local socket or SSH. Crash-clean
  whole-file OS locks only — **PID/mtime stale-lock heuristics are forbidden**; unsupported hosts
  fail closed. Pages are logical cost units, so results/plan/cost are identical through the service.
- **Host file API** (`spec/design/api.md`): explicit `commit`; `close` discards uncommitted work.

---

## 10. How to work in this repo (AI-agent-first)

- **The corpus is the contract.** A feature is a vertical slice (one SQL construct parsed, planned,
  executed, tested) that makes corpus entries pass. On the PG-comparable surface, oracle-check rows
  (`mise run corpus:check`) and ledger divergences; optimizations add a NoREC relation.
- **Tests go in the corpus by default.** Write a per-core unit test **only** for what the corpus
  cannot express: deliberate PG divergences, catalog/host introspection, byte-level checks, cost
  values no cost suite pins, host-API surface, internal invariants. When unsure, write the corpus
  entry. (Model: the per-core `foreign_key` tests vs. `ddl/foreign_key.test`.)
- **Determinism everywhere** — exact multiset, values, types, errors, selected plan, estimates,
  cost; no wall-clock nondeterminism. Default-deny with a ledger
  (`spec/design/determinism.md`, `spec/conformance/determinism_exceptions.toml`). Entropy and clock
  come only through the host-supplied seam in the handle's immutable `Environment`
  (`spec/design/entropy.md`, `environment.md`); no process-global loaded data.
- **Benchmarks are wall-clock, never conformance** (`bench/`, `spec/design/benchmarks.md`), outside
  `mise run ci`, but answers are checksummed across engines. Perf-relevant features add a
  benchmark; perf-sensitive changes report before/after numbers (`mise run bench:diff`).
- **Keep `/web` in sync**: a change to user-facing SQL or the host API updates the matching
  `web/src/routes/docs/` page and `web/examples/<topic>/` sources in the same change (reference
  pages generate from spec TOML). Run `mise run e2e:web` when touching `web/src/lib/jed/` or a
  documented behavior.
- **Boring, explicit code** — no deep generics/macro magic in Rust, no over-interfacing in Go.
- **Ruby for scripting; `mise run <task>` is the only entry point** (no Rakefile, no Make). A
  composite or straight-line task is a shell `run` in `mise.toml`; a task with real logic is an
  executable Ruby script in `mise-tasks/` named by its path (helpers in `scripts/lib/tasks.rb`;
  larger tools in `scripts/`). Don't wrap plain commands in Ruby or grow shell past a few lines.
  `mise run <task> --help` shows arguments. mise runs `depends` **in parallel**, so sequence steps
  as `{ task = … }` entries in `run` (benchmarks must never overlap anything); the gates (`test`,
  `ci`, `conformance:all`) parallelize via `scripts/lib/parallel.rb` and declare `after:` ordering.
- **Spec-first per subsystem**: a design doc + its corpus is enough to work it.
- **Many sessions, one object store.** Claude and Codex sessions each work in a **git worktree of
  one checkout**, sharing one object store and ref namespace: a commit is visible to every worktree
  immediately, without push or fetch — **committing** (even WIP) is the hand-off boundary.
  Consequences: other sessions may move shared refs (so **bare `git stash`/`stash pop` is
  forbidden**); a branch checked out in one worktree can't be in another, and `master` lives in the
  main checkout, so fast-forward there (`git -C <main> merge --ff-only <sha>`, after checking it is
  clean); verify another session's claims against git, not its prose.
- **`origin` is the public GitHub repo** (`jackc/jed`); pushing publishes. **Push only when the
  user asks.** Merge to `master` only when green (`mise run ci`). **`master` history is strictly
  linear** — rebase then fast-forward, squash, or cherry-pick; never a merge commit, never `git
  pull` without `--ff-only`/`--rebase`. Memory notes about landed work say "pushed" or "local only,
  NOT pushed".

---

## 11. Build order and roadmap

The original bootstrap order (scaffold → type spec → corpus format → seam + key fixtures → first
integer vertical slice in Rust and Go → on-disk format round-trip → UPDATE/DELETE) is complete.
Each step was independently testable; keep that property — no point where progress blocks on one
giant subsystem.

**Forward work is tracked in [TODO.md](TODO.md).** Consult it when planning any feature, confirm
the work fits the commitments here, and update it in the same change when the plan moves.

---

## 12. Local reference sources (uncommitted)

Source checkouts of other databases live in `references/` (gitignored) as oracles and design
references: `postgres` (`REL_18_STABLE`, matching `spec/conformance/oracle_profile.toml`),
`sqlite`, `duckdb`, `bbolt`, `sqllogictest-rs`. **CockroachDB is excluded** (BSL); read its
key-encoding ideas via `spec/encoding/` or an old Apache-2.0 tag.

> **Do NOT provision references automatically.** Never run `mise run references:setup` /
> `references:update` or any other large download or heavy operation on your own initiative —
> work without it or ask the user. **Exception:** if every `<name>.git` mirror already exists with
> refs at the machine-level mirror path, `references:setup` only adds local worktrees and you may
> run it. `references:update` (network) is never covered.

Tasks: `references:setup | update | status | clean`. Mirrors are bare clones outside any checkout
(`REFERENCES_MIRROR_DIR`; default `/persist/shared/references/` in a devcontainer,
`${XDG_DATA_HOME:-~/.local/share}/jed/references/` natively); `references/<name>` is a worktree of
the mirror.

**Live PostgreSQL oracle**: each checkout has its own cluster (`.dev/postgres`, `mise run db:init`,
supervised by process-compose via `mise run dev`), reached over a Unix socket whose port/dir come
from `.dev/ports.env` / `.dev/derived.env` (loaded by mise). **Never set `PGHOST` and `PGPORT`
independently.** Its configuration is declared data (`spec/conformance/oracle_profile.toml`,
`mise run oracle:status`); corpus probes use the `jed_oracle` database (`mise run oracle:setup`) on
PG's `builtin` `C.UTF-8` provider, whose order is jed's `C` collation — so `psql -d jed_oracle` for
corpus work.

---

## 13. Untrusted queries: safe to run

**Requirement: untrusted (adversarial) SQL is safe to run** — it cannot violate memory safety,
cannot reach outside the database (no filesystem, network, process, environment, or clock beyond
the sanctioned seams), and its resource use is bounded. The vehicle is a configured session
(`spec/design/session.md`): minimal privileges + `max_cost` + `lifetime_max_cost` +
`max_query_memory_bytes`.

1. **Memory safety** — every core is memory-safe; any `unsafe`/cgo/FFI path must be justified
   against this and §14. Wrapped cores wrap the *safe* Rust core.
2. **No dangerous built-ins** — the built-in catalog is **pure and side-effect-free**: no
   `pg_read_file`, no host `COPY`, no `DO`/extension loader. The only window outward is the
   host-injected entropy/clock seam. A function that breaks purity does not belong in the set.
3. **Bounded resources** — a **deterministic, cross-core-identical cost meter + ceiling** (units
   as data; the abort point is deterministic; `spec/design/cost.md`); a **fixed parser
   nesting-depth limit** (`54001`, cost.md §7); the finite `max_scalar_bytes` allowance (`54P04`);
   the opt-in `max_query_memory_bytes` account (`54P05`, `spec/design/memory.md`); and opt-in
   `max_storage_bytes` (`54P06`). Plan identity is part of cost identity (§8). These are
   **guardrails, not a whole-engine heap cap** — never claim a full memory guarantee; complete
   query-memory coverage remains required.

**The host-extension boundary.** Every jed guarantee — safety, determinism and byte-identity,
self-describing single files, no external row sources, atomic transactions — is a promise about
**jed's own engine and built-in surface**, non-negotiable there. A host extension (function, type,
access method, virtual row source, layout) **may relax any of them**; the host owns the
consequence. Don't argue host extensions down on core guarantees. jed keeps the boundary legible
(e.g. a file needing host code fails closed `XX002`; nondeterministic host values are tainted) and
offers containment as opt-in tools. A host function without a declared cost is admissible only on
an unlimited (`max_cost = 0`) handle. See `spec/design/extensibility.md` §2.

---

## 14. Third-party dependencies

The parser, planner, executor, storage, type system, and evaluator are **never** delegated to a
library. A dependency at the edges is allowed only when at least one holds:

1. **All cores can match** — byte-identical, deterministic behavior across every language core.
2. **A per-language library is *significantly* faster** — and still output-identical to the spec.
3. **Cryptography** — never roll our own; use vetted libraries.

**Always get explicit human confirmation before adding any dependency** — name the clause and wait
for a yes. Guardrails: engine code and dependency APIs stay memory-safe (reviewed crypto
assembly/intrinsics and narrowly approved Rust `unsafe` behind safe APIs are permitted; the Go core
stays free of cgo and external C FFI); no leaked nondeterminism or cross-core divergence (the ICU
trap, `types.md` §11); dependencies live only at bounded edges. `bench/` modules are not cores
(their deps, including the cgo SQLite baseline, stay out of core manifests; new ones still need
confirmation).

**Approved dependencies** (exact pins; nothing else inherits these approvals):

- **Digests** (2026-10-13, clause 3): Go stdlib; Rust `md-5 = 0.11.0`, `sha2 = 0.11.0` (its
  SHA-NI/SIMD `unsafe` accepted knowingly); TS `@noble/hashes = 2.4.0` (pure JS, no transitive
  deps) — `spec/design/string-functions.md` §4.
- **Encryption at rest** (2026-10-20, clause 3): Rust `aes = 0.9.3`, `aes-gcm = 0.11.1` (default
  features off), `hkdf = 0.13.0` (+ `hmac 0.13.0`), `argon2 = 0.6.0` (default features off); Go
  stdlib + `golang.org/x/crypto v0.58.0` for `argon2` only (pure Go); TS `node:crypto` (Node-only
  module), WebCrypto in the browser, `@noble/hashes` Argon2id. XAES-256-GCM glue is hand-written
  per core against C2SP vectors — `spec/design/encryption.md`.
- **Node lock host** (2026-07-16): `impl/ts/native-lock` may use safe-Rust `napi = 3.10.5`,
  `napi-derive = 3.5.10`, build-only `napi-build = 2.3.2`, for whole-file try-lock/unlock only;
  excluded from browser bundles.
- **AEAD benchmark** (2026-10-17/19): `bench/aead/` only, for measurement — see
  `bench/aead/README.md`.
- **Frozen experiments**: Ruby (OpenSSL digests/AES/HKDF, stdlib Zlib CRC32, `argon2-kdf`),
  Swift (`swift-crypto` + CryptoExtras, pinned reference Argon2), C# (`BouncyCastle.Cryptography
  2.7.0` for SHA-224/Argon2id only), Java (`bcprov-jdk18on:1.86` for Argon2id only; JDK FFM for
  host OS calls). Exact scope in each experiment's doc (and `swift-native-crypto.md`,
  `csharp-native-crypto.md`). None authorizes native engine components.
