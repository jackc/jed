# Project Design Brief

> This file is the standing context for all work in this repository. It is the
> load-bearing record of architectural decisions. Read it before making changes.
> When a decision here is revised, update this file in the same change.

The project name is **jed**. This document also refers to it descriptively as "the engine."

---

## 1. What we are building

An **embedded SQL database**. The one-line north star:

> **SQLite's footprint, PostgreSQL's behavior, and a real (strict, static) type system.**

The split is deliberate: take the *deployment model* from SQLite and the *observable
behavior* from PostgreSQL.

Properties:

- **Embeddable** — a library you link into a host program, not a server process. (SQLite's
  model.)
- **Single-file storage** — one database = one file on disk, like SQLite.
- **A deliberate, strict, static type system** — this is the product. Not SQLite's
  runtime type affinity; closer to PostgreSQL's, but stricter.
- **PostgreSQL behavior** — the semantics a query observes (NULL logic, comparisons,
  ordering, exact numerics, errors) match PostgreSQL.
- **SQL-first, not SQL-only** — relational SQL is the primary surface and *everything*
  must be reachable through it, but the storage layer is designed so SQL need not be the
  only access path (see §9).

**PostgreSQL is the behavioral default.** The standing rule, which settles most design
questions and divergence hotspots (§8) by default: **when a decision has an option that
matches PostgreSQL and there is no overriding reason against it, take that option.** This
covers three-valued NULL logic, exact decimals, comparison/ordering semantics (including
NULL sort position), error conditions, and the like — borrow PG's behavior rather than
reinventing it.

PostgreSQL is the default, **not a compatibility target.** We do **not** owe wire-protocol
compatibility, `pg_catalog` fidelity, the full PG type-coercion lattice, or arbitrary-SQL
coverage — we choose *which* surface to implement (**we own our surface**), and for the
surface we *do* implement, behavior tracks PostgreSQL. An **overriding reason** is a genuine
engineering tradeoff — simplicity, determinism (§10), the strict type system (§4), the
single-writer model (§3), or the memory-safety / cost-bound requirements (§13) — documented
at the point it is taken, not mere preference. Where the engine deliberately diverges from
PG, the divergence is recorded in the relevant spec doc.

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
   language. Pure Go: **no cgo, no FFI** (a standing constraint the dependency policy §14
   does not relax).

Rust and Go are about as far apart as two systems languages get, so this pairing does
the bulk of the honesty work. Build these two in genuine lockstep from the first
vertical slice.

**Two distinct goals, kept separate.** The honesty mechanism above justifies a *small
differential core set* — the maximally-different reimplementations whose disagreement
hardens the spec. That job is essentially done: Rust + Go do the bulk, and the native TS
core (#3 below) closed the two axes they agreed on by construction (`f64`+`BigInt` integers,
UTF-16 strings). Supporting **any further language is a different goal: give that language's
users the best experience.** The two goals are independent, and the second is *not* argued
on the first's terms — "it surfaces little new divergence" is not a mark against a reach
language, because hardening the spec was never the reason to add it.

3. **JS / TypeScript** — ✅ **landed as a native core** (`impl/ts`), at full parity with
   Rust and Go (every capability and conformance suite, and the byte-exact on-disk
   format — `rust == go == ts == ruby`). A Rust→WASM wrap remains an acceptable *production*
   fallback, but the native core is what stresses the spec — and it does: i64 is exact
   via uniform `bigint` (JS numbers are f64), names are UTF-8 (JS strings are UTF-16), and
   bytes are big-endian via `DataView`. Runs on modern Node by **native type-stripping**
   (no build step), TS limited to the erasable subset. The **Browser/OPFS storage host** (CLAUDE.md
   §9) has since **landed** in this core (`spec/design/hosts.md` §5): the engine runs in a Web Worker
   over an `OpfsBlockStore`, validated by file-host byte parity — proof the host-agnostic seam paid
   off. Rust, Go, and this TS core **are** the differential set; the honesty work is theirs.

**Beyond the differential set — best experience per language.** For every language past
core #3 the question is not "how much new divergence does it surface" (usually little) but
**"what gives this language's users the best experience?"** — a real per-language
engineering judgment between two first-class options:

- **Wrap the Rust core** when *performance* and *byte-exact-behavior-for-free* dominate: the
  engine runs at Rust speed and conforms by construction. Wrapping is a **first-class
  choice, not a fallback exception.** When wrapping, wrap the **safe Rust** core (§13).
- **Write a native core** when *cleaner, simpler integration* dominates: no FFI boundary,
  idiomatic in-process host-defined functions, a pure single-language package, and none of
  the per-platform native-artifact build/sign/ship burden.

Either way the **conformance contract still binds** (§7/§8): a native core must pass the
corpus and the byte-exact on-disk round-trip; a wrapped core inherits both from Rust. The
choice governs *which approach*, never *whether to conform* — though a wrapped core is a
distribution artifact, never an independent conformance voice (it can only echo Rust).
Decide per language, on the merits, and record the call in `spec/design/cores.md`.

4. **Java**, **C#**, **Swift** (and later reach languages) — **native or wrapped, chosen
   per language on best-experience grounds**, not by a blanket rule. Current leanings
   (`spec/design/cores.md` §2): **C#** is the strongest *native* candidate (value-type
   generics, `Span<T>`, NativeAOT → near-Rust speed with the cleanest managed packaging);
   **Swift** leans **wrap** (Apple packaging is well-trodden; native's one real edge is
   in-process host functions); **Java** is the most conflicted (wrap for performance
   pre-Valhalla, native for clean pure-JAR packaging). Two pivots decide it, and can pull
   apart: (a) **host-defined functions** — hot-path per-row favors native (no FFI/upcall per
   call), occasional/coarse favors wrap (engine at Rust speed); (b) **parallelism** — the §3
   immutable-snapshot read path is near-lock-free, so the question is cheap cross-thread
   sharing + CPU fan-out, where **wrapping Rust gives every host Rayon-grade intra-query
   parallelism for free** (and notably sidesteps Swift's ARC-contention problem), while
   native is strong for Go/C#/Java (GC-cheap sharing) and weak for Swift. Design the
   host-function API **vectorized/batched** so a wrap stays viable.

With Java/C#/Swift covered — native or wrapped — essentially every modern environment has a
first-class jed.

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
  `timestamptz`, `bytea`, **`uuid`** (a fixed 16-byte value), and `json`/`jsonb` if we
  want a headline feature.
  - **First implemented step — signed integers only:** `i16` / `smallint` (16-bit),
    `i32` / `int` / `integer` (32-bit), `i64` / `bigint` (64-bit). Canonical names state
    width in **bits** under the **`i`/`f` prefix** (`i16`/`i32`/`i64`, `f32`/`f64` — the
    Rust/Zig convention). The prefix is load-bearing: it makes jed's bit-namespace
    (`i8`…`i64`) **lexically disjoint** from PostgreSQL's byte-namespace (`int2`/`int4`/`int8`,
    `float4`/`float8`), so jed accepts **both** the SQL-standard words *and* PG's byte-shorthand
    as aliases (`int8` → `i64`) with no `int8`-means-8-bit-vs-64-bit collision, and a future
    8-bit `i8` stays free (the same property that lets a future `int8range` alias `i64range`
    without colliding with `i8range` — `spec/design/types.md` §2). The old jed names
    `i16`/`i32`/`i64`/`f32`/`f64` are a **clean break** — no longer accepted.
    Two's-complement, with trap-on-overflow (§8). Every other scalar above is explicitly
    **deferred** to a later slice. The float/decimal/collation decisions in §8 do not bind
    step 1.
  - **Beyond scalars — the `array` container is the second open-`Type` axis**
    (`spec/design/array.md`, `i32[]`, `ARRAY[1,2,3]`, `'{1,2,3}'::i32[]`). An array is a
    *container* layered over the element type, not a scalar — its own value codec, comparison
    rules, and (deferred) order-preserving key encoding. Two decisions distinguish it from
    composite. (a) It is a **structural** type — `Type::Array(Box<Type>)` carries the element
    type inline, no `CREATE TYPE`, no catalog object, no array-type id (contrast composite's
    *nominal* `Composite(catalog-ref)`); this is observably identical to PostgreSQL because array
    type identity is a bijection on the element type, and self-describing on disk. (b) Matching
    PostgreSQL exactly (§1), **array *shape* — dimensionality, lengths, lower bounds — is a
    property of the *value*, not the type** (`i32[3]` enforces nothing; a column holds arrays
    of mixed dimensionality), which relaxes "strict static" **only on shape** — the **element
    type stays static and strictly enforced**. Array comparison uses PostgreSQL btree NULL
    semantics (NULLs comparable, always a definite boolean), *not* composite's 3VL. Delivered
    S0–S4; arrays-as-key, multidimensional values, and the array function surface are deferred
    `0A000` follow-ons (`spec/design/array.md` §12).
  - **The type system is OPEN, not closed — composite (row) types have landed**
    (`spec/design/composite.md`, `CREATE TYPE addr AS (street text, zip i32)`). This is the
    pivot the scalar set above only hinted at: a type is no longer *only* a compiled-in
    `ScalarType` variant but can be **a fact about a database** — named, created/dropped at
    runtime, recursive, persisted in the catalog. So a column/value type is `Type { Scalar |
    Composite(catalog-ref) }`, the **open** wrapper threaded through parser/resolver/evaluator/
    codec/comparator/catalog in every core (the closed `ScalarType` enum is kept *intact inside*
    `Type::Scalar` — it never gains user variants). Composite is the first **container** axis and
    the shared open-`Type` foundation the future `array` axis reuses; **named composites only this
    slice** (no anonymous `record`), with composite-as-key deferred `0A000` (the text/decimal-PK
    precedent) and no implicit per-table row types (a documented PG divergence).
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

**Open types shift the contract in kind, not degree.** For scalars the cross-core contract is
"the data table is byte-identical" — `scalars.toml`/`compare.toml` are authored once and each
core is cross-checked against them. A composite type (§4) has no such fixed data table: its set
is per-database and only known at runtime, so its **recursive codec / comparator / NULL-rule /
text-I/O is hand-written per core** (the §-above "do not codegen" list now implicitly includes
it) and verified instead by **golden fixtures + conformance entries** (§8). This is sound because
every composite method is *derived* from field types that are already cross-core-identical
(`spec/design/extensibility.md` §4.1), so the byte-identity holds by construction; the data-shaped
part that *does* stay shared is the *field list*, persisted self-describingly in the on-disk type
catalog.

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
/web/                   # the jed website: static SvelteKit + Tailwind docs + live in-browser
                        # playground. A NON-CORE tooling module (the bench/ precedent, §14): its
                        # deps never touch a core manifest. Consumes the TS core (impl/ts) via a
                        # Vite alias and runs the engine in a Web Worker (in-memory + OPFS). See §10.
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
- **Concurrency is in the contract too — the schedule format.** The sqllogictest corpus is
  single-handle and sequential, so it cannot express the §3 concurrency model (concurrent
  readers vs. a writer, the reader-liveness watermark). A sibling **`# format: concurrency`**
  corpus (`suites/concurrency/`, `spec/design/concurrency-testing.md`) closes that: an explicit
  **total order over named read/write sessions** on one shared handle, **deterministic** because
  jed read results depend only on commit order + pin-points, never timing — so every core runs
  the identical schedule (a single-threaded core sequentially; a threaded core may enforce the
  same order under a race detector). It runs *inside* `rake ci` via the capability gate
  (`txn.shared`/`txn.read_handle`/`txn.watermark`/`txn.gate_blocking`). True-parallelism **stress**
  (random schedule, invariant-checked) is the separate bench-family Layer 3, *outside* `rake ci`.
  **Landed: Layers 1–3, all three cores.** Layers 1–2 run *inside* `rake ci` (stepped-sequential
  everywhere = the canonical result; the opt-in stepped-threaded mode on Go + Rust, one
  thread/goroutine per session under a turn token, run under the race detector by `rake
  concurrency:race`). Layer 2 adds the write-gate `open <sid> write blocks` annotation: a writer-open
  on the held single-writer gate, deferred to the gate-releasing step (the equivalent serial order) —
  and on Go/Rust the queued writer's thread parks inside the real `write()` on the gate under the race
  detector, the one concurrency path the sequential walk never exercises. This is what pulls
  concurrency — previously per-core hand-mirrored tests — back into the §2 differential net.
  **Layer 3 (`rake stress`)** is the bench-family parallel-stress harness, *outside* `rake ci`:
  `stress/*.stress.toml` workloads (concurrent writers + readers, no fixed order) run by one stress
  binary per core in the `bench/` modules (reusing the splitmix64 PRNG + FNV answer checksum), checked
  by per-snapshot invariants + a confluent final-state checksum that must agree across cores (Go under
  `-race`, Rust over real threads, TS via a seeded-sequential interleaver).
- **Bootstrap the corpus via differential testing against PostgreSQL.** The real PG
  service is the **result oracle** (§1): run a supported-subset query against it, capture
  output, emit a corpus entry. Generates a large, *correct* corpus cheaply. Where our
  semantics intentionally diverge from PG, override the expected output by hand and document
  why. **SQLite is deliberately *not* a result oracle** — it diverges from PG on exactly the
  surface that is jed's product (dynamic type affinity vs. strict static types, no exact
  `decimal`, silent integer→float promotion, NULL/3VL edges), so its answers would manufacture
  false divergences on the surface we care most about. SQLite's role here is the
  deployment-model north star (§1) and the origin of the sqllogictest format (above), not a
  semantic authority; the one oracle-adjacent use is mining its sqllogictest *test corpus for
  query shapes* (the answers still come from PG).
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
decisions; they are miserable to retrofit. **Default tie-breaker: match PostgreSQL** (§1) —
where one option matches PG behavior and nothing overriding argues against it, that is the
decision (e.g. NULL sorts last / NULL is the largest value, `spec/design/encoding.md`). The
biases below are where an overriding reason *does* steer away from PG.

- **Float formatting** — every language prints `f64` differently. Decision bias: keep
  binary floats **out of the comparison and text-output paths entirely**; lean on exact
  `decimal`. This aligns with "a real type system" and kills the worst offender. ✅ `decimal`
  has landed (`spec/design/decimal.md`) as that exact path; binary `float` stays deferred.
- **Decimal rounding** — ✅ **decided: round half away from zero** (PostgreSQL `numeric`;
  `0.125 → 0.13`, `2.5 → 3`), one mode engine-wide, applied to scale coercion / casts /
  division (`spec/design/decimal.md` §3). Result **scale** follows PG's per-operator rules
  (add/sub `max(s1,s2)`, mul `s1+s2`, div `select_div_scale`; §4).
- **NaN / infinity ordering** — for `decimal`: **excluded**, the type is always finite (no
  float source; `x/0` traps `22012`), so there is no NaN/∞ to order (a documented PG
  divergence — `spec/design/decimal.md` §2). Revisit only if a binary `float` type lands.
- **Collation** — start with ONE defined collation (byte/codepoint order is simplest);
  ICU-style collation is an explicit later feature.
- **Integer overflow** — defined wrap vs. trap.
- **Iteration-order leaks** — no hashmap iteration order may leak into the result *multiset*,
  values, types, names, errors, or cost. **Row sequence, however, is defined only by `ORDER
  BY`**: a query with no `ORDER BY` returns the correct *set* of rows in an **unspecified
  order** (SQL-standard and PostgreSQL behavior — §1; and what lets a query parallelize
  without a forced final sort). The determinism that matters is preserved — the multiset is
  exact and byte-identical cross-core, and the conformance harness compares such queries
  order-insensitively (`rowsort`). *With* `ORDER BY` the order is **fully** deterministic, ties
  included (broken by primary key).

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
- **Design target: durable on-disk databases whose dataset is RAM-sized.** Two facts hold
  at once, and neither alone is the picture. (a) **Persistent on-disk storage is the dominant
  mode** — the overwhelming majority of databases are durable files on disk, *not* ephemeral
  in-memory ones, so **durability is core** (crash recovery, ordered writes, fsync at commit),
  not an optional add-on; a pure in-memory database (no backing file) is a real but
  **minority** mode. (b) The dataset is **typically small enough to be fully resident in
  RAM** — so the in-memory representation stays a **first-class concern** (a fully-resident
  working set, not a partial cache of a larger-than-RAM file), and steady-state reads are
  served from memory. In short: a *durable disk database that happens to fit in memory*, not
  an in-memory engine with optional persistence. Persistence targets **SSDs**, not spinning
  disks: choose block/page size, on-disk layout, and write patterns for SSD characteristics
  (page-aligned I/O, write-amplification awareness) rather than HDD seek-minimization. This
  pairs with the staging-buffer commit model (§3): writes batch in memory and land **durably**
  on the SSD at commit — synchronously by default, or batched/deferred under a relaxed
  `synchronous` setting that decouples the fsync from the (always-atomic) commit boundary
  (`spec/design/transactions.md` §9; the commit *visibility* is unchanged, only fsync *timing*).
- **Must not preclude larger-than-RAM (TB-scale) operation.** RAM-sized is the dominant
  case, *not* a hard assumption: the engine must eventually handle a **TB-scale file whose
  data far exceeds available RAM without falling over**, and **nothing in the current design
  may foreclose that** — a standing non-foreclosure rule, like the encryption/compression
  doors below. The hooks that keep it open are already load-bearing and must stay so: the
  **page-structured on-disk format** (fixed pages, header-recorded page size, root pointer),
  the **storage seam** as a block/page interface, **order-preserving key encoding** (§8), and
  **per-page cost metering** counted as *logical* page accesses (so a cache/buffer pool stays
  invisible to the deterministic cost — §13). The concrete path (Phase 6, none foreclosed) —
  **incremental copy-on-write commit** (write only dirty pages) and **B-tree interior pages**
  (logarithmic, page-local lookup/scan) **landed in P6.1**, **page reclamation** in P6.2, the
  *logical* `page_read` cost unit in P6.3 — continues with: **demand paging / a bounded buffer
  pool** (the resident set becomes a cache of pages with eviction, not the whole file — **design
  landed**, [spec/design/pager.md](spec/design/pager.md), a *universal* pool reached
  seam-foundation-first; P6.4) and **streaming + spill-to-disk operators** (sort / hash join /
  aggregate / DISTINCT bounded by a memory budget, spilling when exceeded — the **`ORDER BY`
  external merge sort + its streaming single-table feed have landed**, [spec/design/spill.md](spec/design/spill.md),
  bounded by the `work_mem` handle setting; the spilling hash aggregate / `DISTINCT` / hash JOIN
  are follow-ons). The binding constraint on
  present work: **no code above the storage seam may harden a full-residency assumption** — no
  "load = read the whole file into one buffer," no operator that *requires* its entire input
  or output to fit in RAM. Today's whole-image load/commit and flat record chain are
  deliberately-narrowed *current forms* (§11 step 5b), replaceable behind these seams — not
  the permanent shape.
- The core defines a **storage seam** (a block/file interface) that each host
  implements: `os.File` in Go, the **Browser/OPFS host** (`FileSystemSyncAccessHandle`, ✅ built in the
  TS core — engine-in-a-Web-Worker, `spec/design/hosts.md` §5), direct file access natively.
  Designing this seam early is what makes "single-file, embeddable, everywhere" work — the OPFS host
  landed as *an added `BlockStore`*, not a reshape, exactly as intended. The
  **formal host interface** — the five-method `BlockStore` byte device, the per-core mapping,
  the host catalog, and where the encryption codec / replication tee sit — is
  `spec/design/hosts.md`; the per-core `Pager` (buffer pool, preallocation, fault seam) is the
  host-independent policy above it.
- **Keep the storage model pluggable behind the relational layer.** SQL is the primary
  access path and everything MUST be reachable via SQL (§1), but it is not assumed to be
  the only one. The architecture should not foreclose: (a) **multiple physical layouts** —
  row-oriented now, with column-oriented or key-value stores as possible per-table
  alternatives later; or (b) a **low-level direct access API** beneath SQL (e.g.
  `value = getValue("tableName", key)`, direct row read/write). Whether either ships is
  **undecided** — the requirement is to keep the seam open, not to build them now.
- **Leave the door open for encryption at rest (file-level).** The single-file format and
  the storage seam (storage.md §2) are kept so whole-file (or per-page) **encryption at
  rest** can be added later **without a reshape**. Not built now; the only present requirement
  is that nothing foreclose it (don't assume page bytes are plaintext-comparable on disk) —
  already satisfied (hosts keep page bytes opaque). **Designed in `spec/design/encryption.md`:**
  a page codec **in the core above the block seam** (not a per-host duty), a standardized AEAD
  under a **deterministic `(page_index, txid)` nonce** that keeps the §8 cross-core
  byte-identity (the asymmetry with LZ4 — AEADs *are* standardized, so a library agrees
  byte-for-byte), the auth tag *closing* the tamper gap the `format_version` 7 CRC leaves open,
  and the key as a handle setting the engine never persists. When it lands, the crypto comes
  from a **vetted library, never a hand-rolled algorithm** — the dependency policy (§14), the
  build gate.
- **Replication — block-shipping, no WAL (`spec/design/replication.md`).** A door kept open,
  not built; the architecture is **decided**: replicate by shipping the **per-commit
  page-delta** (the dirty pages + meta swap §3's commit already produces), in `txid` order, as
  a tee at the block seam — **not a write-ahead log**. A WAL is unmotivated because copy-on-write
  + the root swap already give atomicity *and* lock-free reader/writer concurrency (the two
  reasons to grow one), and the block-delta inherits the §8 byte-identity (it applies
  byte-identically on any core/host) and the §3 atomic-apply recipe. The tee sits **below** the
  encryption codec so a backup replica can be **keyless**. The trade is write-amplification
  (a one-byte change ships whole pages); a **logical** changeset stream (compact wire,
  heterogeneous consumers) is a separate higher-layer door, not foreclosed and not scheduled.
- **Compression of large values — ✅ built (large-values Slice B).** Large values (long
  `text`, `bytea`, big `decimal`, future `json`) are **compressed** transparently at the
  storage layer with a **hand-rolled, byte-pinned LZ4-block codec**
  (`spec/fileformat/lz4.md` + `lz4_vectors.toml`): a record over `RECORD_MAX` compresses its
  largest values first and externalizes only what still doesn't fit
  (`spec/design/large-values.md` §13). Deliberately **no compression library** — LZ4
  *encoders* are not standardized, so a per-core library would break the §8 byte-identity
  the goldens and the deterministic cost depend on (the §14 analysis, recorded in
  large-values.md §6); the work is metered by the `value_compress`/`value_decompress` cost
  units (§13).
- On-disk format and key encoding are spec'd with byte fixtures (§8). **Status:** the
  single-file on-disk format is authored in `spec/fileformat/format.md` and is now the
  **page-backed copy-on-write B-tree** (`format_version` 2, Phase 6): each table's rows are an
  on-disk B-tree (leaf + interior node pages) and a commit is **incremental** — it writes only
  the dirty pages a mutation introduced and publishes the new root by alternating the meta slot
  (P6.1), rather than rewriting the whole image. **Page reclamation** (P6.2) reconstructs a
  **free-list** of dead pages on open and the commit allocator reuses them, so a file no longer
  grows without bound. All three cores (Rust, Go, TS) **and** the Ruby reference read/write
  byte-identical files, verified against shared golden fixtures (the §8 cross-core round-trip;
  the goldens pin the clean *from-scratch* image). The double-buffered meta page + root pointer
  are the hooks the incremental commit model (§3) uses. **Landed since:** demand paging / the
  bounded buffer pool (P6.4), **large values** (`format_version` 3 — out-of-line overflow
  chains + transparent LZ4 compression, `spec/design/large-values.md`), **CHECK constraints**
  (`format_version` 4 — the catalog check list, `spec/design/constraints.md` §4),
  **secondary indexes** (`format_version` 5 — the catalog reshape: an explicit primary-key
  ordinal list in key order plus per-table index lists, each index an on-disk B-tree of
  empty-payload records, `spec/design/indexes.md`), **UNIQUE constraints**
  (`format_version` 6 — a per-index flags byte carrying the `unique` bit; a UNIQUE
  constraint IS its backing unique index, `spec/design/constraints.md` §5 /
  `spec/design/indexes.md` §8), and **per-page checksums** (`format_version` 7 — the page
  header grows 12→16 bytes for a CRC-32/IEEE on **every** body page, so silent at-rest
  corruption of a catalog/node/overflow page is detected as `XX001` on read rather than
  served as wrong rows; through v6 only the meta slots were checksummed —
  `spec/fileformat/format.md` *Page header*, `spec/design/storage.md` §6), and **expression
  column DEFAULTs** (`format_version` 8 — a per-column flag marks a DEFAULT as a non-constant
  expression (`uuidv7()`, `1 + 1`) evaluated per row at INSERT through the per-statement seam,
  rather than a constant folded at `CREATE TABLE`, `spec/design/constraints.md` §2),
  **composite (row) types** (`format_version` 9 — kind-tagged catalog entries + a composite-type
  section + two-pass acyclic load, `spec/design/composite.md`), **array (`T[]`) columns**
  (`format_version` 10 — `type_code 15` + an inline element-type descriptor + the compact array
  value body, `spec/design/array.md`), **`FOREIGN KEY` constraints** (`format_version` 11 — the
  table catalog entry gains a per-table foreign-key list after the index list, referencing the
  parent table/columns by name/ordinal with an `on_delete`/`on_update` action byte; an FK owns no
  B-tree, so it adds no value-codec change, `spec/design/constraints.md` §6), and **sequences**
  (`format_version` 12 — a third kind-tagged catalog entry `entry_kind 2` carrying the name, six
  fixed i64 fields, and a flags byte; emitted composites→sequences→tables; a sequence owns no
  B-tree. `nextval` is **transactional** — the counter is a snapshot field that rolls back with its
  transaction, a deliberate PG divergence mandated by determinism.md §5, `spec/design/sequences.md`),
  and the **`serial` owned-sequence link** (`format_version` 14 — the sequence-entry flags byte gains
  a `has_owner` bit + a trailing owner table-name/column-ordinal; a `serial`/`bigserial`/`smallserial`
  column auto-creates an *owned* default-i64 sequence with a `DEFAULT nextval(...)`, so `DROP TABLE`
  auto-drops it and `DROP SEQUENCE` of an owned sequence is `2BP01`, `spec/design/sequences.md` §12).
  **Still deferred**
  (later Phase-6, none foreclosed): continuous within-session reclamation + on-disk free-list
  persistence (the P6.2 follow-ons). The from-scratch whole-image serializer survives as
  `create`'s initial write and the golden generator.
- **Host file API (Phase 7).** The embedding surface (`spec/design/api.md`) `open`s/`create`s
  a database file and `commit`s the whole image **durably** via temp-file + fsync + atomic
  rename + dir fsync (whole-image rewrite ⇒ rename gives all-or-nothing for free). `commit` is
  **explicit** and `close` does **not** auto-flush (uncommitted changes are discarded); an
  in-memory `commit` is a no-op success, so the operation stays uniform and forward-compatible
  with the future §3 staging-buffer transactions. Same shape across all three cores.

---

## 10. How to work in this repo (this is an AI-agent-first codebase)

The design is optimized for AI agents even more than for humans. In practice:

- **The conformance corpus is the contract.** Implement a feature as "make these corpus
  entries pass." A feature = one SQL construct, parsed + planned + executed + tested, as
  a **vertical slice**. That is the unit of agent work and the unit of cross-language
  porting. When a slice touches the PostgreSQL-comparable surface, oracle-check its rows
  (`rake corpus:check`) and record any deliberate PG divergence in the override ledger; when it
  adds a query optimization, add a metamorphic (NoREC) relation so the sweep keeps pace —
  neither grows on its own (`spec/design/conformance.md` §5/§8).
- **Put tests in the corpus by default; write a per-core unit test ONLY for what the corpus
  cannot express.** The corpus runs on **every** core, so one entry tests all cores at once —
  re-asserting the same *SQL-in → rows/error-out, PostgreSQL-agreeing* behavior as a per-core
  unit test adds **no** coverage and drifts N ways (the §5 trap, in test form). A per-core unit
  test earns its place **only** when the behavior is structurally out of the corpus's reach:
  a deliberate **PG divergence** (the oracle corpus is PG-clean, so a jed-stricter / jed-differs
  case cannot live there), **catalog/host introspection** (constraint/index names, ordinals,
  internal state), **on-disk / byte-level** checks (golden round-trips, key-encoding vectors),
  **cost-meter values** no cost suite pins, **host-API surface** (open/create/commit/close,
  param binding, cursors, concurrency handles), or **internal invariants** (tree/split shapes,
  page counts, spec-constant cross-checks). When unsure, write the corpus entry, not the unit
  test. The per-core `foreign_key` tests are the model: they keep only the divergences +
  introspection and leave the agreeing behavior (23503 at every write site, MATCH SIMPLE, the
  batch end state, 42830/2BP01) to `ddl/foreign_key.test`.
- **Determinism everywhere** — deterministic results (exact multiset, values, types, errors,
  cost), deterministic error messages, no wall-clock nondeterminism. **Row order is
  deterministic iff `ORDER BY` is present** (§8): without it the order is unspecified (the
  harness compares `rowsort`), so a query need not be force-ordered just to be testable — but
  everything *else* stays bit-reproducible, which is what the agent loop and cross-impl sync
  depend on. Determinism is **default-deny with a ledger** (`spec/design/determinism.md`): the
  few sanctioned relaxations are enumerated in `spec/conformance/determinism_exceptions.toml` —
  `f64` value/render (class A), and the **`uuidv4`/`uuidv7` generators** plus the **clock
  functions `now()`/`current_timestamp`/`clock_timestamp()`**, which read entropy + the clock through
  a **host-injected seam** (`spec/design/entropy.md`) and so stay *deterministic given the seam
  inputs* (tests inject a fixed seed + a fixed/advancing clock → byte-identical cross-core; production
  reads OS entropy + wall clock). The seam joins the storage and cost seams as the engine's third
  "host supplies it" boundary.
- **Benchmarks are wall-clock, never conformance.** `bench/` (`rake bench:setup/run/report`,
  [spec/design/benchmarks.md](spec/design/benchmarks.md)) compares the three cores against
  PostgreSQL and SQLite. Deliberately **outside `rake ci`** and the conformance contract
  (wall-clock is nondeterministic) — but answers are still checked: every result carries a
  cross-engine checksum and the report fails on any disagreement. When a perf-relevant
  feature lands, **add a benchmark** for it (the same growth obligation as NoREC relations);
  before/after a perf-sensitive change, **run the affected benchmarks** and report both
  numbers in the change description (`rake bench:diff` emits the before/after comparison
  as JSONL; `rake bench:html` / `bench:markdown` render a run — with deltas — for humans).
- **Keep the website (`/web`) in sync with the surface it documents.** The static SvelteKit site
  (§6) is a tracked downstream consumer of the user-facing surface, like the corpus and benchmarks.
  When a change **adds or alters a user-facing SQL feature or the host/embedding API**, update the
  corresponding `/web` docs in the **same change**: the relevant page under `web/src/routes/docs/`
  (an `api/` per-language page or a `sql/` live-panel page), its live example or per-language
  `CodeTabs` source (`web/examples/<topic>/{rust.rs,go.go,ts.ts}`), and — if the type/function/error
  set changed — nothing by hand, since the reference pages generate from the spec TOML. The site runs
  the **TS core in a browser Web Worker** (in-memory + OPFS), so its `web/e2e/` Playwright suite
  (`npm run test:browser` in `/web`) is the interactive-feature contract; run it when touching the
  bridge (`web/src/lib/jed/`) or a documented behavior. Internal-only changes need no `/web` update.
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
- **Multiple agent instances; sync through `origin`, not just shared memory.** Several
  Claude instances run in **separate devcontainers** that share the project memory directory
  and `project-status.md` (one `/persist` volume) but **not** a git working tree — each
  container's checkout drifts independently. `origin`
  (`git@edi.jackchristensen.com:repos/jed.git`) is a **private hub** every container can
  reach, so it is the propagation path. Standing convention (a deliberate, scoped exception
  to the harness "push only when the user asks" default — it covers *feature branches to this
  private origin only*, never `master` mid-slice and never a public remote): **push a feature
  branch to `origin` promptly** — `git push -u origin <branch>` right after the first commit,
  then `git push` after each subsequent one — so the work is fetchable everywhere and backed
  up. **Merge to `master` only when green** (`rake ci` / verify) and **push `master`
  immediately on merge**, so the master tip is never left local-only. **Memory references
  *pushed* state:** a "landed / committed at `<hash>`" note means pushed, and must say so
  (or explicitly "branch-only, NOT pushed — container-local") so the next instance knows
  whether `git fetch` will find it. Before trusting or continuing another instance's work,
  `git fetch origin` and verify against your own git (`git cat-file -t <hash>`, `git log`,
  `git branch -a`) rather than the prose. **Worktrees do not solve this** — they share an
  object store by absolute path on *one* filesystem, and `/workspaces` is container-local
  (only `/persist` is shared), so a worktree made in one container is invisible in another.

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
   (`i16`/`i32`/`i64`, §4), driven through **both** the Rust and Go cores against
   shared corpus entries. Proves the whole multi-core machinery end to end.
5b. **On-disk format + cross-core round-trip** — the single-file byte format
   (`spec/fileformat/format.md`) with byte-exact golden fixtures and the load-bearing §8
   test: each core writes bytes identical to a shared golden and reads the others'. Authored
   as a **whole-image** format (full serialize per commit); incremental commit deferred (§9).
6. **Row mutation — `UPDATE` and `DELETE`** (integer columns), in both cores against a
   `mutation` conformance profile. `UPDATE` is in-place value replacement and is
   **two-phase / all-or-nothing**: every matching row's new values are type-checked
   (22003/23502) before any are written. Two deliberate, documented narrowings, relaxable
   later: (a) assigning a **PRIMARY KEY column is rejected** (`0A000`) so a row's storage
   key never changes; (b) there is **no cross-statement transaction** yet (the §3
   staging-buffer model is still future) — UPDATE's two-phase pass gives per-statement
   atomicity without it. The no-PK synthetic rowid became a **monotonic counter** (never
   reused), reconstructed on load, so `DELETE` then `INSERT` cannot collide.

Each step is independently testable and independently useful. There is deliberately no
point where progress is blocked on one giant subsystem.

**Forward work is tracked in [TODO.md](TODO.md)** — the roadmap of features beyond step 6,
ordered roughly by dependency / importance / difficulty. **Consult it when planning any new
feature** and confirm the work fits the overall plan (and the commitments in this file);
update TODO.md in the same change when the plan moves.

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
source checkout. The oracle is reached over a **Unix-domain socket** shared into the devcontainer
(`PGHOST=/var/run/postgresql`, trust auth) — the connection env is preconfigured, so just run bare
`psql` or `rake corpus:check[<repo-root path>]` and **never override `PGHOST`** (the recurring
foot-gun). The `db` service name also resolves over TCP, but the socket is the path we use.
**CockroachDB** is deliberately **excluded** despite being cited in §7/§8:
its core is BSL 1.1 (source-available, not OSI-free). For its key-encoding design, read it
from `spec/encoding/` or an old Apache-2.0 tag rather than vendoring the BSL source.

---

## 13. Untrusted queries: safe to run

**A fundamental project requirement: untrusted SQL is safe to run.** A first-class use case
is a host exposing an ad-hoc query surface to its own users, so a query supplied by an
adversary — not just a careless one — **cannot do bad things**. "Bad things" is concrete and
bounded: it cannot violate memory safety, it cannot reach outside the database (no
filesystem, network, process, environment, or clock access beyond the sanctioned seams), and
it cannot exhaust resources. This is a **standing guarantee about the engine and its
built-in surface**, not a feature toggled on per query — every core upholds it by
construction. It rests on **three guarantees**, each below:

1. **Memory safety** — a crafted query cannot corrupt memory (every core is a memory-safe
   language).
2. **No dangerous built-ins** — the engine provides **no function or operator that can do
   bad things**: the built-in catalog is **pure and side-effect-free** (no I/O, no host
   reach, no nondeterminism outside the §10 entropy/clock seam). There is simply nothing in
   the surface to abuse.
3. **Bounded resources** — execution cannot consume unbounded resources: a **deterministic
   cost meter + ceiling** bounds work, and a **fixed parser nesting-depth limit** bounds
   native-stack recursion. The two are independent gates (one strikes during execution, the
   other before any cost is metered).

**Scope boundary — host/application-supplied functions are excluded.** This guarantee covers
the engine and its built-in surface *only*. The moment a host registers an
application-supplied function (§2; TODO.md Phase 7/9), that function is **opaque**
— the engine has no way to know what it does (it may touch the filesystem, the network, or
burn unbounded CPU). So host-defined functions are **outside** the untrusted-query safety
guarantee by definition: a host that exposes them to untrusted queries owns that decision and
its consequences. The engine's one mechanical defense is the cost contract — a host function
that does not declare its cost is admissible **only** on an unlimited (`max_cost = 0`) handle,
never the untrusted-query surface (`spec/design/cost.md` §6). Purity and bounded-resources
above are promises about *jed's* surface, not about arbitrary code a host bolts on.

### Memory safety — largely free, but a standing requirement

Every core is written in a **memory-safe language** (Rust, Go, TypeScript — and any later
core: Java, C#, or a Swift core whether native or **wrapping the safe Rust core**, §2). So
the engine is *reasonably* safe against malicious input without special hardening: a crafted
query cannot trigger a buffer overrun, use-after-free, or out-of-bounds read. Treat memory
safety as a **standing requirement**, not an accident — any future `unsafe` / cgo / FFI path
(or a non-memory-safe core) must be justified against this property (and against the
dependency policy, §14). It is also one more reason a **wrapped** core (§2) wraps the
*safe Rust* core rather than dropping to C.

This covers memory safety **only** — it bounds neither what a query can *reach* nor what it
can *consume*. Those are the next two guarantees.

### No dangerous built-ins — a pure, side-effect-free surface

The engine provides **no function or operator capable of doing harm**. Every built-in in the
function/operator catalog (`spec/functions/`, `spec/design/functions.md`) is **pure**: it
maps input values to an output value and **nothing else** — no filesystem access, no network,
no process or shell execution, no environment or host reach, no mutation of state outside the
expression. There is deliberately no `pg_read_file`-style escape hatch, no `COPY … TO/FROM`
to the host, no dynamic-language `DO`/extension loader — the surface is curated, and a
construct that *could* do bad things is simply **never added** to it. The lone sanctioned
window onto the outside world is the **entropy/clock seam** (§10, `uuidv4`/`uuidv7`,
`now()`/`clock_timestamp()`), which is host-injected, deterministic-given-its-inputs, and
reads *only* entropy + the clock — never arbitrary host state. Keeping the surface pure is a
**standing requirement on the catalog**, enforced the same way every other catalog property
is: a new function is added with its semantics stated as data (§5), and a function that
breaks purity does not belong in the built-in set. (Purity is also what makes the §10
determinism contract hold; the two requirements reinforce each other.)

This bounds what a query can *reach*. It does not bound what a query can *consume* — that is
the third guarantee.

### Bounded resources — deterministic cost meter + ceiling, and a depth limit

An untrusted query must not consume unbounded resources. Two distinct hazards, two
independent gates:

1. a pathological *amount of work* — a runaway scan, a cross join, an expensive expression
   evaluated over a huge input — bounded by the **deterministic cost meter + ceiling**
   (below);
2. pathological *nesting depth* — input like `1 + 1 + … + 1` thousands deep, or deeply nested
   parens / `ARRAY[…]` / `CASE` / scalar subqueries — which would **overflow the native call
   stack during parse/resolve, before any cost is metered**, bounded by a **fixed parser
   nesting-depth limit** (`MAX_EXPR_DEPTH`, abort `54001` `statement_too_complex`,
   `spec/design/cost.md` §7). The cost ceiling structurally cannot catch this (the overflow
   precedes metering) and memory safety does not either (a stack overflow is an abort, not a
   memory error), so the depth limit is its own gate.

The cost meter is the primary mechanism. The engine must **deterministically meter the cost
of executing a query** and **abort when a caller-supplied ceiling is exceeded**.

- **Deterministic cost.** Execution accrues a running cost from defined units — each **page
  read**, each **row produced**, each **function/operator evaluation**, etc. (the unit
  schedule is spec'd as data, like everything mechanical — §5). The cost of a given
  `(query, database state)` is **fully deterministic**: the same query against the same
  database always yields the **same** cost, with no dependence on wall-clock, allocation, or
  iteration order (§10).
- **Cross-core identity.** Because there is no reference implementation (§2), cost is part
  of the shared contract: **every core must compute the identical cost** for the same
  `(query, database)`. This makes cost a §8-style divergence hotspot and a candidate for the
  conformance corpus (assert the cost, not only the rows).
- **Ceiling + abort.** A caller may set a **maximum cost**; the instant accrued cost reaches
  it, execution **aborts deterministically** with a defined error code (registered in
  `spec/errors/`). The abort point is itself deterministic (same query + db + ceiling → same
  abort).
- **Bake the seam in early.** Enforcement and tuning are *not* needed immediately, but the
  **metering seam threads through the executor, expression evaluator, and storage reads**,
  so it is far cheaper to carry the cost counter from early than to retrofit it across a
  grown executor. Design the seam early even if the ceiling/limits API lands later. Tracked
  in [TODO.md](TODO.md).

---

## 14. Third-party dependencies

The engine core is written **from scratch** (§2): the parser, planner, executor, storage
layer, type system, and expression evaluator are hand-written in every core and are
**never** delegated to a library (the same irreducibly-per-language list §5 forbids
codegenning). That is the project's identity and it does not change. This section governs
the **narrow** remainder — bounded, mechanical utilities at the edges (cryptography,
compression, and the like) — where pulling in a dependency is the right call rather than
reinventing it N times.

**A third-party dependency is allowed only when at least one of these holds:**

1. **All cores can be made to match.** A dependency (or a per-language equivalent) is
   available across **every** core such that behavior stays **byte-identical and
   deterministic** (§2/§8) — adding it does not make the cores disagree. Here "platform"
   means a *language core* (Rust, Go, TS, …), not an OS.
2. **A platform-specific implementation is significantly faster.** For a given core, a
   library native to that language's ecosystem is *significantly* faster than a hand-rolled
   equivalent — **and still produces output identical to the spec and to the other cores**
   (§8). "Significantly," not marginally; a small speedup does not justify a dependency, and
   speed never buys a divergence.
3. **Cryptography.** We do **not** roll our own crypto. Encryption/hashing primitives (the
   §9 encryption-at-rest door) come from a vetted, well-reviewed library, never a
   hand-written algorithm.

The `bench/` harness modules (`bench/go`, `bench/rust`, `bench/ts`) are **not engine
cores**: each is a separate module whose dependencies (PG/SQLite drivers, TOML parsers —
[spec/design/benchmarks.md](spec/design/benchmarks.md) §7) never touch a core's manifest.
The pure-Go no-cgo rule binds the Go *core*; the `bench-sqlite-cgo` baseline uses cgo
inside the bench module only. New bench dependencies still require explicit confirmation.

**Always get explicit human confirmation before adding any dependency.** A dependency is a
standing maintenance and supply-chain commitment across every core; it is **never** added on
an agent's own initiative — same spirit as the heavy-operation rule in §12. Propose it, name
which clause above justifies it and why, and wait for a yes.

**Guardrails that bind every dependency, no exceptions:**

- **Memory safety (§13).** A dependency must not introduce an `unsafe` / cgo / FFI or
  otherwise non-memory-safe path. The Go core stays **pure Go — no cgo, no FFI** (§2); a
  dependency does not relax that.
- **Determinism + cross-core byte-identity (§8).** A dependency may never leak
  nondeterminism (iteration order, float formatting, locale/library-version-sensitive
  behavior — the ICU-collation trap in [spec/design/types.md](spec/design/types.md) §11 is
  the cautionary tale) nor make two cores diverge. Clause 2's speedup is conditioned on this.
- **Bounded surface.** Dependencies live at well-defined edges (crypto, compression),
  never inside the parser / planner / executor / type-system core (§2/§5).
