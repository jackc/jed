# Project Design Brief

> This file is the standing context for all work in this repository. It is the
> load-bearing record of architectural decisions. Read it before making changes.
> When a decision here is revised, update this file in the same change.

The project name is TBD. This document refers to it as "the engine."

---

## 1. What we are building

An **embedded SQL database**. The one-line north star:

> **like SQLite, but with a real type system.**

Properties:

- **Embeddable** — a library you link into a host program, not a server process.
- **Single-file storage** — one database = one file on disk, like SQLite.
- **A deliberate, strict, static type system** — this is the product. Not SQLite's
  runtime type affinity.
- **SQL-first, not SQL-only** — relational SQL is the primary surface and *everything*
  must be reachable through it, but the storage layer is designed so SQL need not be the
  only access path (see §9).

PostgreSQL is **inspiration, not a compatibility target.** Where PG's *behavior* is
principled (three-valued NULL logic, exact decimals, comparison semantics) we borrow
it. We do **not** owe anyone wire-protocol compatibility, `pg_catalog` fidelity, the
full PG type-coercion lattice, or arbitrary-SQL coverage. We own our surface.

---

## 2. The central commitment: multiple native cores, no reference implementation

The engine is implemented **natively in multiple languages**, from scratch, in
**lockstep**. There is deliberately **no reference implementation**.

**Why.** Two maximally-different implementations evolving together turn every spec
ambiguity into a failing test the day it is written. A single implementation just
resolves ambiguities silently in whatever way its code happened to run. The honesty
mechanism is *divergence under a shared contract*, not implementation count.

**Consequence — the spec is the project.** Because no implementation is canonical,
the **language-neutral specification and conformance corpus is the canonical
artifact.** Every implementation, including the first, is a *downstream consumer* of
it. If we let one language's core lead and write the spec from it, the spec inherits
that language's accidents — which is exactly the leakage we are preventing.

### Implementation priority

1. **Rust** — manual ownership, no GC, no runtime.
2. **Go** — GC, goroutine concurrency, a runtime. The maintainer's daily-driver
   language. Pure Go: **no cgo, no FFI.**

Rust and Go are about as far apart as two systems languages get, so this pairing does
the bulk of the honesty work. Build these two in genuine lockstep from the first
vertical slice.

Later consumers of an already-hardened spec (reveal far fewer new ambiguities; mostly
prove portability and expand coverage):

3. **JS / TypeScript** — must run in the browser. Native TS implementation is the
   default under the multi-core philosophy; a Rust→WASM wrap remains a pragmatic
   fallback. **TBD.**
4. **Java**, **C#** — native.
5. **Swift** — the one asterisk. If Swift's Rust-interop story makes a native port
   impractical, wrapping the Rust core is an acceptable *deliberate exception* to the
   native-implementation rule, not a continuation of it.

With Java/C#/Swift covered, essentially every modern environment has a native engine.

---

## 3. Scope simplifications (load-bearing — do not quietly relax these)

- **Concurrency: single writer; readers block only during commit.** At most one writer
  at a time. A writer accumulates *all* of its changes in a **private in-memory staging
  area** (a pending write set), leaving the last committed state untouched and
  continuously readable. Readers see the last committed state and run **without blocking
  against an in-flight writer**; the only exclusive window is the **commit itself**,
  where the staged changes are applied to the live state atomically — favor a
  pointer/root swap so that window is as small as possible. This is still **NOT MVCC** —
  there are no per-row version chains, no visibility timestamps, no retained multiple
  concurrent versions, no vacuum. There is exactly **one committed version plus one
  writer's pending set**. Single-writer keeps a single-threaded core clean; concurrency
  remains the host's problem, now mediated by the staging buffer + a short commit lock
  rather than a whole-database read-write lock. (May be revisited far in the future.
  Until then, assume it everywhere.)
- **No users, no roles, no RBAC, no auth.** Deletes the permission catalog entirely.
- **PG is aspirational, not strict.** See §1.

---

## 4. The type system is the product

Design it on paper (as data — see §5) **before** writing the executor. It is the spec
everything else tests against, not a detail discovered during implementation.

- **Strict static column types.** A column has one type; values are not silently
  reinterpreted at runtime.
- **A deliberate scalar set**, starting small and growing on demand. Eventual intent:
  fixed-width integers (with *defined* overflow behavior), an **exact `decimal`**,
  `text` (one defined collation/encoding to start), `boolean`, `timestamp` /
  `timestamptz`, `bytea`, and `json`/`jsonb` if we want a headline feature.
  - **First implemented step — signed integers only:** `int16` / `smallint` (16-bit),
    `int32` / `int` / `integer` (32-bit), `int64` / `bigint` (64-bit). Canonical names
    state width in **bits** (the programming-language convention); SQL-standard names are
    aliases. Two's-complement, with trap-on-overflow (§8). Every other scalar above is
    explicitly **deferred** to a later slice. The float/decimal/collation decisions in §8
    do not bind step 1.
- **Three-valued NULL logic.**
- **An explicit, documented comparison / coercion / promotion matrix** — expressed as
  data, not prose.

---

## 5. Data over code

**Anything mechanical and data-shaped is shared data, never per-language code.** Code
is written N times and drifts N ways; shared data is authored once and verified.

Shared, language-neutral data:

- The **comparison / coercion / promotion matrix**.
- The **function / operator catalog** — name, arg types, return type, null behavior.
- The **error-code registry** (errors are structured data, not free text).

**Codegen is the middle path** for large, purely mechanical surfaces (the function
catalog especially): generate per-language stubs from the shared definition. It sits
between runtime-loaded data (portable but indirect) and hand-writing N times
(drift-prone).

**Do NOT codegen** the parser, planner, executor, storage layer, or expression
evaluator. Those are irreducibly per-language and are the parts that genuinely cost N
times. Everything else, push into the shared layer.

---

## 6. Repository shape

The center of gravity is a **language-neutral spec directory**, not any single
implementation. Suggested layout:

```
/spec/                  # CANONICAL. The source of truth.
  design/               # prose design docs per subsystem (the "why")
  grammar/              # one EBNF grammar; parsers are hand-written per language
  types/                # scalar set + coercion/comparison matrix, as data
  functions/            # function/operator catalog, as data
  errors/               # error-code registry
  fileformat/           # on-disk format spec + byte-exact fixtures
  encoding/             # key-encoding spec + byte test vectors
  conformance/          # sqllogictest-style corpus + the differential oracle harness
/impl/
  rust/
  go/
  ts/                   # later
  ...
```

Each implementation ships a **thin harness** that runs the shared conformance corpus.

---

## 7. Conformance suite — the contract between implementations

This is the spine of the project. Treat it as the contract, not an afterthought.

- **Format: sqllogictest-style** (plain-text, declarative: `statement ok`,
  `statement error <pattern>`, `query <coltypes> <sortmode>` + expected rows, with
  hashing for large result sets). It was invented by SQLite precisely to run identical
  conformance tests across multiple independent engines — our exact problem. CockroachDB,
  DuckDB, and RisingWave use it for the same reason.
- **Bootstrap the corpus via differential testing.** PostgreSQL and SQLite source and
  test suites are fair game as **oracles**: run a supported-subset query against real
  PG/SQLite, capture output, emit a corpus entry. Generates a large, *correct* corpus
  cheaply. Where our semantics intentionally diverge, override the expected output by
  hand and document why.
- **Layer metamorphic / generative testing later.** SQLancer is the canonical prior art
  (finds logic bugs by synthesizing queries whose correct answer is known by
  construction). Well-suited to an agent-driven loop.
- **Version the spec and tier the corpus.** Each implementation declares a conformance
  level (capability flags / feature tiers) so Go can run ahead while TS catches up
  without the whole suite reading as broken. sqllogictest `skipif`/`onlyif` handles
  per-engine quirks; the tier system handles different speeds.

---

## 8. Cross-implementation divergence hotspots (decide in the spec BEFORE coding)

These are the classic sources of silent divergence. Make explicit, documented
decisions; they are miserable to retrofit.

- **Float formatting** — every language prints `f64` differently. Decision bias: keep
  binary floats **out of the comparison and text-output paths entirely**; lean on exact
  `decimal`. This aligns with "a real type system" and kills the worst offender.
- **Decimal rounding** — define mode and scale.
- **NaN / infinity ordering** — define it.
- **Collation** — start with ONE defined collation (byte/codepoint order is simplest);
  ICU-style collation is an explicit later feature.
- **Integer overflow** — defined wrap vs. trap.
- **Iteration-order leaks** — no hashmap iteration order may leak into results. Defined
  ordering everywhere.

### Byte fixtures make the two worst subsystems verifiable, not hoped-for

- **Key encoding must be order-preserving**: stored keys iterate in raw byte order, so
  encoded keys must sort identically to logical values across *every* implementation.
  Big-endian unsigned ints; sign-bit flip for signed; inversion for descending;
  length-prefixed or fixed-width composite components. Ship `(value → expected bytes)`
  test vectors as shared fixtures. (CockroachDB's `encoding` package is a good reference
  design.)
- **File format round-trip is a conformance test**: a database file written by the Rust
  core must be byte-readable by the Go core and vice versa. This single test catches an
  entire class of divergence automatically.

---

## 9. Storage

- **Single file** per database.
- **Design target: in-RAM datasets, SSD-backed persistence.** As an embedded database,
  the *entire dataset resident in memory* is a common — often the expected — case, so the
  in-memory representation is a **first-class concern**, not merely a cache over disk.
  Persistence targets **SSDs**, not spinning disks: choose block/page size, on-disk
  layout, and write patterns for SSD characteristics (page-aligned I/O,
  write-amplification awareness) rather than HDD seek-minimization. This pairs with the
  staging-buffer commit model (§3): writes batch in memory and land on the SSD at commit.
- The core defines a **storage seam** (a block/file interface) that each host
  implements: `os.File` in Go, OPFS in the browser, direct file access natively.
  Designing this seam early is what makes "single-file, embeddable, everywhere" work.
- **Keep the storage model pluggable behind the relational layer.** SQL is the primary
  access path and everything MUST be reachable via SQL (§1), but it is not assumed to be
  the only one. The architecture should not foreclose: (a) **multiple physical layouts** —
  row-oriented now, with column-oriented or key-value stores as possible per-table
  alternatives later; or (b) a **low-level direct access API** beneath SQL (e.g.
  `value = getValue("tableName", key)`, direct row read/write). Whether either ships is
  **undecided** — the requirement is to keep the seam open, not to build them now.
- On-disk format and key encoding are spec'd with byte fixtures (§8). **Status:** the
  single-file on-disk format is authored (step 5b) in `spec/fileformat/format.md` in a
  deliberately narrowed **whole-image** form — a commit serializes the entire database to
  one byte image. Both the Rust and Go cores read/write byte-identical files, verified
  against shared golden fixtures (the §8 cross-core round-trip). **Deferred until
  `UPDATE`/`DELETE`:** incremental copy-on-write, free-list/page reclamation, and B-tree
  interior pages. The double-buffered meta page + root pointer are the forward-compatible
  hooks for the live incremental commit model (§3).

---

## 10. How to work in this repo (this is an AI-agent-first codebase)

The design is optimized for AI agents even more than for humans. In practice:

- **The conformance corpus is the contract.** Implement a feature as "make these corpus
  entries pass." A feature = one SQL construct, parsed + planned + executed + tested, as
  a **vertical slice**. That is the unit of agent work and the unit of cross-language
  porting.
- **Determinism everywhere** — defined result ordering, deterministic error messages, no
  wall-clock or iteration-order nondeterminism in tests. The agent loop and cross-impl
  sync both depend on bit-reproducibility.
- **Structured errors**, not free text — so failures are machine-legible and
  `statement error` matching is stable.
- **Boring, explicit code over clever abstraction.** In Rust, resist deep generics and
  macro magic. In Go, resist over-interfacing. Flat, well-named, single-responsibility
  modules with small context footprints are easier for agents (and humans) to reason
  over than implicit cleverness.
- **Prefer Ruby and Rake for scripting and task running** — over bash and Make for
  build scripts, automation, codegen drivers, and dependency/task orchestration. This is
  a preference, not a prohibition: reach for bash or Make only when it is a *clearly*
  better choice for the job (a trivial one-liner, or a tool that specifically expects a
  Makefile). Ruby's readability keeps automation legible for agents and humans alike,
  consistent with "boring, explicit code over clever abstraction."
- **Spec-first per subsystem.** A subsystem's design doc + the relevant corpus is what an
  agent needs to work it without holding the whole engine in context.

---

## 11. Build order

1. **Scaffold the repo** with the `/spec` directory at its center (§6).
2. **Type-system spec** — scalar set + comparison/coercion matrix as a data table (§4).
   This is the product; everything tests against it. Forces the float/decimal/null
   decisions (§8) into the open.
3. **Conformance harness format + first corpus tier** (§7).
4. **Storage seam + key-encoding fixtures** (§8, §9).
5. **First vertical slice — the "it's alive" milestone:**
   `CREATE TABLE` / `INSERT` / `SELECT ... WHERE pk = $1`, **with integer columns only**
   (`int16`/`int32`/`int64`, §4), driven through **both** the Rust and Go cores against
   shared corpus entries. Proves the whole multi-core machinery end to end.
5b. **On-disk format + cross-core round-trip** — the single-file byte format
   (`spec/fileformat/format.md`) with byte-exact golden fixtures and the load-bearing §8
   test: each core writes bytes identical to a shared golden and reads the others'. Authored
   as a **whole-image** format (full serialize per commit); incremental commit deferred (§9).

Each step is independently testable and independently useful. There is deliberately no
point where progress is blocked on one giant subsystem.

---

## 12. Local reference sources (uncommitted)

Full source checkouts of other databases are kept locally for reading — as
differential-testing **oracles** (§7) and as **design references** (§8). They are **not**
committed (the workspace `references/` directory is in `.gitignore`).

> **Do NOT provision the references automatically.** Cloning the mirrors is a multi-GB
> download (PostgreSQL and DuckDB especially). Never run `rake references:setup` /
> `references:update`, or any other large download, on your own initiative — not to
> "be helpful", not as a side effect of another task. If a reference you need is not
> present in `references/`, either **work without it** or **ask the user to run
> `rake references:setup`** (or for permission to). The same rule applies to any
> heavy/expensive operation: surface it and let the user decide, don't auto-trigger it.

Provision or refresh them with Rake (§10):

```
rake references:setup    # clone mirrors (once) + check out worktrees into references/
rake references:update   # fetch upstream, re-point worktrees
rake references:status   # list repos, pinned ref, current HEAD
rake references:clean    # remove worktrees, keep the cached mirrors
```

**Storage model.** A bare `--mirror` clone of each repo lives on the **persist volume**
(`/persist/shared/references/<name>.git`, override with `REFERENCES_MIRROR_DIR`): full
history, downloaded once, shared across every container, survives rebuilds. The browsable
checkout in `references/<name>` is a **git worktree** of that mirror — it shares the
object store (no re-download, no duplicated history) but has its own HEAD, so a container
can check out a different branch/tag locally without disturbing the mirror or other
containers. Provisioning a fresh container is a cheap `git worktree add`, not a re-clone.

**What's checked out** (all free/OSS licenses):

| Repo | Ref | License | Why it's here |
|---|---|---|---|
| `postgres` | `REL_18_STABLE` | PostgreSQL License | Semantic oracle (§1, §7); `numeric.c` is the exact-decimal reference (§8). Pinned to match the live `postgres:18` service in `.devcontainer/docker-compose.yml`. |
| `sqlite` | `master` | Public Domain | The north star (§1); origin of the sqllogictest format (§7). |
| `duckdb` | `main` | MIT | Embedded DB that also uses sqllogictest — closest living architecture reference (§7). |
| `bbolt` | `main` | MIT | Single-file store whose single-writer / root-pointer-swap commit model matches §3 / §9. |
| `sqllogictest-rs` | `main` | MIT / Apache-2.0 | Reference Rust sqllogictest runner — useful for `impl/rust`'s harness (§7). |

PostgreSQL also runs **live** as the `db` service (a queryable oracle), separate from this
source checkout. **CockroachDB** is deliberately **excluded** despite being cited in §7/§8:
its core is BSL 1.1 (source-available, not OSI-free). For its key-encoding design, read it
from `spec/encoding/` or an old Apache-2.0 tag rather than vendoring the BSL source.
