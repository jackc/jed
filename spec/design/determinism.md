# Determinism — design

> The determinism contract and its **sanctioned exceptions**. CLAUDE.md §8/§10/§13 require
> deterministic, cross-core-byte-identical results; this doc is where that requirement is
> decomposed into the distinct guarantees it actually bundles, and where the few places jed
> deliberately relaxes one of them are enumerated, bounded, and given a test mechanism.
>
> **Status: this doc is a framework + proposal.** The four-guarantee model and the
> already-existing carve-out (row order without `ORDER BY`) are **ratified**; the
> seam/injection design, the exception ledger, and the `order_sensitive` catalog flag are
> **proposed** (not yet built); **binary floats** (§6) and **plan-dependent observables**
> (§8) are **unratified** open questions. Each section marks its status. When any decision
> here is ratified, update the data it points at, [conformance.md](conformance.md) §4, and
> [CLAUDE.md](../../CLAUDE.md) §8/§10/§13 in the same change (§10 below lists the edits).

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
determinism" suggests: essentially floats, plus (eventually) plan-dependent observables.

---

## 2. The taxonomy of sanctioned relaxations

Five classes, by the **source** of the non-determinism. Each row states which guarantees it
keeps, and the test mechanism that still catches real bugs in it.

| Class | Source | Examples | Keeps | Drops | Test mechanism |
|---|---|---|---|---|---|
| **U** — Underspecified contract | SQL leaves it open | row order without `ORDER BY`; `LIMIT` without `ORDER BY`; `DISTINCT`/`GROUP BY` output order | G1, G2, G3 | G4 (sequence only) | `rowsort` (multiset compare) |
| **B** — Boundary inputs | host clock / OS entropy | `now()`/`current_timestamp`; `random()`; `gen_random_uuid()` (v4); UUIDv7 | G1, G2 *(injected/seeded)* | — *(prod: G1, G2 on the raw draw only)* | inject fixed clock+seed → exact; else property/bounds (§5) |
| **A** — Approximate / internal | IEEE / libm / approximate algorithm | binary `float` compute (§6); future `approx_count_distinct`, `TABLESAMPLE`, percentiles | G1 *(per binary)* | G2, G3 | epsilon / reduced-precision `R` tag + PG-only oracle |
| **I** — Implementation identity | the core itself | `version()`, build/identity reflection | G1 | G2, G3 *(by construction)* | property / regex on format, never value |
| **P** — Plan / strategy dependent | independent planners; parallelism | error *selection* among ≥2 candidate errors; cost under divergent plans; parallel fold order (§7) | varies — see §7/§8 | varies | serial==parallel metamorphic relation (§7); single-error test authoring (§8) |

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
  collation precisely so ordering is table-free and version-independent. If linguistic
  collation ever lands it must **vendor + version-pin** the UCA/CLDR tables as shared spec
  data (CLAUDE.md §5), turning it back into deterministic data — never a sanctioned
  exception.
- **Hash / iteration order.** This is **never** sanctioned — it is a *forbidden leak*
  (CLAUDE.md §8). The distinction from class **U** is sharp: no-`ORDER BY` leaves the row
  *sequence* unasserted while the *multiset* stays exact; an iteration-order leak would
  corrupt the multiset / values / types / errors / cost, which is always a bug.

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

Concretely this argues for keeping exempt values **out of keys and order-determining
positions** wherever possible — e.g. float out of `PRIMARY KEY`/index (the standing
narrowing every non-integer type already takes — [encoding.md](encoding.md)), so a tainted
value can at worst reorder a query result, never the *stored* order.

---

## 5. Push it to the boundary — the clock and entropy seams (class B)

**Status: proposed.** This is the jed-idiomatic move and it collapses almost all of class
**B** back into the deterministic contract. Do not make *the engine* non-deterministic; make
the engine a **deterministic function of two new host inputs**, behind seams — exactly like
the storage seam (CLAUDE.md §9, [storage.md](storage.md)):

- **Clock seam** — `now()` / `current_timestamp` read it. Production returns the wall clock;
  tests inject a fixed instant. The injected path is fully G1+G2 (every core, given the same
  injected instant, renders byte-identical).
- **Entropy seam** — `gen_random_uuid()` (v4) and unseeded `random()` draw from it.
  Production reads the OS RNG; tests inject a fixed seed.
- **A spec'd PRNG as shared data** (CLAUDE.md §5) — pin one algorithm (PCG / xoshiro),
  byte-for-byte, the way CRC-32 and the decimal limb arithmetic are already hand-rolled
  identically across cores ([decimal.md](decimal.md) §1). Then `setseed(s); random()` is
  **cross-core identical**, and even `gen_random_uuid` is reproducible under an injected seed.

The result: the corpus tests `now()` / `random()` / `uuid` with **injected clock+seed and
exact assertions** — G1+G2 preserved. The *only* irreducible production non-determinism left
in class **B** is the raw clock read and the raw entropy draw; everything downstream (the
rendering of `now()`, the bit-layout of a UUID, the distribution of `random()`) stays in the
deterministic, honesty-mechanism-covered world.

**Stability scope.** Match PG: `now()` / `current_timestamp` are fixed for the **statement**
(one read, reused for every row), so a statement's time value cannot vary row-to-row and is
trivially parallel-safe (§7). A per-call `clock_timestamp()` would be a distinct, separately
ledgered function.

**Not class B — deterministic counters.** Sequences / `SERIAL` / identity columns and jed's
existing synthetic rowid counter (CLAUDE.md §11 step 6) are **fully deterministic** (a
monotonic counter, reconstructed on load) and stay inside the contract. Do not exempt them.

This is `now()` / `EXTRACT`-time-functions and uuid-generation as currently filed under
"deferred follow-ups" in [TODO.md](../../TODO.md) (the timestamp + uuid entries): the seam
is the design that lets them land without breaking G1/G2.

---

## 6. Binary floats — the only internal surrender of G2 (UNRATIFIED)

**Status: ratified — `float64` is landing as the first exempted type** ([float.md](float.md);
the design doc, type-system data, and exception ledger are authored, cores in progress). It is
the *defining* member of class **A**: the one case where a value that **has** a right answer is
computed *differently per core* (G2) and per platform (G3) and cannot be injected away — but the
exemption is **narrow** (storage, total order, the `+ − * / sqrt` kernel, the exact-sum
`SUM`/`AVG`, `MIN`/`MAX`/`COUNT`, and cost all stay in-contract; only transcendental *values* and
text-rendering *layout* are exempt, both absorbed by the `R` tag's tolerant compare). The three
ledger entries are in [../conformance/determinism_exceptions.toml](../conformance/determinism_exceptions.toml).

The hard surface and the resolutions taken (full detail in [float.md](float.md)):

- **Arithmetic kernel** (`+ − * / sqrt`) is correctly-rounded by IEEE 754 and therefore
  **already G2/G3-identical** across Rust/Go/TS — *if* fusion is defeated: no x87 extended
  precision, round-ties-to-even, no flush-to-zero, and **no FMA contraction**. Rust and TS do
  not contract by default; **Go does** (`(Add (Mul x y) z)` fuses on ARM64 always, amd64 at
  `GOAMD64≥v3`), so the same Go source diverges across platforms (a G3 break) unless each
  multiply-feeding-add is written `float64(a*b)+c` (the spec-blessed barrier; a named
  intermediate is *not* guaranteed). In a tree-walking evaluator this barrier is structural
  (the product is a rounded return value before the add sees it), so the real exposure is
  hand-written numeric kernels — i.e. aggregates and transcendentals.
- **Aggregation** (`SUM`/`AVG`) is non-associative for float → naive folds diverge across
  cores *and* across parallel schedules. **Recommended:** define float `SUM`/`AVG` as the
  **correctly-rounded exact sum** (an exact accumulator), which is order-independent by
  construction → G1+G2+G3 hold and parallelism is free (§7). A documented PG divergence (PG's
  float sum is order-dependent), but a strictly better one.
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
| `SUM`/`AVG` over **int** | no (widens to int64/decimal; intermediates don't overflow) | parallel-safe |
| `SUM`/`AVG` over **decimal** | *error edge only* | values associative, but jed currently traps on the **first over-cap intermediate** ([decimal.md](decimal.md) §2) → order-dependent trap. **Fix:** overflow-check the **final** result only (matches PG) → order-independent |
| `SUM`/`AVG` over **float** | yes (value) | **A**: exact accumulator → order-independent (§6) |
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
  non-deterministic. This is the **same** problem as plan-divergent error selection (§8, class
  **P**), so it folds in there: the *fact* of erroring stays deterministic where possible,
  *which* of several distinct errors wins is the ledgered part, and error `.test` cases are
  authored with a **single offending row** so they stay deterministic. The related
  "does it error at all under `LIMIT`" edge (jed already has it — `SELECT DISTINCT 1/a … LIMIT
  1` traps where the plain form does not, [cost.md](cost.md) §3) is evaluation-strategy-
  dependent today; parallelism only widens it.

---

## 8. Plan-dependent observables (UNRATIFIED — open fork)

**Status: unratified.** Today the planner is simple enough that all cores choose the same
plan (the NoREC sweep even asserts pushdown happens), so plan-dependent observables agree by
construction. As the optimizer grows (cost-based join ordering, index selection), two
independently hand-written planners (CLAUDE.md §5 forbids codegenning them) will sometimes
pick **different plans**, and anything observable that depends on the chosen plan diverges
across cores — class **P**:

- **Error selection** — which of several candidate errors surfaces depends on
  evaluation/visitation order, which is plan- (and parallelism-) dependent.
- **Cost itself (CLAUDE.md §13).** This is the sharp edge: cost = Σ `page_read` +
  `row_produced` + `operator_eval`, and an index seek vs a seq scan have *different costs*. So
  **cross-core cost-identity (G2 on cost) silently presumes plan-identity.** A cost-based
  join-orderer can break it.

The fork to decide when it first bites (do not resolve now, but do not let it surprise you):

- **Spec the plan** — make plan choice / cost-model tie-breaking part of the shared contract
  so independent planners converge on the same plan. Strongest (keeps cost in G2) but the
  hardest, and in tension with "don't codegen the planner."
- **Ledger the divergence** — weaken cost-identity to *per-core* (G1, not G2) when plans
  legitimately differ, recorded as a class-**P** exception with the divergence bounded to cost
  + error-selection.

Until ratified, keep error `.test` cases single-error (§7) and keep `# cost:` assertions on
query shapes where all cores plan identically.

---

## 9. The ledger + admission criteria

**Status: proposed.** Mirror the two precedents already in the repo: the
[oracle_overrides.toml](../conformance/oracle_overrides.toml) machine-checked PG-divergence
ledger ([conformance.md](conformance.md) §5) and the §14 default-deny dependency policy.
Add a `spec/conformance/determinism_exceptions.toml` whose entries each state:

```toml
# Determinism-exception ledger (CLAUDE.md §2/§8/§10/§13; spec/design/determinism.md).
# Default-deny: an engine behavior that relaxes any of G1–G4 (determinism.md §1) is a BUG
# unless it has an entry here. Each entry names the surface, which guarantee it drops, the
# bounded blast radius (determinism.md §4), and how the corpus still tests it.
[[exception]]
id          = "float-transcendental"      # stable slug
surface     = "sin/cos/exp/log/pow over float64"
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
   clock, IEEE/libm, an underspecified contract, the core's own identity, or independent
   planners — never from an implementation artifact (hashmap order stays forbidden, §3).
2. **Cannot be cheaply narrowed away.** Prefer seam injection (§5), a deterministic
   tiebreaker (jed already breaks `ORDER BY` ties by PK — [encoding.md](encoding.md)), a
   spec'd algorithm, or an order-independent definition (§7 resolution A) **over** granting an
   exception.
3. **Testable by a weaker-but-real contract** that still catches genuine bugs — `rowsort`,
   epsilon/`R`, injected-input-exact, or a property/metamorphic relation. (This is where the
   NoREC/TLP machinery earns its keep: property oracles test what value-equality cannot.)
4. **Blast radius stated and bounded** (§4), so contamination is contained.

---

## 10. What ratification changes (downstream edits)

When a section here moves from proposed/unratified to ratified, update **in the same change**:

- **[conformance.md](conformance.md) §4** — flip the *Determinism rules* from absolute
  prohibition ("No nondeterminism. No wall-clock, no random. No floats.") to **default-deny +
  enumerated ledgered exceptions**, each citing its class and test mechanism here.
- **New harness mechanisms** — injected clock+seed for replay (makes class **B** exact); the
  **`R`** reduced-precision comparison (class **A**, the tag is already reserved); property /
  invariant assertions (a `# satisfies:`-style directive, classes **I**/**A**); the
  serial-vs-parallel metamorphic relation (§7) in the sweep.
- **[../functions/catalog.toml](../functions/catalog.toml)** — the `order_sensitive` field on
  aggregates (§7), with its `verify.rb`/codegen branch.
- **[CLAUDE.md](../../CLAUDE.md)** — §8 (divergence hotspots) and §10 (determinism everywhere)
  gain a pointer to this doc; §13 gains the explicit note that cost-identity presumes
  plan-identity (the §8 fork).
- **[../conformance/determinism_exceptions.toml](../conformance/)** — created with the §9
  format and wired into `rake verify` (coherence) and the importer/sweep (drift).

---

## 11. Status summary

| Section | Subject | Status |
|---|---|---|
| §1 | Four-guarantee model (G1–G4), default-deny | **ratified** (formalizes existing intent) |
| §2 class **U** | Row order without `ORDER BY` | **ratified** (already `rowsort`, conformance.md §4) |
| §3 | Non-members (limits, collation, iteration-order) | **ratified** (restates existing rules) |
| §4 | Containment / no-contamination invariant | **proposed** |
| §5 class **B** | Clock + entropy seams, spec'd PRNG | **proposed** |
| §6 class **A** | Binary floats (`float64`) | **ratified** — spec + ledger authored, cores in progress ([float.md](float.md)) |
| §7 class **P** | Parallelism = optimization; `order_sensitive` flag | **proposed framework** |
| §8 class **P** | Plan-dependent observables / cost-identity fork | **unratified** (open) |
| §9 | Exception ledger + admission criteria | **proposed** |
