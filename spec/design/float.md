# The `f32` / `f64` types — design

> The reasoning behind binary floating point. The **data is authoritative**
> ([../types/scalars.toml](../types/scalars.toml) — the type, encoding;
> [../types/compare.toml](../types/compare.toml) — comparability/promotion;
> [../types/casts.toml](../types/casts.toml) — casts;
> [../functions/catalog.toml](../functions/catalog.toml) — operators/functions/aggregates;
> [../fileformat/format.md](../fileformat/format.md) — the on-disk value codec;
> [../grammar/grammar.ebnf](../grammar/grammar.ebnf) — literals). This doc is the *why* and
> the precise semantics, **and** the one place the determinism contract is partially relaxed
> — read [determinism.md](determinism.md) first (the four-guarantee model and the exception
> ledger). When a decision here changes, change the data and here in the same edit, and
> update [CLAUDE.md](../../CLAUDE.md) §8 and [determinism.md](determinism.md) §6 if it revises
> a commitment.

`f64` is the engine's **approximate** numeric: IEEE 754 binary64 (double precision). It
is deliberately the opposite of `decimal` ([decimal.md](decimal.md)) — `decimal` is exact,
base-10, finite, and fully cross-core deterministic; `f64` is inexact, base-2, admits
NaN/±Infinity, and is the **first type exempted from cross-core byte-identity** (CLAUDE.md
§2/§8). The exemption is **narrow and deliberate** (determinism.md §1–§6): determinism is
preserved everywhere it is *reasonably easy*, and surrendered only where IEEE/libm make it
genuinely hard.

## 1. Why floats at all, and the determinism stance

`decimal` already owns exact numerics (the §8 bias is "keep `f64` out of compare/text"), so
`f64` exists only for what decimal cannot serve: ingesting/storing real-world
double-precision data (scientific, sensor, ML, geo), 8-byte compactness, fast approximate
math, and transcendental functions. Because the type is intrinsically approximate, paying
`decimal`-level effort to make it bit-identical across cores would be over-investing the
project's scarcest currency (hand-rolled cross-core algorithms) in its least on-brand type.
So the stance (determinism.md §6, class **A**):

**Kept deterministic and cross-core byte-identical (G1–G3) — the easy wins:**

- **Storage** — the 8 IEEE bytes round-trip identically; the golden on-disk fixture is
  byte-exact `rust == go == ts == ruby` like every other type.
- **Comparison & ordering** — a defined total order (§3), so a float *at rest* sorts
  identically in every core.
- **The arithmetic kernel** `+ − * /`, unary `−` — IEEE 754 mandates these are *correctly
  rounded*, so they are bit-identical across cores **with light discipline** (§5): no x87
  extended precision, round-ties-to-even, no flush-to-zero, and **no FMA contraction** (the
  Go hazard, §5). `sqrt` joins them (IEEE-mandated correctly-rounded).
- **`MIN`/`MAX`/`COUNT`** and the **int / decimal `SUM`/`AVG`** — order-independent (the total
  order §3; integers widen to i64 and decimals fold uncapped, so intermediates are associative),
  hence identical across cores *and* across any future parallel/serial plan (determinism.md §7).
  Only the **float** `SUM`/`AVG` is exempted (below).
- **Cost** — structural (one `operator_eval` per node, one `aggregate_accumulate` per row);
  it depends on the *count* of operations, never the float *value*, so it stays fully
  deterministic and cross-core, and float queries still carry `# cost:` assertions.
- **Text** — every rendering of a float is PostgreSQL's `float8out` / `float4out` text, computed
  the same way in every core (§9).

**Exempted from cross-core identity (G2/G3) — ledgered (`determinism_exceptions.toml`):**

- **Transcendental function *values*** (`exp`, `ln`, `log10`, `pow`/`^`, `sin`, `cos`, `tan`,
  `atan2`, …) — not IEEE-correctly-rounded; every libm differs by an ULP (§8).
- **Float `SUM`/`AVG` fold order** — folded as a **streaming running total in scan order**
  (§7), not order-independently. Because IEEE `+` is non-associative, the low bits depend on
  the fold order, so the value is exempted (class **A ∩ P** — determinism.md §7): it is
  cross-core identical *today* (every core folds the same PK-scan order with the same IEEE
  add) but that is **not promised** — a future parallel plan may reorder the fold. The `R` tag
  + PG-only oracle absorb it.
- **Float-gated control flow** — a query whose *row multiset* depends on an exempted value
  (a transcendental result near a `WHERE`/`ORDER BY … LIMIT` boundary) is cross-core
  unspecified (the §4 contamination rule of determinism.md). A float **is** keyable (§10), so a
  *tainted* float stored into a key can also reach *stored* order — the same bounded
  contamination, now ledgered as a widening of the `float-transcendental` blast radius rather
  than prevented by a key narrowing (determinism.md §4). A float at rest is in-contract, so this
  bites only a tainted-float key, never an ordinary one.

Net: a float query that does not call a transcendental is, in practice, cross-core identical
today (float `SUM`/`AVG` included — every core folds the same scan order); the exemption
withholds the *promise* only at transcendentals, float `SUM`/`AVG` fold order (against a future
parallel plan), and the PG oracle.

## 2. Scope — two widths forming a promotion tower

Two types, a **promotion tower** like the integers (canonical ids state width in bits, the
`i16`/`i32`/`i64` convention — [types.md](types.md) §2):

| Canonical id | rank | Aliases | IEEE | On-disk code |
|---|---|---|---|---|
| `f32` | 1 | `real` | binary32 (single, ~6 digits) | 13 |
| `f64` | 2 | `double precision`, `float` | binary64 (double, ~15 digits) | 12 |

**Naming, against the C/Java intuition:** in PostgreSQL a bare **`float` (no precision) is
`double precision` (64-bit)** — *not* 32-bit; the 32-bit type is spelled `real` (`float4`).
So `float` aliases `f64`, and `real` aliases `f32`. PG's byte-shorthand `float8` → `f64`
and `float4` → `f32` **is** accepted as an alias — the `f` prefix keeps jed's bit-namespace
disjoint from PG's byte-namespace, so there is no collision (the same disjoint-namespace rule
that lets `int8` → `i64` — types.md §2, CLAUDE.md §1/§4). The `float(p)` precision typmod is
**not** accepted.

**The tower (compare.toml `max-rank`).** When two floats of different width meet
(arithmetic or comparison), both widen to the higher rank: **`f32 → f64`, which is
lossless** (every binary32 is an exact binary64), so it is also an **implicit cast** — the
float analogue of `i16 → i64`, and matching PG (`real` promotes to `double`). `f64 →
f32` is lossy and **explicit**. Crossing *out of* the float family (int/decimal ↔ float)
stays **explicit** either way (§6) — within-family widening is the *only* implicit float edge.
The widening is scalar-only: an `f32[]` never compares with an `f64[]` (`42804`, array.md §5 —
PostgreSQL has no `real[] = double precision[]` operator either).

**Where the cast sits — one rule for every core.** The values are PostgreSQL's (`float4` widens to
`float8`, then the pair compares under the total order of §3 or computes at `f64`), so
`0.1::f32 = 0.1::f64` is FALSE and `0.1::f32 > 0.1::f64` is TRUE. The resolver places the widening
as an explicit `cast(x as f64)` node on the `f32` operand of every comparison operator
(`IS [NOT] DISTINCT FROM` and the `IN`-list / `BETWEEN` / simple-`CASE` desugarings included),
arithmetic operator, and `f64`-result float function (all but `abs`), and on the `f32` arms of
`CASE` / `COALESCE` / `GREATEST` / `LEAST`. Two `f32` operands get no cast. Comparisons made inside
one node — `NULLIF` (whose result keeps `a`'s width), row and record comparisons, `= ANY` / `ALL`
over arrays and subqueries, `IN (subquery)` — widen at the value level instead. The placement is
observable, so it is part of the cross-core contract: the cast charges one `operator_eval`, shows
in `EXPLAIN (VERBOSE)`, and keeps the pair out of key-bound detection (a cast operand is not a key
source), so `f32col = f64value` scans with a residual filter in every core (cost.md "Mixed-width
floats"). Pinned by `types/float_mixed_width.test`.

**Everything below applies to each width** (the total order §3, the trap model §3, the kernel
§5, the exact-sum aggregates §7, the functions §8, rendering §9), differing only by width:
`f32` arithmetic rounds to binary32 (Rust `f32`, Go `f32`, **TS `Math.fround` on every
op/literal/cast** — the one extra determinism discipline a second width adds), stores 4 bytes,
and `SUM`/`AVG` round at the input width. A mixed-width binary op promotes to `f64` first,
so the actual computation is always at one width.

## 3. Representation, the total order, and special values

**Representation.** A value is an IEEE 754 binary64: sign, 11-bit exponent, 52-bit mantissa.
NaN and ±Infinity are **first-class values** (unlike `decimal`, which excludes them —
decimal.md §2 — because it has no float source; `f64` is that source). This is the
`timestamp` precedent generalized: `timestamp` already carries ±infinity as totally-ordered
sentinels (timestamp.md), and `f64` does the same for ±Inf and NaN.

**The total order (PostgreSQL's `float8` btree order — CLAUDE.md §1).** IEEE comparison is a
*partial* order (NaN is unordered; `-0 == +0`), but SQL needs a total order for `ORDER BY`,
`DISTINCT`, `GROUP BY`, indexes, and `MIN`/`MAX`. jed adopts PG's total order:

```
-Infinity  <  (finite, numerically)  <  +Infinity  <  NaN
```

with **`-0 = +0`** (negative zero equals positive zero) and **`NaN = NaN`** (all NaNs are one
equivalence class — bit patterns are not distinguished). So `NaN` is the single largest value
(above `+Infinity`), and two NaNs collapse under `DISTINCT`/`GROUP BY`. This is a documented
divergence from raw IEEE (where `NaN <> NaN` and comparisons with NaN are UNKNOWN) — but it is
PG's behavior, and it is what makes ordering, dedup, and keys total and deterministic. The
`=`/`<`/`>`/`<=`/`>=` operators and `IS [NOT] DISTINCT FROM` all use this total order, so
`NaN = NaN` is **TRUE** in jed (PG's float8 `=` agrees). `f64 × f64` is the only
comparable pair (§6); NULL is still the largest of all (after NaN — the presence tag, §10).

**How special values arise — PG's trap model (CLAUDE.md §8, the existing trap philosophy).**
Finite arithmetic **never produces** Inf/NaN; it traps instead, exactly like the integer and
decimal types:

- a finite operation whose true result overflows the binary64 range traps **`22003`**
  (`numeric_value_out_of_range`) — e.g. `1e308 * 10`;
- `x / 0` (and `x % 0`) traps **`22012`** (`division_by_zero`) for **every numerator except
  `NaN`**: `1/0`, `Inf/0`, and `0/0` all trap; **only `NaN / 0` escapes the trap and
  propagates** (`NaN / 0 = NaN`). This matches PostgreSQL exactly — PG raises *division by
  zero* for a finite or infinite numerator over a zero divisor and yields `NaN` only when the
  numerator is already `NaN`. (The zero-divisor rule is the same for `/` and `%`.)

So Inf/NaN enter **only** as input — a literal (`float 'Infinity'`, `float 'NaN'`), a text
cast, or a stored value — and then **propagate** through arithmetic by IEEE rules (`Inf + 1 =
Inf`, `Inf - Inf = NaN`, `NaN * 0 = NaN`). The **one** exception is a zero divisor: `Inf / 0`
traps rather than propagating to `±Inf` (above), because PG treats *any* zero divisor with a
non-`NaN` numerator as a division-by-zero error. This keeps the *common* path (finite math)
free of non-finite results while still modelling the values when a user supplies them, and it
matches PG.

**Negative-zero canonicalization.** `-0.0` and `+0.0` are equal in value and **must produce
identical key bytes and dedup to one bucket** (§10). The value codec stores the bits as given
(so a stored `-0.0` round-trips its bits), but **equality, ordering, key encoding, and
`DISTINCT`/`GROUP BY` treat `-0 = +0`** — the comparator and the key encoder canonicalize
`-0 → +0` before acting. A core that lets `-0` and `+0` land in different groups diverges.

## 4. Literals

There is no dedicated float literal token; a float value is written one of three ways (the
existing literal machinery — types.md §6, grammar.md §36):

- **A decimal literal adapting to a float context.** A `.`-bearing numeric constant is an
  untyped decimal constant (decimal.md §6) that adapts to its context. In a **float context**
  — `INSERT`/`UPDATE` into a `f64` column, a comparison against one (`WHERE f = 1.5`), or
  the other operand of float arithmetic — it coerces **decimal → f64** at resolve time
  (the nearest binary64 to the exact decimal, round-ties-to-even, the IEEE conversion). An
  integer literal adapts the same way. This is *literal adaptation*, not an implicit
  cross-family cast (§6): a bare literal carries no type until its context names one. The
  context **width** is the float operand's — an `f32` column, or the element of an `f32[]` in
  `x = ANY(arr)` (array-functions.md §11.3), makes it the nearest **binary32** — so
  `real_col = 0.1` and `0.1 = ANY(real_arr)` match a stored `0.1`. **A documented divergence at
  `f32`:** PostgreSQL types `0.1` as `numeric`, has no `real = numeric` operator, and compares both
  sides as `double precision`, where the widened binary32 nearest 0.1 (`0.10000000149011612`) never
  equals the double nearest 0.1 — so PG returns FALSE. jed keeps the rule (a literal takes its
  context's type, exactly as an integer literal takes an `i16` column's width); ledgered in
  `oracle_overrides.toml` (`suites/types/float_literal_adaptation.test`). At `f64` the engines agree.
- **The typed literal `float '…'`** (and `f64 '…'`, `CAST('…' AS f64)`) — the
  `type 'string'` form (grammar.md §36). The string is parsed by f64's input function:
  an optional sign, decimal digits with an optional point and **`e`-notation** (`1.5e10`,
  `-3E-7` — the same e-notation a bare decimal literal now takes, grammar.md §14, here via
  f64's own string parse), plus the case-insensitive special words
  **`Infinity`/`+Infinity`/`-Infinity`/`inf`/`NaN`** (PG's `float8in` spellings). Malformed
  input traps **`22P02`** (`invalid_text_representation`) deterministically at resolve, before
  any row is scanned; a value outside the binary64 range traps **`22003`**.
- **A bare string literal adapting to a float column.** An untyped string literal stored into an
  `f32`/`f64` column — an `INSERT … VALUES` cell or an `UPDATE … SET` value — is parsed by that
  column's float input function, exactly as the typed literal above (same grammar, same special
  words, binary32 rounding at `f32`), as it is for the other string-input types (`date`,
  `timestamp`, `uuid`, … — types.md §6). This matches PostgreSQL, which resolves an `unknown`
  literal against the target column, and it is how a `VALUES` row spells `NaN`/`±Infinity`, which
  no bare numeric literal can. Each row of a multi-row `VALUES` adapts independently, so
  `VALUES (1, 1.5), (2, 'Infinity')` mixes numeric and string literals in one float column.
  Malformed input traps **`22P02`**, a finite value outside the column's range **`22003`** (at
  store for an `INSERT` row; at resolve for an `UPDATE`/`ON CONFLICT DO UPDATE` value, like the
  typed literal). Only a *literal* adapts: a text-typed expression (`'Inf' || 'inity'`, a text
  column) is not assignable to a float column (`42804`, as in PostgreSQL), keeping §6's "a value
  never silently becomes a float". (`suites/types/float_string_literal.test`.)

## 5. Arithmetic — the correctly-rounded kernel

`f64 ⊕ f64 → f64` for `+ − * /` and unary `−` (and `%`/`mod`, IEEE `fmod`, exact).
Each is the IEEE 754 correctly-rounded operation (round-ties-to-even), evaluated **one
operator per expression node** in the tree-walking evaluator. Division by zero traps `22012`;
a finite result that overflows binary64 traps `22003` (§3). Operands that are already Inf/NaN
propagate (no trap) per IEEE.

**Cross-core determinism of the kernel (in-contract — the easy win).** IEEE mandates these
operations are correctly rounded, so they are **bit-identical across Rust/Go/TS** provided no
core silently changes the computation. The disciplines, pinned here as a §8-style hotspot:

1. **No FMA contraction.** A compiler may fuse `a*b + c` into one rounding (an FMA), changing
   the result. Rust and TS do not contract by default. **Go does** — its spec permits fusion
   and the gc compiler emits an FMA for `(Mul x y) + z` on ARM64 (always) and amd64
   (`GOAMD64≥v3`), so the same source would diverge across platforms (a G3 break). jed's
   float arithmetic lives in the **tree-walking evaluator**, where each operator is a separate
   node that writes its rounded result to a value before the next node consumes it — fusion is
   structurally impossible across that boundary. Any hand-written numeric kernel (the §8
   transcendentals, the §7 accumulator) that computes `a*b+c` in one Go expression **must**
   defeat fusion with the spec-blessed barrier `f64(a*b) + c` (a named intermediate is
   *not* a guaranteed barrier).
2. **No x87 extended precision** (modern SSE2/ARM64/WASM scalar f64 — a build note, not a code
   path), **no flush-to-zero** (subnormals computed, not zeroed), **round-ties-to-even** (the
   default everywhere; never changed).

So the kernel keeps G1–G3. Only transcendentals (§8) leave the contract.

## 6. Coercion and casts — strict, no implicit cross-family

`f64` is its own comparison/arithmetic family. **No implicit coercion** crosses into or
out of it (stricter than PG, justified by the strict type system — CLAUDE.md §4):

- `int ⊕ f64`, `decimal ⊕ f64`, `int = f64`, `decimal < f64`, … are
  **`42804`** datatype-mismatch errors. (PG promotes the other operand to `float8`; jed
  requires an explicit cast. A documented divergence, oracle-ledgered.) Only *literals* adapt
  to a float context (§4) — a *value* never silently becomes a float.

Casts (all **explicit** `CAST` — [../types/casts.toml](../types/casts.toml)), because every
one is lossy or representation-changing:

| from → to | mode | rule |
|---|---|---|
| `int{16,32,64} → f64` | explicit | nearest binary64, round-ties-to-even (exact ≤ 2^53; larger i64 may round). Never traps. |
| `decimal → f64` | explicit | nearest binary64 to the exact decimal value, round-ties-to-even. Never traps (a huge decimal → ±Inf? **traps `22003`** rather than yielding Inf, matching the finite-overflow rule §3). |
| `f64 → int{16,32,64}` | explicit | round **half away from zero** to an integer (jed's one rounding mode — decimal.md §3), then range-check (`22003`). NaN/±Inf → `22003`. **Documented divergence from PG**, which rounds half-to-even (`rint`); jed keeps one engine-wide mode. |
| `f64 → decimal` | explicit | the exact decimal of the binary64 value, then the target typmod's scale coercion (decimal.md §3). NaN/±Inf → `22003` (decimal is finite). |
| `text ⇄ f64` | explicit | the `float '…'` literal coercion (§4) is the text→float *literal* path; a **runtime** `CAST(text_col AS f64)` parses per row (the runtime-text-cast slice), and `CAST(float_expr AS text)` renders PostgreSQL's `float8out` / `float4out` text (§9, types.md §5), the same in every core, so it can be compared, stored, and keyed. |

`int`/`decimal` → `f64` is explicit (not implicit like `int → decimal`) precisely because
it is **lossy** — the whole point of the strict matrix.

## 7. `SUM` / `AVG` — the streaming scan-order fold (ledgered order-dependent)

Naive float summation is non-associative, so its result depends on the order rows are folded.
There are three ways to face that (determinism.md §7): **(A)** make it order-independent so the
value is fixed regardless of fold order; **(B)** pin a canonical fold order; **(C)** ledger it as
order-dependent. Only a **correctly-rounded exact accumulator** (a superaccumulator / exact
expansion, round-once over the true mathematical sum) truly achieves (A) at O(1) streaming cost —
and that is precisely the hand-rolled cross-core numerical code §1/§6 declines to spend on jed's
least on-brand type. Option (B) as a *sort-then-fold* (jed's prior form) is O(n·log n) time and
O(n) memory, and — because it still folds with lossy IEEE adds — is not even correctly-rounded: it
buys determinism at a real cost while delivering an arbitrary (magnitude-sorted) value no more
"right" than any other. jed therefore takes **(C)**: float `SUM`/`AVG` is a **streaming running
total folded in scan order**, O(1) memory, and its fold order is **ledgered as non-deterministic**
(`determinism_exceptions.toml`, `float-sum-order`, class **A ∩ P**).

What this keeps and drops (determinism.md §1): the value is **cross-core identical today** — every
core folds the same base-table PK-scan order with the same IEEE add, so `rust == go == ts` still
holds on the current single-threaded executor — but jed **does not promise** it (G2/G3 withheld),
so a future parallel plan may reorder the fold without a contract break (the point of ledgering it
now rather than re-visiting under parallelism). It is a documented divergence from PG (whose float
sum is likewise order-dependent), and the value stays within the `R`-tag tolerance of PG (§9). Int
and decimal `SUM`/`AVG`, and `MIN`/`MAX`/`COUNT`, remain order-independent and fully in-contract
(§1) — this exemption is float `SUM`/`AVG` only. The float statistical aggregates (`var_pop` and the
rest of [aggregates.md](aggregates.md) §23) fold PostgreSQL's Youngs-Cramer update in scan order on
the same footing, ledgered separately as `float-moment-order`.

**Algorithm** (the identical steps every core runs — CLAUDE.md §2/§5):

1. **Special-value flags** (order-independent): track whether any input is `NaN`, `+Inf`, `-Inf`.
   NULL inputs are skipped (as every aggregate).
2. **Fold each finite input** into a single running total, in the order the scan produces rows:
   map `-0.0 → +0.0`, then add with width-correct IEEE addition (round-ties-to-even per add;
   `f32` re-rounds to binary32 each step — TS `Math.fround`). No collection, no sort.
3. **Resolve at finalize** (special values dominate the running total): if any `NaN` → `NaN`;
   else both `+Inf` and `-Inf` → `NaN`; else `+Inf` → `+Inf`; else `-Inf` → `-Inf`; else the
   running total — which, if it reached ±Inf from finite inputs, is a finite-overflow `22003`
   (the §3 rule; PG yields ±Inf — a documented divergence). Since finite `+ ±Inf` cannot recover
   to finite, a final `isInf` check on the running total is equivalent to a per-add check.

`AVG` = `SUM / count` (count exact; the division rounded once at the input width), NULLs skipped,
empty group → `NULL`. **Result types**: `SUM`/`AVG(f32) → f32`, `SUM`/`AVG(f64) →
f64` (a float sum/avg stays the input width — `same_as_input`, matching PG `sum(real) → real`;
AVG over float stays float, unlike `AVG(int) → decimal`, and a minor divergence from PG which
widens `AVG(real) → double`). `MIN`/`MAX(floatN) → floatN` (the §3 total order), `COUNT → i64`.

**Cost.** One `aggregate_accumulate` per input row (the running add is O(1)), deterministic and
cross-core — so float aggregate queries keep `# cost:` assertions even though their *values* are
PG-oracle-only. The streaming fold also lets float `SUM`/`AVG` ride the same O(1) fold-during-scan
path as int/count/min/max (no per-column materialization), unlike the prior collect-and-sort form.

## 8. Functions — the exact set (in-contract) vs the transcendental set (exempted)

Float scalar functions split by whether they are correctly-rounded:

**Exact / correctly-rounded — in-contract (G1–G3):** `abs`, `ceil`, `floor`, `trunc`, `round`
(half away from zero — the engine's one mode; `round(f)` and `round(f, n)`), `sign`, and
**`sqrt`** (IEEE-mandated correctly-rounded). These are bit-identical across cores and carry
exact `R`-tag assertions; they reuse the existing scalar-function machinery (functions.md §9,
the `abs`/`round` precedent) with `f64` overloads. **Also in-contract** — though their *values*
are irrational — are **`pi()`** (a shared f64 literal, no operand) and **`radians`/`degrees`**
(a SINGLE correctly-rounded IEEE multiply/divide by PG's exact `RADIANS_PER_DEGREE` literal):
one IEEE op with one shared constant is byte-identical cross-core, so these carry **no** ledger
entry.

**Transcendental — exempted (G2/G3 dropped, ledgered):** `exp`, `ln`, `log10`, `pow(x, y)` /
the `^` operator, `power` (the PG-name alias for `pow`), `sin`, `cos`, `tan`, `cbrt`, `asin`,
`acos`, `atan`, `atan2`, `cot`, and the hyperbolics `sinh`/`cosh`/`tanh`/`asinh`/`acosh`/`atanh`.
Each core calls its native libm (or, for `cot`, `1/tan`); results may differ in the last ULP
across cores and from PG. These share one **`determinism_exceptions.toml` entry**
(`float-transcendental`, class **A**, drops G2/G3, blast radius = the result column, promoting
only via float-gated control flow §1), are compared by the `R` tag's tolerant rule (§9), and are
**PG-oracle-only** in the corpus (an `oracle_overrides.toml` note where PG's ULP differs). Domain
errors follow PG exactly: `ln(0)` / `ln(-1)` / `sqrt(-1)` → `22003`; `asin`/`acos` outside
[-1, 1] and `acosh`/`atanh` outside their domains (and ±Inf) → `22003` — rather than returning
NaN, keeping NaN an *input-only* value (§3) — while `cot(0)` and `atanh(±1)` return `±Infinity`
(PG-faithful, admissible since the result is in the exempted column), and a NaN *operand*
propagates. `sinh`/`cosh` overflow to `±Infinity` with NO trap (a PG quirk, unlike `exp`/`pow`).

**Numeric transcendentals have LANDED** ([decimal.md §8](decimal.md)). PostgreSQL also offers
`sqrt`/`ln`/`exp`/`power(numeric,numeric)`/`log(numeric)`/`log(b, x)` over `numeric` (PG has no
float two-arg `log`). These must be **byte-identical across cores** (decimal is in-contract, §5 —
they cannot ride the ULP exemption above), so they are a hand-rolled PG-faithful arbitrary-precision
port of `numeric.c` (`sqrt_var`/`ln_var`/`exp_var`/`log_var`/`power_var`), computed entirely in the
exact-decimal limb arithmetic with **no libm transcendental on the value path** (the result scale is
chosen deterministically — decimal.md §8). The float `power`/`pow` and the numeric
`power`/`pow`/`log` now coexist as disjoint overloads (the `f` prefix keeps the families apart).

The transcendental list is a generous starting set; further FLOAT functions are easy additive
follow-ons (each one operator-catalog row + a line on the shared ledger entry). The EXACT numeric
follow-ons `sign`/`mod`/`div`/`gcd`/`lcm`/`factorial`/`width_bucket` and the decimal-introspection
trio `scale`/`min_scale`/`trim_scale` have **landed** (in-contract, no ledger — see
`spec/conformance/suites/expr/numeric_functions.test`).

## 9. Rendering and the `R` conformance tag

**Rendering.** Every rendering of a float is PostgreSQL's `float8out` (an `f32`: `float4out`)
text at the oracle's `extra_float_digits = 1`: a result cell, `CAST(float AS text)` and what is
built on it (an array's or composite's text, a `text[]` element), a JSON document (§9.1), an
error's Detail field, a `column_default`, an EXPLAIN literal. It is in-contract, byte-identical
across cores and with PostgreSQL, with no ledger entry; the text can be compared, stored, and
keyed (a `UNIQUE` index on `f::text` holds in every core).

- **Digits** — PostgreSQL's Ryu (`src/common/d2s.c` / `f2s.c`), built with `STRICTLY_SHORTEST 0`:
  the fewest significant digits of a decimal **strictly inside** the value's rounding interval
  (both bounds excluded, whatever the mantissa's parity); among those, the one nearest the exact
  value; a tie to the even last digit. The interval is `x ± ½ ulp`, except that its lower half is
  `¼ ulp` when the mantissa is a power of two above the lowest binade. An `f32` takes the
  binary32 interval, so `real '0.1'` is `0.1`, not the widened binary64's `0.10000000149011612`.
- **Layout** — Ryu's `to_chars`: fixed-point when the decimal exponent of the first digit is in
  `[-4, 15)` for an `f64` or `[-4, 6)` for an `f32` (`0.0001`, `100000000000000`, `123456`),
  otherwise `d[.ddd]e±XX` with at least two exponent digits (`1e-05`, `1e+15`, `1.6777215e+07`,
  `5e-324`). `NaN`, `Infinity`, `-Infinity`, `0`, `-0` are spelled out.

The native shortest round trips (Rust `{}`, Go `strconv.FormatFloat(f, 'g', -1, 64)`,
JavaScript `Number.prototype.toString`) are not enough. Their layouts differ, and so, rarely, do
their digits: a shorter decimal can lie exactly on the rounding interval's bound (Go accepts it
when the mantissa is even; the double nearest `1e23`, `99999999999999991611392`, is `1e+23` in Go
and `9.999999999999999e+22` in PostgreSQL), or the value can lie exactly halfway between two
shortest candidates (`3454183008863.90625`: Go and `toString` take the even digit, Rust's `Display`
and JavaScript's `toPrecision` the other).

So each core computes the digits the same way, from the exact value with its own decimal limb
arithmetic (`floatShortestExact`): for each length n, the n-digit decimals just below and just
above the exact value, kept if strictly inside the interval; the first length with one wins. The
search starts at the native shortest length, a valid lower bound because a native formatter
accepts every decimal Ryu does (TypeScript, which has no native binary32 formatter, starts an
`f32` at 1). An `f64` skips the search for the native result when a guard proves the two agree:
an integer below `2^53` always does (its interval is at most one unit wide, and the bounds have
more digits than it); otherwise a disagreement needs an exact tie (a value with at most 18
significant digits, so `o·2^k` with odd `o` and `k` in `[-25, -1]`) or a bound with at most 17
significant digits (a negative exponent of at least `-25`, or an integer bound below `2^57` or
with a factor 5 in its odd part). A fuzz of fifty thousand values against the oracle (random bit
patterns, every power of two, the neighbours of every power of ten, short dyadics, large
integers), on both paths, agreed exactly. The guarded values cost a few times the native
formatter (`json_float_render` benchmark). Only internal keys that are never displayed (the
DISTINCT/GROUP BY dedup key) keep a native formatter.

**The `R` (real) render tag** (conformance.md §1 — long reserved, now in use). A column tagged
`R` is compared **by value, not by string**: both expected and actual are parsed to f64 and
considered equal iff bit-equal **or** within a small relative/ULP tolerance, with `NaN` ==
`NaN`, `±Inf` exact, and `-0` == `+0`. This single rule covers (a) a transcendental's last-ULP
cross-core divergence, (b) the float `SUM`/`AVG` fold order, and (c) the larger jed-vs-PG
divergence in the oracle import (PG libm). The in-contract surface
(kernel, exact functions, exact-sum aggregates) is bit-identical and would pass an exact
compare; the tolerance exists for the exempted surface and the oracle. `# cost:`/`# names:`/
`# types:` are unaffected — they are structural and stay exact (§1).

### 9.1 Floats in JSON

A float converted to JSON (`to_json[b]`, `array_to_json`, `row_to_json`, `JSON_SCALAR`, the
builders and aggregates, a `FORMAT JSON` `PASSING` value — [json-sql-functions.md](json-sql-functions.md)
§2.2) takes the §9 text: in `json` it is a number, or for NaN/±Infinity a JSON string; in `jsonb`
it is the `numeric` that `numeric_in` reads from it (`1e+21` is `1000000000000000000000`, `-0` is
`0`), or the string; an object key is the text. A float `PASSING` value without `FORMAT JSON`
does not use the text: it is a jsonpath number by `float8_numeric` / `float4_numeric` (`%.15g` /
`%.6g` of the exact value), and NaN/±Infinity, numeric NaN/Infinity items in PostgreSQL, are a
deferred `0A000` because jed's `decimal` is finite.

### 9.2 Floats in `to_char`

`to_char(f32 | f64, template)` does not use the §9 text either: PostgreSQL's `float4_to_char` /
`float8_to_char` format with C `printf` (`"%.*f"`, `"%+.*e"`) at a template-chosen precision capped
at `FLT_DIG` / `DBL_DIG` significant digits. jed reproduces those conversions from the float's
exact decimal expansion (§6), rounded half to even as glibc does, with PostgreSQL's own `snprintf`
spellings `NaN` / `Infinity` — pure integer digit arithmetic, so the result is in contract and
byte-identical across cores ([formatting.md](formatting.md) §6.3).

## 10. On-disk, keys, and cost

- **On-disk value codec** — stable **type code 12** (`f64`, 8 bytes) and **13** (`f32`,
  4 bytes) (format.md). The body is the IEEE bytes, **big-endian** (`f64`: Go
  `math.Float64bits`, TS `DataView.setFloat64(_, false)`, Rust `f64::to_bits().to_be_bytes()`;
  `f32`: Go `math.Float32bits`, TS `setFloat32(_, false)`, Rust `f32::to_bits().to_be_bytes()`)
  behind the shared presence tag (NULL = tag only). Fixed-width, so no length prefix — like
  `uuid`/`timestamp`. The stored bits are preserved **verbatim for every value except `NaN`**: a
  stored `-0.0` keeps its sign bit, and `±Infinity`/finite values keep theirs, but a `NaN` is
  **canonicalized to the single quiet pattern** `0x7FF8000000000000` (`f64`) / `0x7FC00000`
  (`f32`) on the way to disk. This NaN-only step is the one storage divergence from verbatim,
  and the determinism contract forces it: a NaN's *payload* bits are **core-specific** (Go's
  `math.NaN()` is `0x7FF8…001`, Go/Rust hardware `Inf − Inf` is the negative `0xFFF8…`, JS
  materializes `0x7FF8…000`), so storing them verbatim would make the cores' files disagree. A
  stored value is **in-contract** (§8), so an exempt/computed NaN's bits must not contaminate it
  ([determinism.md](determinism.md) §4 no-contamination) — the codec is the boundary that
  re-canonicalizes them. (This is a *NaN-only* normalization; unlike the comparison/key form §3 it
  does **not** collapse `-0 → +0`, since both zeros are already cross-core identical.) Byte-exact
  goldens `float32_table.jed` / `float64_table.jed` (`rust == go == ts == ruby`), the cross-core
  round-trip every type ships.
- **Key encoding** — `float-order-preserving` ([encoding.md](encoding.md) §2.8):
  canonicalize `-0 → +0` and all NaNs to one pattern, take the IEEE bits as a big-endian u64/u32,
  and **if the sign bit is set (negative) flip all bits, else flip just the sign bit** — the
  standard transform that maps the float total order (§3) monotonically onto unsigned byte
  order, with NaN's canonical pattern landing above `+Inf`. **EXERCISED**: a `f32`/`f64`
  `PRIMARY KEY` / ordered secondary index / `UNIQUE` key / FK target is **supported**, and so is a
  `float`-element **array** key (`f64[]`/`f32[]`, encoding.md §2.14) — float is the *last* scalar
  to become keyable, so every scalar is now keyable (only the recursive `composite` container
  stays `0A000`). This **reverses** the prior "permanent" narrowing: a float **at rest** is fully
  in-contract (§1 — storage, the total order, and the key bytes are all deterministic and cross-core
  byte-identical), so a float key built from in-contract values sorts identically in every core.
  The only determinism cost is that a **tainted** float (a transcendental result) stored into a key
  reaches *stored* order — a **bounded widening of the existing `float-transcendental` exemption's
  blast radius** (query-time → stored order), not a new exemption ([determinism.md](determinism.md)
  §4), and PG-faithful (PostgreSQL admits `float8`/`float4` btree keys). Because `-0 = +0` and all
  NaNs canonicalize to one key, a `UNIQUE` float key treats them as one. The `(value → bytes)`
  fixtures are [../encoding/float.toml](../encoding/float.toml) and the on-disk images are pinned by
  the `float64_pk_table.jed` / `float32_pk_table.jed` goldens.
- **Cost** — arithmetic and function nodes charge the uniform `operator_eval`; aggregates
  charge `aggregate_accumulate` per row (§7). All structural ⇒ deterministic and cross-core
  ([cost.md](cost.md)); float queries carry `# cost:` like any other.

## 11. Determinism trap checklist (the cross-core / exemption boundary)

1. **Total order, not IEEE compare** — `-0 = +0`, `NaN = NaN`, `NaN` largest; `=`/order/dedup/
   keys all use it. A core using raw IEEE `<`/`==` (NaN unordered) diverges.
2. **Negative-zero canonicalization** — `-0 → +0` in the comparator and key encoder (and so in
   `DISTINCT`/`GROUP BY`); stored bits preserved.
3. **NaN is input-only** — finite arithmetic traps (`22003`/`22012`) instead of producing
   Inf/NaN; domain errors trap. NaN/±Inf enter via literals/casts/stored values and propagate.
4. **FMA discipline (G3)** — the kernel is safe via the tree-walking evaluator; any hand-rolled
   `a*b+c` (transcendentals, the §7 accumulator) uses the `f64(a*b)+c` barrier in Go.
5. **Exact accumulator** — `SUM`/`AVG` round once over an order-independent exact sum; special
   values resolved before the finite sum. Hand-rolled identically per core (no library drift).
6. **In-contract vs exempted** — storage, ordering, the kernel, exact functions (incl `sqrt`),
   exact-sum aggregates, text rendering (§9), and **cost/names/types** are bit-identical (G1–G3).
   Only transcendental *values* and the float `SUM`/`AVG` fold order are exempted, absorbed by the
   `R` tag's tolerant compare + the ledger; float-gated control flow is the one promotion path
   (§1, §4).
7. **`R` tag compares by value** (parse to f64 + tolerance), never by string; NaN==NaN, ±Inf
   exact, -0==+0.
8. **Strict coercion** — no implicit `int`/`decimal` ⊕ `f64` (`42804`); only literals adapt
   to a float context; all casts explicit.
