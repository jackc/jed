# Determinism — design

> The determinism contract and its **sanctioned exceptions**. CLAUDE.md §8/§10/§13 require
> deterministic, cross-core-byte-identical results; this doc is where that requirement is
> decomposed into the distinct guarantees it actually bundles, and where the few places jed
> deliberately relaxes one of them are enumerated, bounded, and given a test mechanism.
>
> **Status: this doc is a framework + ledger.** The four-guarantee model and the
> already-existing carve-out (row order without `ORDER BY`) are **ratified**. The clock/entropy
> seam (§5) and the exception ledger (§9) have **landed**, and **binary floats** (§6, class A)
> have **landed** across all three cores. Plan-dependent observables (§8) are **ratified** by
> specifying the plan; they remain inside G1/G2 rather than becoming an exception. Still open:
> the `order_sensitive` catalog flag and the rest of the parallelism framework (§7). Each section
> marks its status. When any decision here is ratified, update the data it points at,
> [conformance.md](conformance.md) §4, and [CLAUDE.md](../../CLAUDE.md) §8/§10/§13 in the same
> change (§10 below lists the edits).

The honesty mechanism of the whole project (CLAUDE.md §2) is *divergence under a shared
contract*: with no reference implementation, the only thing that says two cores agree is
that they produce identical results on the same shared tests. Anything that relaxes that
contract is therefore touching the project's spine and must be done **deliberately,
narrowly, and on the record** — never as an accident discovered at runtime.

---

## 1. Determinism is four guarantees, not one

CLAUDE.md and [conformance.md](conformance.md) §4 today speak of "determinism" as a single
property. It is really **four independent guarantees**, and relaxing "determinism" is
always relaxing *one of them* — confusing them is what makes the relaxation feel
all-or-nothing when it is not:

| # | Guarantee | What it promises | Why it matters |
|---|---|---|---|
| **G1** | **Reproducibility** | same `(query, db, inputs)` → same result, every run, on one binary | the most basic promise; the agent loop and regression tests depend on it |
| **G2** | **Cross-core identity** | Rust == Go == TS == Ruby, byte-for-byte | **the honesty mechanism (CLAUDE.md §2)** — the sacred one |
| **G3** | **Cross-platform identity** | same core, different CPU/arch → same bytes | golden fixtures must not depend on the build target (the FMA-fusion hazard, §6) |
| **G4** | **Contract granularity** | *what the corpus asserts* — multiset vs row-sequence, exact value vs epsilon, value vs property | sets how strong a claim a passing test actually makes |

**The default is that all four hold** — this is a **default-deny** contract (the §14
dependency-policy posture, applied to determinism). A feature relaxes a guarantee **only**
via an entry in the exception ledger (§9), justified and blast-radius-bounded. The sections
below enumerate the only relaxations jed contemplates; everything not listed keeps all
four.

The decomposition immediately sorts the candidate relaxations. Most "scary" cases turn out
to drop a *weaker* guarantee, or none:

- Row order without `ORDER BY` drops only **G4** (it was never in the contract) — §2, case **U**.
- `now()` / `random()` / `gen_random_uuid()` *look* like they drop everything but can be
  pulled back to keeping **G1+G2** by moving the non-determinism to a host seam — §5, case **B**.
- Binary float arithmetic is the *only* contemplated case that genuinely surrenders **G2**
  internally (a value with a right answer, computed differently per core) — §6, case **A**.

So the genuinely-irreducible surrender of the sacred guarantee is far narrower than "relax
determinism" suggests: essentially floats, plus any deliberately admitted parallel-strategy
exception. Plan identity is not in that set (§8).

---

## 2. The taxonomy of sanctioned relaxations

Five classes, by the **source** of the non-determinism. Each row states which guarantees it
keeps, and the test mechanism that still catches real bugs in it.

| Class | Source | Examples | Keeps | Drops | Test mechanism |
|---|---|---|---|---|---|
| **U** — Underspecified contract | SQL leaves it open | row order without `ORDER BY`; `LIMIT` without `ORDER BY`; `DISTINCT`/`GROUP BY` output order | G1, G2, G3 | G4 (sequence only) | `rowsort` (multiset compare) |
| **B** — Boundary inputs | host clock / OS entropy | `now()`/`current_timestamp`; `clock_timestamp()`; `random()`; `gen_random_uuid()` (v4); UUIDv7 | G1, G2 *(injected/seeded)* | — *(prod: G1, G2 on the raw draw only)* | inject fixed/advancing clock+seed → exact; else property/bounds (§5) |
| **A** — Approximate / internal | IEEE / libm / approximate algorithm | binary `float` compute (§6); future `approx_count_distinct`, `TABLESAMPLE`, percentiles | G1 *(per binary)* | G2, G3 | epsilon / reduced-precision `R` tag + PG-only oracle |
| **I** — Implementation identity | the core itself | `version()`, build/identity reflection | G1 | G2, G3 *(by construction)* | property / regex on format, never value |
| **P** — Plan / strategy dependent | deliberately underspecified physical strategy; parallelism | error *selection* among ≥2 candidate errors; parallel fold order (§7) | varies — see §7 | varies | serial==parallel metamorphic relation (§7); single-error test authoring (§8) |

Mapping to the original framing of these discussions: case **U** = "SELECT without ORDER
BY," **B** = "random and time-based functions," **A** = "inherently approximate/lossy types
or functions." Classes **I** and **P** are the additions; parallelism is **not** a class of
its own — it folds into **P** (and intersects **A** for float aggregates), see §7.

---

## 3. The non-members — what is *not* eligible

A taxonomy needs edges, or it becomes a license. Three things look like they belong but jed
deliberately keeps **inside** the deterministic contract by paying for it elsewhere:

- **Resource / memory limits.** Most engines let "out of memory / too deep" be
  environment-dependent. jed does **not**: [cost.md](cost.md)'s *logical* cost units
  (page-reads, rows, operator-evals — never bytes or wall-clock) make the ceiling
  deterministic and cross-core (CLAUDE.md §13). A limit hit is part of G1/G2, not an
  exception.
- **Collation / Unicode version.** The ICU-version-dependent-ordering trap
  ([types.md](types.md) §11) is exactly an exception jed refuses: it ships one fixed `C`
  collation precisely so ordering is table-free and version-independent. Linguistic collation
  has since landed ([collation.md](collation.md)) on exactly this footing: it **version-pins**
  the UCA/CLDR tables as jed's own shared spec data (CLAUDE.md §5) and the engine reads only
  those pinned bytes — whether compiled in or **loaded from a host-supplied bundle**
  (collation.md §9), the bytes are identical — turning it back into deterministic data, never a
  sanctioned exception.
- **Hash / iteration order.** This is **never** sanctioned — it is a *forbidden leak*
  (CLAUDE.md §8). The distinction from class **U** is sharp: no-`ORDER BY` leaves the row
  *sequence* unasserted while the *multiset* stays exact; an iteration-order leak would
  corrupt the multiset / values / types / errors / cost, which is always a bug. The
  forthcoming `jsonb` type ([json.md](json.md)) is the model for paying the cost: object
  members are stored in a **canonical key order** (length-then-bytewise, dedup last-wins,
  [json.md §2.3](json.md)) so the bytes and every key-enumerating render are a pure function
  of the value, never of hashmap order — and JSON numbers are exact **`decimal`**, never
  binary float ([json.md §8](json.md)), so JSON introduces **no new** sanctioned relaxation
  (its only seam-reads are the existing clock/entropy ones, the `.datetime()`/`_tz` path
  surface — [jsonpath.md §5.1](jsonpath.md), class **B** below).
- **Plan choice and estimates.** Independently implemented optimizers are not permission to pick
  different winners. [estimator.md](estimator.md) specifies the plan inputs, arithmetic, candidate
  order, and bounded search, so the selected plan, EXPLAIN estimates, actual metered cost, and
  deterministic error visitation remain G1/G2 facts. A mismatch has no class-P ledger escape.

---

## 4. The containment invariant — no contamination

This is the governance teeth. A sanctioned non-deterministic value must not silently
**promote** into the deterministic surface. The danger path: a non-deterministic value flows
through a `WHERE`, an `ORDER BY … LIMIT`, or a narrowing `CAST`, and changes the **row
multiset** — which then makes *other, non-exempt columns* (and cost, and error selection)
diverge. A ≤1-ULP float (§6) or a wall-clock read near a comparison boundary can flip a whole
row, contaminating integer and text columns that are not themselves exempt.

So every ledger entry (§9) states a **blast radius**, not merely "this may be
non-deterministic." The model is **taint that propagates**: a result carries the *weakest*
guarantee of its inputs, and the contract degrades only along the taint, never globally. The
binding invariant:

> **A query that does not use a sanctioned-non-deterministic feature stays fully
> deterministic and cross-core identical (G1–G3).** A query that does use one degrades only
> the columns/rows the taint reaches, bounded by the entry's stated blast radius.

Concretely this argues for keeping exempt values **out of order-determining positions** wherever
the value carrying the taint is itself non-deterministic. The subtlety is *which* float values are
exempt: a float **at rest** — a literal, stored sensor data, the output of the in-contract
`+ − * / sqrt` kernel or the exact-sum aggregates — is **fully in-contract** (G1–G3:
cross-core-byte-identical storage, total order, and `float-order-preserving` key bytes,
[float.md](float.md) §1). Only a **computed transcendental** (and float-gated control flow) is
exempt. So `float` keys (`PRIMARY KEY`/index/`UNIQUE`/FK — [encoding.md](encoding.md) §2.8) are
**allowed**: an ordinary float key sorts identically in every core, and the *only* contamination
path is the narrow case of storing a **tainted** (transcendental-derived) float into a key, which
extends that value's existing exemption from *query-time* order to *stored* order. That is a
**bounded widening of the `float-transcendental` blast radius** (§6/§9), recorded in the ledger —
not a new exemption, and not a reason to forbid the common, fully-deterministic case. (This
reverses an earlier stance that held float out of keys *permanently*; the reversal is sound because
the taint lives on the *transcendental*, not on *float-as-a-key-type*, and it is PG-faithful — PG
admits `float8`/`float4` btree keys. The golden on-disk fixtures store only in-contract float
literals, so they stay byte-identical across cores.) The composite container stays out of keys for
the *separate* reason that its key encoding is not yet exercised, not a determinism one.

---

## 5. Push it to the boundary — the clock and entropy seams (class B)

**Status: RATIFIED for the UUID generators** (`uuidv4`/`uuidv7`, [entropy.md](entropy.md); the
ledger entries `uuidv4-entropy` / `uuidv7-clock-entropy`) **and the clock functions** (`now()` /
`current_timestamp` / `clock_timestamp()`; the ledger entries `now-clock` / `clock-timestamp-clock`);
**proposed** for the rest of class B (a general `random()`), which reuses this exact seam when it
lands. This is the jed-idiomatic move and it collapses almost all of class **B** back into the
deterministic contract. Do not make *the engine* non-deterministic; make
the engine a **deterministic function of two new host inputs**, behind seams — exactly like
the storage seam (CLAUDE.md §9, [storage.md](storage.md)):

- **Clock seam** — `now()` / `current_timestamp` read it. The host injects a clock *function*;
  production returns the wall clock, tests inject a fixed instant (entropy.md §6). The injected path
  is fully G1+G2 (every core, given the same injected instant, renders byte-identical).
- **Entropy seam** — `gen_random_uuid()` (v4) and unseeded `random()` draw from it. The host injects
  a random-bytes *function*; **production's default reads the OS CSPRNG per value** (so output is
  unpredictable — not reconstructable from one sample, the security posture), and tests inject the
  engine's provided deterministic source.
- **A spec'd PRNG as shared data** (CLAUDE.md §5) — one algorithm pinned byte-for-byte, the way
  CRC-32 and the decimal limb arithmetic are already hand-rolled identically across cores
  ([decimal.md](decimal.md) §1). **Landed: splitmix64** ([entropy.md](entropy.md) §2,
  [../encoding/prng.toml](../encoding/prng.toml) + `prng_verify.rb`) as the engine's *provided
  deterministic source* (injectable for reproducibility, **not** the production default), so
  `uuidv4`/`uuidv7` are **cross-core identical** when it is injected; a future `random()` reuses it.

The result: the corpus tests `now()` / `random()` / `uuid` with **injected clock+random-source and
exact assertions** — G1+G2 preserved. The *only* irreducible production non-determinism left in
class **B** is the raw clock read and the raw per-value entropy draws; everything downstream (the
rendering of `now()`, the bit-layout of a UUID, the distribution of `random()`) stays in the
deterministic, honesty-mechanism-covered world.

**Stability scope.** Match PG: `now()` / `current_timestamp` are fixed for the **statement**
(one read, reused for every row), so a statement's time value cannot vary row-to-row and is
trivially parallel-safe (§7). **Landed:** `now()` reads the once-resolved statement clock and
`current_timestamp` is parser sugar for it; the per-call `clock_timestamp()` is the distinct,
separately ledgered function (`clock-timestamp-clock`) — it reads the clock seam on every call (a
fresh read that bypasses the statement-clock cache), and is tested with an injected **advancing**
clock so its per-call advance is deterministic and distinguishable from `now()`.

**Not class B — deterministic counters.** Sequences / `SERIAL` / identity columns and jed's
existing synthetic rowid counter (CLAUDE.md §11 step 6) are **fully deterministic** (a
monotonic counter, reconstructed on load) and stay inside the contract. Do not exempt them.

This is `now()` / `EXTRACT`-time-functions and uuid-generation as currently filed under
"deferred follow-ups" in [TODO.md](../../TODO.md) (the timestamp + uuid entries): the seam
is the design that lets them land without breaking G1/G2.

---

## 6. Binary floats — the only internal surrender of G2

**Status: landed — `f32`/`f64` are the first exempted types** ([float.md](float.md);
the design doc, type-system data, and exception ledger are authored and the types are landed
across all three cores). They are the *defining* members of class **A**: the one case where a value that **has** a right answer is
computed *differently per core* (G2) and per platform (G3) and cannot be injected away — but the
exemption is **narrow** (storage, total order, the `+ − * / sqrt` kernel, `MIN`/`MAX`/`COUNT`,
int/decimal `SUM`/`AVG`, and cost all stay in-contract; transcendental *values*, text-rendering
*layout*, and the float `SUM`/`AVG` *fold order* are exempt, all absorbed by the `R` tag's
tolerant compare). The ledger entries are in [../conformance/determinism_exceptions.toml](../conformance/determinism_exceptions.toml).

The hard surface and the resolutions taken (full detail in [float.md](float.md)):

- **Arithmetic kernel** (`+ − * / sqrt`) is correctly-rounded by IEEE 754 and therefore
  **already G2/G3-identical** across Rust/Go/TS — *if* fusion is defeated: no x87 extended
  precision, round-ties-to-even, no flush-to-zero, and **no FMA contraction**. Rust and TS do
  not contract by default; **Go does** (`(Add (Mul x y) z)` fuses on ARM64 always, amd64 at
  `GOAMD64≥v3`), so the same Go source diverges across platforms (a G3 break) unless each
  multiply-feeding-add is written `f64(a*b)+c` (the spec-blessed barrier; a named
  intermediate is *not* guaranteed). In a tree-walking evaluator this barrier is structural
  (the product is a rounded return value before the add sees it), so the real exposure is
  hand-written numeric kernels — i.e. aggregates and transcendentals.
- **Aggregation** (`SUM`/`AVG`) is non-associative for float → naive folds diverge across
  cores *and* across parallel schedules. The order-independent resolution (an exact
  accumulator, §7 A) is the hand-rolled cross-core numerical code [float.md](float.md) §1/§6
  declines to spend on the least on-brand type; the sort-then-fold stand-in (§7 B) is
  O(n·log n)+O(n) and not even correctly-rounded. jed therefore takes **§7 C — ledger it**
  (`float-sum-order`): float `SUM`/`AVG` folds as a **streaming scan-order running total**
  (O(1)), cross-core-identical on today's serial executor but with G2/G3 **withheld** so a
  future parallel plan may reorder the fold. A documented PG divergence (PG's float sum is
  likewise order-dependent). Int/decimal `SUM`/`AVG` and `MIN`/`MAX`/`COUNT` stay
  order-independent and in-contract.
- **Transcendentals** (`sin`/`exp`/`log`/`pow`) are not IEEE-correctly-rounded; every libm
  differs by an ULP → defer or exclude (or ship bit-pinned implementations later, at
  decimal-limb cost).
- **Text formatting** — shortest-round-trip *digits* are unique but *layout* differs per
  language; needs one hand-rolled formatter pinned with fixtures, or a fixed-precision form.
- **Ordering / keys** — adopt PG's total order (`-0 = +0`, `NaN = NaN`, `NaN` largest);
  order-preserving key encoding via the u64-bit trick with `-0`/`NaN` canonicalization.

If floats are ratified, the residue that genuinely can't be made G2-identical (chiefly
transcendentals, and float-gated control flow per §4) goes in the ledger with the **`R`**
render tag's reduced-precision comparison ([conformance.md](conformance.md) §1, the long-
reserved tag) and a **PG-only** oracle (not cross-core-exact). Everything else about floats
(storage, ordering, the disciplined kernel, exact-accumulator aggregates) stays inside the
contract.

---

## 7. Parallelism is an optimization, not an exception

**Status: proposed framework** (no core parallelizes yet). Some cores will eventually run
queries in parallel (Go goroutines, a Rust thread pool); the TS core may stay
single-threaded indefinitely. That asymmetry **forces** the rule, it is not a preference:

> The moment Go runs a query in parallel and TS runs it serially, **G2 requires the parallel
> result to equal the serial result** — otherwise Go-parallel ≠ TS-serial and the honesty
> mechanism fails on its own machinery. So **parallel execution is a sanctioned
> *optimization*, never a sanctioned *non-determinism*. It must produce a result observably
> identical to serial execution — except for operations already ledgered as order-sensitive,
> which parallelism *joins*, never *creates*.**

This is the NoREC contract ("an optimization must not change the result",
[conformance.md](conformance.md) §8) extended to parallelism.

**Most operations are order-insensitive → parallelism is invisible for free.** Commutative +
associative reductions, filters, projections, joins, set operations need no handling. The
work is isolating the order-**sensitive** operations and marking order-sensitivity as
**catalog data** (CLAUDE.md §5 — a proposed `order_sensitive` field on the aggregate rows in
[../functions/catalog.toml](../functions/catalog.toml)), so the planner has a rule per
operation rather than discovering sensitivity at runtime:

| Aggregate | Order-sensitive? | Resolution |
|---|---|---|
| `COUNT`, `MIN`, `MAX`, `bool_and`/`bool_or` | no (commutative+associative+idempotent) | parallel-safe, no work |
| `SUM`/`AVG` over **int** | no (widens to i64/decimal; intermediates don't overflow) | parallel-safe |
| `SUM`/`AVG` over **decimal** | no (**resolved**) | values associative; the fold checks the cap only on the **final** result (the `add_uncapped` accumulator path — [decimal.md](decimal.md) §2), matching PG → order-independent. (Previously trapped on the first over-cap intermediate, an order-dependent trap; that edge is closed.) |
| `SUM`/`AVG` over **float** | yes (value) | **C**: ledgered (`float-sum-order`) — streaming scan-order fold, order-independence declined as float over-investment (§6, float.md §7). Parallelism *joins* this existing exemption, never creates it. |
| `string_agg`, `array_agg`, `json_agg` | yes (inherently) | **B/C** below — concatenation order *is* the meaning |

**Three resolutions for an order-sensitive operation** (prefer the determinism-preserving):

- **A — make it order-independent** so parallelism is free. The prize for float `SUM`/`AVG`:
  the exact accumulator (§6). Reach for this whenever an order-independent definition exists.
- **B — pin the order** explicitly or canonically. For inherently-ordered aggregates there is
  no order-independent form, so determinism needs a defined order: PG's intra-aggregate
  `string_agg(x, ',' ORDER BY x)`, and absent that either a canonical order (by PK / encoded
  key — the parallel executor sorts the group before folding) or **C**. The `order_sensitive`
  flag tells the planner it must establish order before a parallel fold.
- **C — ledger it** (§9). Only when neither **A** nor **B** is viable.

**Test it the way optimizations are tested.** Add a **serial-vs-parallel metamorphic
relation** to the NoREC/TLP sweep ([conformance.md](conformance.md) §8): run each query
forced-serial and forced-parallel — and under varying worker counts / chunk boundaries — and
assert byte-identical results for everything not ledgered. A parallel fold that diverges from
serial fails the sweep the day it is written, the same independent-oracle guarantee that
already guards predicate pushdown.

**Two subtleties that bite:**

- **Seeded PRNG under parallelism.** The §5 fix (inject a seed → reproducible `random()`)
  breaks if a *shared sequential* stream is consumed in scan order: the sequence is
  reproducible but the *mapping of draws → rows* becomes scheduling-dependent. Fix: a
  **counter/key-based RNG keyed by a deterministic row identity** (`value = prng(seed,
  row_key)`), so each row's randomness is a pure function of `(seed, row)`, independent of
  which thread processes it — reproducible, parallel-safe, and cross-core identical on the
  seeded/test path. (Production-entropy mode is non-deterministic regardless.)
- **Error selection** — when ≥2 rows could trap, parallelism makes *which* error surfaces
  non-deterministic. Serial plan-dependent error selection is fixed by the ratified plan contract
  (§8); an underspecified parallel schedule would be the remaining class-**P** problem. Until that
  is resolved, error `.test` cases are authored with a **single offending row** so they stay
  deterministic. The related
  "does it error at all under `LIMIT`" edge (jed already has it — `SELECT DISTINCT 1/a … LIMIT
  1` traps where the plain form does not, [cost.md](cost.md) §3) is evaluation-strategy-
  dependent today; parallelism only widens it.

---

## 8. Plan-dependent observables (RATIFIED — specify the plan)

**Status: ratified.** Physical plans affect observable actual cost and may affect which of several
candidate runtime errors surfaces through evaluation/visitation order. Cost-based access and join
selection therefore cannot be an implementation-private heuristic: cross-core cost identity (G2)
requires plan identity.

Path B resolves this by **specifying the plan**, not by ledgering divergence. For a fixed resolved
query and visible estimator inputs, every core inventories the same legal candidates, computes the
same exact integer estimates, applies the same total tie order, and performs the same bounded join
search. [estimator.md](estimator.md) is the algorithm contract and
[../cost/estimator.toml](../cost/estimator.toml) owns its mechanical constants and orders. The
planner remains independently hand-written in Rust, Go, and TypeScript; the no-planner-codegen
boundary does not make its outputs discretionary.

The verification stack is deliberately redundant:

- shared estimator vectors pin per-unit counts, rows, weighted cost, and tie keys;
- EXPLAIN pins the chosen physical tree and per-node estimates;
- `# cost:` pins the actual runtime consequence; and
- each newly enabled plan choice adds a NoREC/metamorphic relation for result correctness.

A disagreement is a planner bug and gets no class-P exception entry. Deterministic error selection
follows from selecting the same plan and using each executor's already-fixed evaluation order. A
later plan-contract change may deliberately re-pin which error wins, but all cores must change
together. Error corpus cases should still prefer one offending row so a plan slice tests the
intended rule instead of accidentally pinning an unrelated visitation detail. Parallel execution
remains the separate proposed class-P problem in §7.

---

## 9. The ledger + admission criteria

**Status: landed (populated for classes A and B).** It mirrors the two precedents already in
the repo: the [oracle_overrides.toml](../conformance/oracle_overrides.toml) machine-checked
PG-divergence ledger ([conformance.md](conformance.md) §5) and the §14 default-deny dependency
policy. The [determinism_exceptions.toml](../conformance/determinism_exceptions.toml) ledger
exists and is wired into `rake verify`; each entry states:

```toml
# Determinism-exception ledger (CLAUDE.md §2/§8/§10/§13; spec/design/determinism.md).
# Default-deny: an engine behavior that relaxes any of G1–G4 (determinism.md §1) is a BUG
# unless it has an entry here. Each entry names the surface, which guarantee it drops, the
# bounded blast radius (determinism.md §4), and how the corpus still tests it.
[[exception]]
id          = "float-transcendental"      # stable slug
surface     = "sin/cos/exp/log/pow over f64"
class       = "A"                          # U | B | A | I | P  (determinism.md §2)
drops       = ["G2", "G3"]                 # keeps the rest
blast_radius = "the float result column; promotes to row multiset only via a float-gated WHERE/ORDER BY/CAST (determinism.md §4)"
test        = "R-tag reduced-precision compare; PG-only oracle"
status      = "unratified"                 # ratified | proposed | unratified
reason      = "libm is not IEEE-correctly-rounded; per-core ULP divergence, not injectable."
```

A behavior that diverges with **no** entry is a failure, exactly as an unledgered
PG-divergence warns today — so the ledger cannot fall out of date.

**Admission criteria** (what makes a candidate legitimate vs a bug in disguise):

1. **Intrinsic, not accidental.** The non-determinism is sourced from entropy, the wall
   clock, IEEE/libm, an underspecified contract, the core's own identity, or a deliberately
   underspecified parallel schedule — never from an implementation artifact (hashmap order stays
   forbidden, §3), and never merely from independent planner implementations (§8).
2. **Cannot be cheaply narrowed away.** Prefer seam injection (§5), a deterministic
   tiebreaker (jed already breaks `ORDER BY` ties by PK — [encoding.md](encoding.md)), a
   spec'd algorithm, or an order-independent definition (§7 resolution A) **over** granting an
   exception.
3. **Testable by a weaker-but-real contract** that still catches genuine bugs — `rowsort`,
   epsilon/`R`, injected-input-exact, or a property/metamorphic relation. (This is where the
   NoREC/TLP machinery earns its keep: property oracles test what value-equality cannot.)
4. **Blast radius stated and bounded** (§4), so contamination is contained.

---

## 10. Ratification checklist (downstream edits)

When a section here moves from proposed/unratified to ratified, update **in the same change**:

- its canonical mechanical data and subsystem design document;
- [conformance.md](conformance.md) §4 plus the exact/property/metamorphic assertion surface that
  will detect cross-core drift;
- [CLAUDE.md](../../CLAUDE.md) §8/§10/§13 where the standing contract changes; and
- [../conformance/determinism_exceptions.toml](../conformance/determinism_exceptions.toml) only if
  a guarantee is actually relaxed, never merely because an implementation is independent.

The §8 Path-B ratification completes those documentation/data edits in P0. Later implementation
slices add estimator vectors, EXPLAIN columns, actual-cost pins, and NoREC relations before each
new physical choice becomes authoritative. The proposed §7 parallelism work additionally needs the
`order_sensitive` catalog field and a serial-vs-parallel sweep before it can be ratified.

---

## 11. Status summary

| Section | Subject | Status |
|---|---|---|
| §1 | Four-guarantee model (G1–G4), default-deny | **ratified** (formalizes existing intent) |
| §2 class **U** | Row order without `ORDER BY` | **ratified** (already `rowsort`, conformance.md §4) |
| §3 | Non-members (limits, collation, iteration-order) | **ratified** (restates existing rules) |
| §4 | Containment / no-contamination invariant | **proposed** |
| §5 class **B** | Clock + entropy seams, spec'd PRNG | **ratified** for `uuidv4`/`uuidv7` and the clock functions `now()`/`current_timestamp`/`clock_timestamp()` ([entropy.md](entropy.md)); **proposed** for `random()` |
| §6 class **A** | Binary floats (`f32`/`f64`) | **landed** — spec + ledger authored, all three cores ([float.md](float.md)) |
| §7 class **P** | Parallelism = optimization; `order_sensitive` flag | **proposed framework** |
| §8 | Plan-dependent observables / cost identity | **ratified** — specify the plan; no exception ([estimator.md](estimator.md)) |
| §9 | Exception ledger + admission criteria | **landed** |
