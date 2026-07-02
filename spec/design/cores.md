# Cores vs. wrappers — design

> When does a new implementation earn its place, and should it be a native core or a wrap
> of the Rust core? There are **two independent goals**, and conflating them is the trap
> this doc exists to prevent:
>
> 1. **Harden the spec** (CLAUDE.md §2) — served by a *small differential core set* of
>    maximally-different reimplementations, judged by **how much new divergence they
>    surface**. Rust + Go + the native TS core already do this; it is essentially done.
> 2. **Reach a language** — give that language's users the **best experience**, judged by
>    performance and integration cleanliness, *not* by divergence. Native and wrapped are
>    **both first-class** here; wrapping is frequently the *best* answer, not a fallback.
>
> §1 below is the rule for goal 1 — it stops "more implementations = more coverage, so add
> Ruby/JS early" before it costs N-times effort for ~zero contract value. §2 adds goal 2 and
> the native-vs-wrap rule that follows from it. The old instinct this doc fought (don't add
> cores for ~zero *contract* value) was right about goal 1 and silent about goal 2 — which
> is the gap this revision closes.

The honesty mechanism (CLAUDE.md §2) is **divergence under a shared contract**: two
maximally-different implementations evolving together turn every spec ambiguity into a
failing test the day it is written. Everything below follows from taking that literally.

## 1. The principle: a core only earns its place if it can *disagree*

The value of an implementation, *for the spec*, is proportional to the new divergence it
can surface. The corollary is sharp:

> **A wrapper cannot disagree with the thing it wraps, because it *is* that thing.**

A Ruby gem over the Rust core, or a JS package over Rust→WASM, runs the exact same
parser / planner / executor / storage bytes. It surfaces **zero** new *semantic*
divergence. What it does test is the FFI/binding seam and packaging — real value for
**shipping** to an ecosystem, no value for **spec-hardening**.

This is the same reasoning CLAUDE.md §2 already applies to Swift ("wrapping the Rust core
is an acceptable *deliberate exception*, not a continuation of the rule"). This doc just
generalizes it: the rule is not "Swift is special," it is "wrappers don't harden the
contract — only independent reimplementations do."

## 2. Two goals, two buckets — and why a wrapper is first-class for the second

Keep the goals separate; they answer different questions and are funded by different
budgets. A given artifact serves one or the other.

| Artifact | Goal it serves | New divergence | Conformance voice? |
|---|---|---|---|
| Rust core, Go core | Harden the spec (§2) | High — the point | Yes |
| Native TS core | Harden the spec | Medium — the new axes (§3) | Yes |
| A native Java / C# / Swift core | **Reach that language** well | Low (the axes are taken) | Yes — it still conforms |
| Rust core wrapped for Swift/Ruby/… | **Reach that language** well | None — it *is* Rust | No — echoes Rust |

The first two rows are the **differential core set** (goal 1); their worth is the divergence
they surface, and §3–§5 are about choosing and timing them. Everything below the line serves
**goal 2 — best experience for that language** — and is judged on entirely different terms.

**For goal 2, native and wrapped are both first-class.** The old framing ("distribution
artifact," a grudging fallback) undersold it: wrapping the Rust core is frequently the
*best* answer for a language, not a consolation prize. The decision is a real per-language
engineering judgment between two poles:

- **Wrap the Rust core** when **performance** and **byte-exact-behavior-for-free** dominate.
  The engine runs at Rust speed and conforms by construction; you build a binding +
  packaging layer, not an engine. Wrap the **safe Rust** core (memory safety, CLAUDE.md §13).
- **Write a native core** when **cleaner, simpler integration** dominates: no FFI boundary,
  idiomatic in-process host-defined functions, a pure single-language package, and none of
  the per-platform native-artifact build/sign/ship burden.

Two invariants hold regardless of which pole you pick:

1. **Conformance still binds.** A native core must pass the corpus and the byte-exact
   on-disk round-trip (CLAUDE.md §7/§8); a wrapped core inherits both from Rust. The choice
   is *which approach*, never *whether to conform*. With persistence the dominant mode (§9),
   this also covers **durable commit + crash recovery** — hard to get right, owned by each
   native core but inherited free by a wrap.
2. **A wrapper is never a conformance voice.** It can only ever echo the core it wraps (§1),
   so it does not vote in the corpus — but that is a fact about goal 1, not a demerit for
   goal 2.

Which pole wins is decided mainly by **two factors that can pull in opposite directions** —
how host functions are called (§2.1) and how the workload parallelizes (§2.2). Weigh both
per use case; a host-function-heavy workload pushes toward native while a
parallelism-heavy one pushes toward wrap, and many workloads want both.

### 2.1 Deciding factor — host-defined functions

Across Swift, .NET, and the JVM the native-vs-wrap call turns in part on how the host
registers its own SQL functions:

- **Native core** → a host function is an ordinary in-process closure/delegate/lambda:
  inlinable, no marshaling, no boundary, clean memory safety. If host functions are
  **hot-path per-row**, native wins decisively.
- **Wrapped core** → every host-function call is an **upcall** back across the FFI boundary
  (engine → host), marshaling each argument and return value. For managed runtimes (JVM,
  .NET) this is worse than for Swift: the upcall also pays **thread-attachment** and
  **GC-safepoint** costs. If host functions are **occasional/coarse**, the engine-at-Rust-
  speed wrap wins and the upcall tax never matters.

The mitigation that keeps a wrap viable: **design the host-function API vectorized /
batched** (a column of values per crossing, not one row at a time), so the boundary is
amortized. Decide this early — it is what determines whether wrapping is on the table at all.

### 2.2 Deciding factor — parallelism (where the threads live)

jed's §3 model does most of the work here, and it changes the question. A writer stages
privately and commits by an atomic root swap, so the committed state readers see is
**immutable while they run**. The read path is therefore **nearly lock-free by
construction** — the only synchronization is the commit swap (a single atomic / brief
writer-exclusive window). So the differentiator is *not* "how good are the language's
locks"; it is two other things:

- **How cheaply can the language share immutable state across threads?** Rust shares an
  `Arc` snapshot for one atomic bump per *query* (traversal then borrows — no per-node
  refcount); a GC runtime (Go, C#, Java) shares plain references for free. **Swift's ARC is
  the outlier** — every traversal touches an *atomic* refcount on the shared nodes, and
  atomic refcount traffic on hot shared objects across many cores is exactly the cache-line
  contention that kills parallel read scaling.
- **Where do the worker threads live?** This splits the two kinds of parallelism:

**Inter-query** (N read queries at once — the common case, implied by §3). The *host* spawns
the concurrency; the engine need only be **safe to call concurrently against one immutable
snapshot**.
- *Native* uses the host's own primitive directly (goroutines / `Task` / threads / Java
  virtual threads), sharing the snapshot as ordinary references. Clean; slight edge to native.
- *Wrapped* works too (immutable snapshot, concurrent downcalls), but pays **marshaling
  throughput under concurrency** and the burden of *proving* the C API safe for concurrent
  calls (Rust's `Send`/`Sync` guarantees stop at the FFI boundary). Host-function upcalls
  compound the per-thread attach/safepoint cost.

**Intra-query** (split one expensive scan/join across cores). Now the *engine* owns the
threads — and this is where wrapping shines:
- *Wrapped Rust* gets **Rayon-grade data parallelism for free, in every host, invisibly** —
  the host calls `Execute` once; Rust fans out internally over Rust-owned memory and returns
  one result. The host's concurrency model is irrelevant, and the parallel work **never
  touches the host's GC or ARC**. For **Swift this is decisive**: wrapping *sidesteps Swift's
  single worst characteristic for this workload* (ARC contention on shared structure).
- *Native* must reimplement it in-language, and quality varies sharply: **Go** (goroutines),
  **C#** (TPL / `Parallel` / PLINQ), and **Java** (ForkJoinPool / parallel streams) are all
  strong with GC-cheap sharing; **Swift** is the weak case (async/actor model is I/O-shaped,
  `Sendable` makes sharing fussy, ARC contention); the native **TS** core has effectively
  *none* (single-threaded event loop; `worker_threads` can't cheaply share the snapshot) —
  fine for its conformance-core role, but the browser/Node target is serial.

The relevant axis for **read parallelism** is **CPU fan-out, not async I/O**: persistence is
the dominant mode (§9), but the dataset is RAM-sized and warm reads are served from memory, so
steady-state read work stays CPU/memory-bound. `async`/`await` (Swift, C#, TS) addresses the
*other* axis — overlapping I/O — which here lives on the **commit/durability** path (fsync per
write) and **cold load**, both single-writer (§3), not on the parallel read path. So the
CPU-fan-out analysis above holds for reads. (The exception is the **larger-than-RAM regime**
— §9's TB-scale non-foreclosure: once reads miss a buffer pool and fetch from disk, read work
becomes I/O-bound and async prefetch *does* matter. A wrapped Rust core absorbs that paging
internally — async/io_uring in Rust, invisible to the host — one more hard part inherited by a
wrap rather than rebuilt per native core.)

**Determinism tax (both kinds).** Parallel execution is just another schedule, and §10/§13
forbid the schedule from changing anything: identical row order *and* byte-identical cost as
the serial run. So any intra-query parallelism needs a **deterministic partition + ordered
merge** and **schedule-invariant cost accrual**. Wrapping solves this **once, in Rust, and
every host inherits it**; a native core must re-solve it *and* match the other cores' cost
byte-for-byte (§13 cross-core identity) — a real conformance burden. The differential set
already hardens it from two directions (Rust threads vs. Go goroutines) before any reach
language inherits or re-proves it.

**One host-API auto-trait divergence: the prepared-statement plan cache.** The prepared-statement
plan cache ([api.md §2.4](api.md)) makes the **Rust** `PreparedStatement` `!Send` — it caches an
`Rc<SelectPlan>` (the plan is `!Sync` via a regex `Cell`, so `Arc` buys nothing). This is the one
place the cores' host-API thread-affinity differs: Go's is fine to share (GC'd), TS's is
single-threaded, but a Rust `PreparedStatement` can no longer be moved across threads (re-prepare
per thread — a cheap re-parse). It is a **non-regression** in practice: the whole Rust query/cursor
path (`Engine`/`Session`/`Rows`) is *already* `!Send` (holds `Rc`s), so a `PreparedStatement` that
was `Send` could not have produced thread-portable rows anyway. `Database` stays `Send + Sync` (it
mints a session per thread). A compile-time guard in the Rust core asserts the `!Send` intent so the
divergence is deliberate and visible.

### 2.3 Per-language leanings (current judgment)

- **C# / .NET — strongest *native* candidate, and parallelism confirms it.** Specialized
  generics over value types, `Span<T>`, `ref struct`, SIMD intrinsics, and NativeAOT let a
  native core run within ~1.5–2× of Rust *and* ship as a clean pure-managed NuGet package (no
  per-RID native asset, no P/Invoke boundary, in-process host functions). On parallelism it
  is strong both ways: TPL/`Parallel`/PLINQ for intra-query fan-out, and GC-cheap sharing of
  the immutable snapshot for inter-query — no marshaling chokepoint under high read
  concurrency. Native here is plausibly the *best experience*, not merely the honest one.
- **Swift — leans *wrap*, and parallelism strengthens that.** A wrapped Rust core (UniFFI +
  XCFramework) presents a clean Swift API, runs at Rust speed, and Apple's
  static-link/packaging path is well-trodden (the old CLAUDE.md §2 "asterisk"). ARC makes a
  native Swift core slower on hot paths *and* is its worst liability under parallel reads
  (atomic-refcount contention on shared structure, §2.2); wrapping sidesteps that entirely by
  doing the fan-out in Rust. The only thing that flips Swift to native is **hot-path per-row
  host functions** outweighing the parallelism win.
- **Java — most conflicted, now tilting a little toward native on parallelism.** Pre-Valhalla,
  erased generics force boxing or off-heap (Panama `MemorySegment`) to go fast, and JIT warmup
  hurts short-lived embedded use — so **wrap for performance**. But a native core ships as a
  clean pure-JAR with in-process host functions and no JNI/upcall tax, *and* the JVM has
  top-tier concurrency: ForkJoinPool/parallel streams for intra-query, virtual threads (Loom)
  for cheap massive inter-query concurrency, all with GC-cheap sharing. Valhalla (value
  classes) tips it further toward native over time.

None of these is final; each is a per-use-case call recorded when the core is actually
scheduled (TODO.md Phase 9). The two pivots can disagree — e.g. Swift with hot per-row host
functions *and* heavy intra-query parallelism is genuinely torn — so resolve them against the
specific workload, not in the abstract.

## 3. "Which language" = "which new axis of divergence"

*(This section is about **goal 1** — adding a core to harden the spec. For goal 2, best
experience per language, see §2. The two use different selection rules.)*

The marginal value of core #3 is not "another language" — it is **a data model the
current cores cannot disagree about because they agree by construction.** The axes that
actually generate spec-leakage:

- **Numeric tower.** Rust and Go *both* have native fixed-width two's-complement integers,
  so they agree on `i16/i32/i64` and overflow (CLAUDE.md §4/§8) almost for free.
  **JavaScript has no native i64 — only `f64` + `BigInt`.** A core whose only number is
  `f64` *forces* the spec to confront integer semantics the current pair quietly satisfies.
  This is the single highest-yield axis not currently exercised.
- **String encoding.** Rust and Go are both **UTF-8**. Java, C#, and JS are **UTF-16**
  internally. The moment `text` enters the type system (collation, length, codepoint
  ordering — CLAUDE.md §8), a UTF-16 core tests whether "codepoint order" was actually
  nailed down or merely happened to work in two UTF-8 cores.
- **Decimal / float formatting** (CLAUDE.md §8) — already the worst offender; a third
  language's number printer is another independent vote on the rule.

Ranked by *new* axis (not popularity, not ship-likelihood):

1. **JS/TS, native** — `f64`+`BigInt` numerics *and* UTF-16 strings: maximally unlike
   Rust/Go on the two axes that matter, **and** the browser is the one genuinely distinct
   *target environment* on the roadmap. Top pick by a clear margin.
2. **Java / C#, native** — UTF-16, JIT, checked/unchecked arithmetic, Java's historic
   no-unsigned. Solid, and they are the "every modern environment" coverage (CLAUDE.md §2)
   — but a second UTF-16/managed pair after JS has diminishing axis-yield.
3. **Python / Ruby** — arbitrary-precision integers are *an* axis, but neither is a target
   *environment* the project has named, so both are gem/package-over-Rust territory, not
   cores. (Ruby's runtime model — GC'd, dynamic — is nothing Go does not already cover.)

The takeaway: **Ruby and JS would *ship* as wrappers, and that is exactly why a *native*
version of them adds nothing — unless the native runtime's data model is uniquely
divergent.** That is true for JS (UTF-16 + f64/BigInt + the browser) and false for Ruby.
JS is the interesting case; Ruby is not.

## 4. Timing — two different questions

Goal 1 and goal 2 each have a *when*, and the answers are **opposite**, so do not run them
together. §4.1 is when to add a **hardening** core (parallel, early); §4.2 is when to build a
**reach** core (sequential, late). Conflating them — e.g. building C# "in lockstep" the way
Rust/Go/TS were — pays the parallel cost for none of the parallel benefit.

### 4.1 A hardening core (goal 1): type-system-early, not calendar-early

Both directions of the §2 logic are real; name them honestly.

- **For adding a divergent core early:** ambiguities are cheapest to fix while the spec is
  soft; a third core hardens foundational decisions (key encoding, file format, integer
  semantics) *before* they ossify.
- **Against:** CLAUDE.md §2 itself says later cores "reveal far fewer new ambiguities,"
  two cores already do "the bulk of the honesty work," and §5 names parser/planner/
  executor/storage as the irreducibly *per-language* cost. A third core multiplies the
  porting tax on **every** future vertical slice — precisely while the slice surface is
  changing fastest.

The synthesis: **the unique divergence axes of a third core do not bite until the type
system exercises them.** While the engine is integer-only (the current state — CLAUDE.md
§4 first step, §11 step 5/5b), there is no `text` (so UTF-16-vs-UTF-8 cannot disagree),
no `decimal`/`float` (no formatting fight), and integers are exactly where Rust+Go agree
by construction. Adding a third core *now* would mostly re-prove what the current pair
already proves, while taxing every slice.

**Trigger, therefore, is a milestone, not a date:** add core #3 when the type system grows
past integers — specifically when `text` (encoding/collation) and `decimal`/`timestamp`
(formatting) land (CLAUDE.md §4 deferred scalars). That expansion is the first moment a
native UTF-16, `f64`-only core catches what the current pair structurally cannot. **(Settled:
the native TS core landed on exactly this trigger; the differential set is now complete — §6.)**

### 4.2 A reach core (goal 2): sequential, triggered by contract maturity

A reach core (C#, Java, native Swift) surfaces little new divergence (§3), so the §4.1 "add
it early so ambiguities show up" logic **does not apply** — there is no hardening benefit to
buy with the parallel-porting tax. The economics invert:

- **Sequential, not parallel.** Building a reach core in lockstep would pay the §5 per-slice
  porting tax on *every* churning slice (`timestamp`, `array`, transactions, …) for **zero**
  hardening return (Rust/Go/TS already saturate it). Build it *against a stable contract*, not
  alongside a moving one.
- **Trigger = contract maturity — not project completion, and not now.** You do not need the
  engine "finished"; you need the parts that *reshape* to have settled: the **type system**
  (the product, CLAUDE.md §4), the **core query / DML / expression surface**, and the **file
  format + key encoding** (frozen, golden fixtures). Once those stop moving, later features
  *extend* the contract without reshaping it, and a reach core catches up on them cheaply (the
  surface no longer thrashes). That milestone is **earlier than "finished"** (don't wait for
  the last feature) but **later than now** (`timestamp`, `array`, and transactions are still
  foundational and ahead — a reach core today would spend its life chasing reshapes).
- **The build is an AI grind against the frozen contract — and doubles as a
  spec-completeness proof.** "Produce a conforming core from the spec + corpus + the three
  reference implementations" is exactly the well-specified, objectively-verifiable task agents
  are best at (the §10 loop, scaled). A successful from-spec build *is* evidence the spec is
  complete and unambiguous — the vindication of "the spec is the project" (CLAUDE.md §2). Done
  **once**, against a mature contract; not run continuously against a moving target.
- **"Pass the corpus" is necessary but not sufficient — differential-test the new core
  against the existing three.** The one real risk of building late: a corpus blind spot lets a
  reach core "pass" while subtly wrong in an uncovered area, and you have stopped looking for
  divergence (the differential set catches bugs only *because* three cores run every slice as
  it is written). Mitigation that turns the risk into an asset — run the new core through
  **generative / differential testing against Rust/Go/TS** (the Phase 8 oracle work, TODO.md),
  not just the static `.test` files: generated queries where all four must agree flush out both
  core bugs *and* corpus gaps, so a late core still contributes hardening.
- **Wraps ship anytime; only native reach cores wait.** A wrapped core inherits behavior,
  format, durability, and cost byte-for-byte (§2), so it is correct against *any* spec version
  by construction — ship it whenever a language is wanted. Reserve the build-it-native-from-
  the-spec project for when the contract is frozen *and* the §2 best-experience analysis says
  native (C# first — §2.3).
- **If the goal is more hardening, build the Phase 8 generative harness, not a 4th core.**
  More cores have diminishing divergence yield (§3); more *coverage* does not. Test
  infrastructure is the higher-leverage hardening investment in the meantime.

## 5. The one genuinely open choice: native TS vs. Rust→WASM for the browser

CLAUDE.md §2 leaves JS "TBD" between a native TS implementation and a Rust→WASM wrap. §1–4
above resolve everything *except* this, because the two goals pull apart:

- For **divergence** (hardening the spec): only a **native** TS core helps; a WASM wrap is
  the Rust core and surfaces nothing.
- For **shipping** (a browser/Node artifact): a **Rust→WASM** wrap is the cheap, correct-
  by-construction path.

These are not mutually exclusive. The defensible split is: **maintain a native TS core as a
conformance participant *and* ship the browser build as Rust→WASM.** The alternative —
ship WASM only, no native TS — is legitimate, but it means the browser target receives
**zero** divergence-testing. That should be a deliberate choice, not a default fallen into.
**This is the decision reserved for the maintainer**; everything else in this doc follows
from the §1 principle.

A **first concrete Rust→WASM artifact now exists** — `impl/wasm`, a `wasm32-wasip1` `cdylib`
wrapping the safe Rust core via a small C ABI (the same host-artifact shape as the Ruby gem's
extension, `impl/ruby/ext`). It is **not** a conformance voice (§1 — it *is* the Rust core), and it
does not pre-empt the decision above; it exists today as a **benchmark engine**
(`jed/wasm/wrap`, `spec/design/benchmarks.md` §7.2, driven from Node over `node:wasi`) and a proof
that the wrap path works end-to-end. Its answer checksums are cross-checked against the native cores
by the benchmark harness, so it is correct-by-construction in practice as well as in principle.

## 6. Current recommendation / status

The differential core set is **in place**: Rust + Go + the native TS core run in lockstep,
byte-exact (`rust == go == ts == ruby`), through the type system as it has grown past
integers (`text`, `decimal`, `bytea` — the axes §3 predicted would bite). **Goal 1 (harden
the spec) is essentially satisfied** by these three; a fourth core would mostly re-prove
what they already prove.

The live question is now **goal 2 — language reach**, governed by §2, not §1:

1. **Do not add another core *for the spec*.** A fourth differential core has low marginal
   divergence yield (§3's axes are taken). New languages are now justified by **best
   experience for their users**, not by spec-hardening.
2. **Java / C# / Swift are reach decisions** (TODO.md Phase 9): native or wrapped per the §2
   rule, with **two pivots** — host-function hotness (§2.1) and parallelism (§2.2). Current
   leanings in §2.3 — C# native, Swift wrap, Java conflicted (tilting native on parallelism).
3. **When to build a native reach core — sequential, on contract maturity (§4.2), not in
   parallel.** Build it *after* the type system + relational core + format/encoding freeze
   (past `timestamp`/`array`/transactions), against the frozen corpus **and** the three
   reference cores — **differential-tested against them**, not just the static `.test` files,
   so a corpus blind spot can't hide a subtle bug. A successful from-spec build doubles as a
   spec-completeness proof. Wraps can ship earlier (they conform by construction, §2); for more
   hardening *meanwhile*, build the Phase 8 generative harness, not a 4th core.
4. **Ship Ruby / browser-JS as wrappers** (gem → Rust, package → Rust-WASM) when the
   ecosystem is wanted; the native TS core already covers the JS *conformance* voice (§5). The
   **Ruby gem** has begun on this path — slice 1 (the gem + the C-ABI/`fiddle` FFI seam over the
   safe Rust core) is built; see [ruby.md](ruby.md) and TODO.md Phase 9.
5. **Browser build** remains the §5 split: native-TS-for-conformance *and*
   Rust-WASM-for-shipping.

This doc records the *rule*; CLAUDE.md §2 remains the canonical priority list. If the rule
here and §2 ever conflict, fix both in the same change (per the CLAUDE.md preamble).
