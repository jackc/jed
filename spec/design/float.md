# The `float32` / `float64` types — design

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

`float64` is the engine's **approximate** numeric: IEEE 754 binary64 (double precision). It
is deliberately the opposite of `decimal` ([decimal.md](decimal.md)) — `decimal` is exact,
base-10, finite, and fully cross-core deterministic; `float64` is inexact, base-2, admits
NaN/±Infinity, and is the **first type exempted from cross-core byte-identity** (CLAUDE.md
§2/§8). The exemption is **narrow and deliberate** (determinism.md §1–§6): determinism is
preserved everywhere it is *reasonably easy*, and surrendered only where IEEE/libm make it
genuinely hard.

## 1. Why floats at all, and the determinism stance

`decimal` already owns exact numerics (the §8 bias is "keep `f64` out of compare/text"), so
`float64` exists only for what decimal cannot serve: ingesting/storing real-world
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
- **The exact-sum aggregates** `SUM`/`AVG` — defined as the *correctly-rounded exact sum*
  via a long accumulator (§7), which is **order-independent by construction** → identical
  across cores *and* across any future parallel/serial plan (determinism.md §7).
- **`MIN`/`MAX`/`COUNT`** — order-independent for the total order (§3).
- **Cost** — structural (one `operator_eval` per node, one `aggregate_accumulate` per row);
  it depends on the *count* of operations, never the float *value*, so it stays fully
  deterministic and cross-core, and float queries still carry `# cost:` assertions.

**Exempted from cross-core identity (G2/G3) — ledgered (`determinism_exceptions.toml`):**

- **Transcendental function *values*** (`exp`, `ln`, `log10`, `pow`/`^`, `sin`, `cos`, `tan`,
  `atan2`, …) — not IEEE-correctly-rounded; every libm differs by an ULP (§8).
- **Text rendering layout** — each core uses its native shortest-round-trip formatter (§9);
  the corpus's **`R`** render tag compares by *parsing back to f64 within a tolerance*, so
  layout differences and a transcendental's last-ULP divergence never fail a test.
- **Float-gated control flow** — a query whose *row multiset* depends on an exempted value
  (a transcendental result near a `WHERE`/`ORDER BY … LIMIT` boundary) is cross-core
  unspecified (the §4 contamination rule of determinism.md). Bounded by keeping float **out
  of keys** (§10).

Net: a float query that does not call a transcendental is, in practice, cross-core identical;
the exemption bites only at transcendentals, the PG oracle, and the rendering layout the
`R` tag already absorbs.

## 2. Scope — two widths forming a promotion tower

Two types, a **promotion tower** like the integers (canonical ids state width in bits, the
`int16`/`int32`/`int64` convention — [types.md](types.md) §2):

| Canonical id | rank | Aliases | IEEE | On-disk code |
|---|---|---|---|---|
| `float32` | 1 | `real` | binary32 (single, ~6 digits) | 13 |
| `float64` | 2 | `double precision`, `float` | binary64 (double, ~15 digits) | 12 |

**Naming, against the C/Java intuition:** in PostgreSQL a bare **`float` (no precision) is
`double precision` (64-bit)** — *not* 32-bit; the 32-bit type is spelled `real` (`float4`).
So `float` aliases `float64`, and `real` aliases `float32`. PG's `float8`/`float4` byte-count
spellings and the `float(p)` precision typmod are **not** accepted (we own our surface —
CLAUDE.md §1, as `int2/4/8` are rejected).

**The tower (compare.toml `max-rank`).** When two floats of different width meet
(arithmetic or comparison), both widen to the higher rank: **`float32 → float64`, which is
lossless** (every binary32 is an exact binary64), so it is also an **implicit cast** — the
float analogue of `int16 → int64`, and matching PG (`real` promotes to `double`). `float64 →
float32` is lossy and **explicit**. Crossing *out of* the float family (int/decimal ↔ float)
stays **explicit** either way (§6) — within-family widening is the *only* implicit float edge.

**Everything below applies to each width** (the total order §3, the trap model §3, the kernel
§5, the exact-sum aggregates §7, the functions §8, rendering §9), differing only by width:
`float32` arithmetic rounds to binary32 (Rust `f32`, Go `float32`, **TS `Math.fround` on every
op/literal/cast** — the one extra determinism discipline a second width adds), stores 4 bytes,
and `SUM`/`AVG` round at the input width. A mixed-width binary op promotes to `float64` first,
so the actual computation is always at one width.

## 3. Representation, the total order, and special values

**Representation.** A value is an IEEE 754 binary64: sign, 11-bit exponent, 52-bit mantissa.
NaN and ±Infinity are **first-class values** (unlike `decimal`, which excludes them —
decimal.md §2 — because it has no float source; `float64` is that source). This is the
`timestamp` precedent generalized: `timestamp` already carries ±infinity as totally-ordered
sentinels (timestamp.md), and `float64` does the same for ±Inf and NaN.

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
`NaN = NaN` is **TRUE** in jed (PG's float8 `=` agrees). `float64 × float64` is the only
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

There is no dedicated float literal token; a float value is written one of two ways (the
existing literal machinery — types.md §6, grammar.md §36):

- **A decimal literal adapting to a float context.** A `.`-bearing numeric constant is an
  untyped decimal constant (decimal.md §6) that adapts to its context. In a **float context**
  — `INSERT`/`UPDATE` into a `float64` column, a comparison against one (`WHERE f = 1.5`), or
  the other operand of float arithmetic — it coerces **decimal → float64** at resolve time
  (the nearest binary64 to the exact decimal, round-ties-to-even, the IEEE conversion). An
  integer literal adapts the same way. This is *literal adaptation*, not an implicit
  cross-family cast (§6): a bare literal carries no type until its context names one.
- **The typed literal `float '…'`** (and `float64 '…'`, `CAST('…' AS float64)`) — the
  `type 'string'` form (grammar.md §36). The string is parsed by float64's input function:
  an optional sign, decimal digits with an optional point and **`e`-notation** (`1.5e10`,
  `-3E-7` — the same e-notation a bare decimal literal now takes, grammar.md §14, here via
  float64's own string parse), plus the case-insensitive special words
  **`Infinity`/`+Infinity`/`-Infinity`/`inf`/`NaN`** (PG's `float8in` spellings). Malformed
  input traps **`22P02`** (`invalid_text_representation`) deterministically at resolve, before
  any row is scanned; a value outside the binary64 range traps **`22003`**.

## 5. Arithmetic — the correctly-rounded kernel

`float64 ⊕ float64 → float64` for `+ − * /` and unary `−` (and `%`/`mod`, IEEE `fmod`, exact).
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
   defeat fusion with the spec-blessed barrier `float64(a*b) + c` (a named intermediate is
   *not* a guaranteed barrier).
2. **No x87 extended precision** (modern SSE2/ARM64/WASM scalar f64 — a build note, not a code
   path), **no flush-to-zero** (subnormals computed, not zeroed), **round-ties-to-even** (the
   default everywhere; never changed).

So the kernel keeps G1–G3. Only transcendentals (§8) leave the contract.

## 6. Coercion and casts — strict, no implicit cross-family

`float64` is its own comparison/arithmetic family. **No implicit coercion** crosses into or
out of it (stricter than PG, justified by the strict type system — CLAUDE.md §4):

- `int ⊕ float64`, `decimal ⊕ float64`, `int = float64`, `decimal < float64`, … are
  **`42804`** datatype-mismatch errors. (PG promotes the other operand to `float8`; jed
  requires an explicit cast. A documented divergence, oracle-ledgered.) Only *literals* adapt
  to a float context (§4) — a *value* never silently becomes a float.

Casts (all **explicit** `CAST` — [../types/casts.toml](../types/casts.toml)), because every
one is lossy or representation-changing:

| from → to | mode | rule |
|---|---|---|
| `int{16,32,64} → float64` | explicit | nearest binary64, round-ties-to-even (exact ≤ 2^53; larger int64 may round). Never traps. |
| `decimal → float64` | explicit | nearest binary64 to the exact decimal value, round-ties-to-even. Never traps (a huge decimal → ±Inf? **traps `22003`** rather than yielding Inf, matching the finite-overflow rule §3). |
| `float64 → int{16,32,64}` | explicit | round **half away from zero** to an integer (jed's one rounding mode — decimal.md §3), then range-check (`22003`). NaN/±Inf → `22003`. **Documented divergence from PG**, which rounds half-to-even (`rint`); jed keeps one engine-wide mode. |
| `float64 → decimal` | explicit | the exact decimal of the binary64 value, then the target typmod's scale coercion (decimal.md §3). NaN/±Inf → `22003` (decimal is finite). |
| `text ⇄ float64` | — | the `float '…'` literal coercion (§4) is the text→float *literal* path; a **runtime** `CAST(text_col AS float64)` and `CAST(float_expr AS text)` are **deferred `0A000`** (the general runtime-text-cast slice — types.md §5), exactly as for the other types. |

`int`/`decimal` → `float64` is explicit (not implicit like `int → decimal`) precisely because
it is **lossy** — the whole point of the strict matrix.

## 7. `SUM` / `AVG` — the order-independent exact accumulator

Naive float summation is non-associative, so its result depends on the order rows are folded
— which violates G1 under future parallelism *and* G2 across cores (determinism.md §7). jed
therefore defines float `SUM`/`AVG` as an **order-independent canonical-order fold**: the inputs
are reduced in a *canonical order fixed by the data, not by row order*, so the result is identical
regardless of scan/partition order and **bit-identical across cores** — the in-contract,
determinism-preserving resolution (determinism.md §7, A). It is a documented divergence from PG
(whose float sum is order-dependent and sloppy); the value stays within the `R`-tag tolerance of
PG (§9). (A strictly *correctly-rounded* exact accumulator — round-once over the true mathematical
sum — is a future refinement; it is harder to keep byte-identical across three hand-written cores
— the §2/§5 drift hazard — and unnecessary for the contract, which only requires order-independence
+ cross-core identity, both of which the canonical fold guarantees.)

**Algorithm** (the identical steps every core runs — CLAUDE.md §2/§5):

1. **Special values first** (order-independent): if any input is NaN → result `NaN`; else if
   both `+Inf` and `-Inf` appear → `NaN`; else if `+Inf` appears → `+Inf`; else if `-Inf`
   appears → `-Inf`; else all-finite → step 2. NULL inputs are skipped (as every aggregate).
2. **Canonicalize and sort.** Map each finite input's `-0.0 → +0.0`, then sort the values by the
   §3 total order (equivalently, by the `float-order-preserving` key — encoding.md §2.8). After
   `-0` canonicalization and NaN/Inf extraction, distinct values have distinct keys, so the sort
   is **total and deterministic** — every core sees the same sequence.
3. **Fold left** with width-correct IEEE addition (round-ties-to-even per add; `float32` via the
   width's rounding — TS `Math.fround` each step). A running total that overflows to ±Inf → `22003`
   (the §3 finite-overflow rule; PG yields ±Inf — a documented divergence). One canonical order +
   one rounding rule ⇒ bit-identical across cores and across any serial/parallel plan.

`AVG` = `SUM / count` (count exact; the division rounded once at the input width), NULLs skipped,
empty group → `NULL`. **Result types**: `SUM`/`AVG(float32) → float32`, `SUM`/`AVG(float64) →
float64` (a float sum/avg stays the input width — `same_as_input`, matching PG `sum(real) → real`;
AVG over float stays float, unlike `AVG(int) → decimal`, and a minor divergence from PG which
widens `AVG(real) → double`). `MIN`/`MAX(floatN) → floatN` (the §3 total order), `COUNT → int64`.

**Cost.** One `aggregate_accumulate` per input row (the accumulator add is O(1) amortized),
deterministic and cross-core — so float aggregate queries keep `# cost:` assertions even
though their *values* are PG-oracle-only.

## 8. Functions — the exact set (in-contract) vs the transcendental set (exempted)

Float scalar functions split by whether they are correctly-rounded:

**Exact / correctly-rounded — in-contract (G1–G3):** `abs`, `ceil`, `floor`, `trunc`, `round`
(half away from zero — the engine's one mode; `round(f)` and `round(f, n)`), `sign`, and
**`sqrt`** (IEEE-mandated correctly-rounded). These are bit-identical across cores and carry
exact `R`-tag assertions; they reuse the existing scalar-function machinery (functions.md §9,
the `abs`/`round` precedent) with `float64` overloads.

**Transcendental — exempted (G2/G3 dropped, ledgered):** `exp`, `ln`, `log10`, `log(b, x)`,
`pow(x, y)` / the `^` operator, `sin`, `cos`, `tan`, `asin`, `acos`, `atan`, `atan2`, `cbrt`.
Each core calls its native libm; results may differ in the last ULP across cores and from PG.
These get a **`determinism_exceptions.toml` entry** (class **A**, drops G2/G3, blast radius =
the result column, promoting only via float-gated control flow §1), are compared by the `R`
tag's tolerant rule (§9), and are **PG-oracle-only** in the corpus (an `oracle_overrides.toml`
note where PG's ULP differs). Domain errors (`sqrt(-1)`, `ln(-1)`, `ln(0)`) follow PG: `ln(0)`
→ `22003`; `sqrt`/`ln` of a negative → `22003` (`argument ... out of range`) rather than
returning NaN — keeping NaN an *input-only* value (§3).

The transcendental list is a generous starting set; further functions are easy additive
follow-ons (each one operator-catalog row + a ledger line).

## 9. Rendering and the `R` conformance tag

**Rendering.** A `float64` renders with each core's **native shortest round-trip** formatter
(Rust `{}`, Go `strconv.FormatFloat(f, 'g', -1, 64)`, JS `Number.prototype.toString`),
producing the shortest decimal string that parses back to the same binary64. Special values
render PG-style: `Infinity`, `-Infinity`, `NaN` (and `-0` renders `-0`). The *digits* of
shortest-round-trip are mathematically unique, so the cores already agree on them; only the
*layout* (exponent threshold/spelling) may differ — which the `R` tag absorbs, so jed does
**not** hand-roll a shared formatter (the exemption's payoff, determinism.md §6).

**The `R` (real) render tag** (conformance.md §1 — long reserved, now in use). A column tagged
`R` is compared **by value, not by string**: both expected and actual are parsed to f64 and
considered equal iff bit-equal **or** within a small relative/ULP tolerance, with `NaN` ==
`NaN`, `±Inf` exact, and `-0` == `+0`. This single rule covers (a) cross-core layout
differences, (b) a transcendental's last-ULP cross-core divergence, and (c) the larger
jed-vs-PG divergence in the oracle import (PG formatting + PG libm). The in-contract surface
(kernel, exact functions, exact-sum aggregates) is bit-identical and would pass an exact
compare; the tolerance exists for the exempted surface and the oracle. `# cost:`/`# names:`/
`# types:` are unaffected — they are structural and stay exact (§1).

## 10. On-disk, keys, and cost

- **On-disk value codec** — stable **type code 12** (`float64`, 8 bytes) and **13** (`float32`,
  4 bytes) (format.md). The body is the IEEE bytes, **big-endian** (`float64`: Go
  `math.Float64bits`, TS `DataView.setFloat64(_, false)`, Rust `f64::to_bits().to_be_bytes()`;
  `float32`: Go `math.Float32bits`, TS `setFloat32(_, false)`, Rust `f32::to_bits().to_be_bytes()`)
  behind the shared presence tag (NULL = tag only). Fixed-width, so no length prefix — like
  `uuid`/`timestamp`. The stored bits are preserved **verbatim for every value except `NaN`**: a
  stored `-0.0` keeps its sign bit, and `±Infinity`/finite values keep theirs, but a `NaN` is
  **canonicalized to the single quiet pattern** `0x7FF8000000000000` (`float64`) / `0x7FC00000`
  (`float32`) on the way to disk. This NaN-only step is the one storage divergence from verbatim,
  and the determinism contract forces it: a NaN's *payload* bits are **core-specific** (Go's
  `math.NaN()` is `0x7FF8…001`, Go/Rust hardware `Inf − Inf` is the negative `0xFFF8…`, JS
  materializes `0x7FF8…000`), so storing them verbatim would make the cores' files disagree. A
  stored value is **in-contract** (§8), so an exempt/computed NaN's bits must not contaminate it
  ([determinism.md](determinism.md) §4 no-contamination) — the codec is the boundary that
  re-canonicalizes them. (This is a *NaN-only* normalization; unlike the comparison/key form §3 it
  does **not** collapse `-0 → +0`, since both zeros are already cross-core identical.) Byte-exact
  goldens `float32_table.jed` / `float64_table.jed` (`rust == go == ts == ruby`), the cross-core
  round-trip every type ships.
- **Key encoding** — `float-order-preserving` ([encoding.md](encoding.md) §2.8, to author):
  canonicalize `-0 → +0` and all NaNs to one pattern, take the IEEE bits as a big-endian u64,
  and **if the sign bit is set (negative) flip all 64 bits, else flip just the sign bit** — the
  standard transform that maps the binary64 total order (§3) monotonically onto unsigned byte
  order, with NaN's canonical pattern landing above `+Inf`. **Authored but unexercised this
  slice**: a `float64 PRIMARY KEY`/index is rejected **`0A000`** (the text/decimal/bytea/
  interval precedent — and the determinism.md §4 contamination argument: keeping float out of
  keys bounds an exempted value to *query-time* order, never *stored* order). Lifting it adds
  the byte-vector fixtures + the executor key path.
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
   `a*b+c` (transcendentals, the §7 accumulator) uses the `float64(a*b)+c` barrier in Go.
5. **Exact accumulator** — `SUM`/`AVG` round once over an order-independent exact sum; special
   values resolved before the finite sum. Hand-rolled identically per core (no library drift).
6. **In-contract vs exempted** — storage, ordering, the kernel, exact functions (incl `sqrt`),
   exact-sum aggregates, and **cost/names/types** are bit-identical (G1–G3). Only transcendental
   *values* and rendering *layout* are exempted, both absorbed by the `R` tag's tolerant
   compare + the ledger; float-gated control flow is the one promotion path (§1, §4).
7. **`R` tag compares by value** (parse to f64 + tolerance), never by string; NaN==NaN, ±Inf
   exact, -0==+0.
8. **Strict coercion** — no implicit `int`/`decimal` ⊕ `float64` (`42804`); only literals adapt
   to a float context; all casts explicit.
