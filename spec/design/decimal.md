# The `decimal` / `numeric` type — design

> The reasoning behind the exact-decimal type. The **data is authoritative**
> ([../types/scalars.toml](../types/scalars.toml) — the type, limits, encoding method;
> [../types/compare.toml](../types/compare.toml) — comparability/promotion;
> [../types/casts.toml](../types/casts.toml) — casts;
> [../functions/catalog.toml](../functions/catalog.toml) — operators;
> [../fileformat/format.md](../fileformat/format.md) — the on-disk value codec;
> [../grammar/grammar.ebnf](../grammar/grammar.ebnf) — literals + the typmod). This doc is
> the *why* and the precise arithmetic the three cores must reproduce **byte/digit-exactly**
> (CLAUDE.md §2, §8). When a decision here changes, change the data and here in the same
> edit, and update [CLAUDE.md](../../CLAUDE.md) §4/§8 if it revises a commitment.

`decimal` is the headline of the type system (CLAUDE.md §4): an **exact** base-10 numeric for
money and any computation that must not lose a cent to binary floating point. It keeps binary
floats out of the comparison and text-output paths entirely (CLAUDE.md §8) — there is no
`f64` anywhere in this type. PostgreSQL `numeric` (`numeric.c`) is the behavioral reference;
where jed diverges it is noted and justified.

## 1. Representation — sign + base-10⁹ coefficient + scale

A value is `(neg, coefficient, scale)` with logical value
**`(−1)^neg · coefficient · 10^(−scale)`**. The coefficient is a non-negative integer held as
a hand-rolled **limb array** whose base and order are internal implementation details, *not* a
cross-core contract (only the rendered value and the on-disk bytes are): Rust and Go use
base-10⁹ least-significant-limb-first (a `u64` accumulator holds the limb products); TS uses
**base-10⁴** so a limb product stays within JS's safe-integer range (2⁵³) without `bigint` on
the value path. `scale ≥ 0` is the number of fractional digits the value *displays* (its
"dscale", PG's term). Examples: `1.50` is
`(neg=false, coeff=150, scale=2)`; `1.5` is `(false, 15, 1)` — **equal in value, distinct in
display**; `−0.013` is `(true, 13, 3)`.

It is **hand-rolled identically in all three cores** (Rust `Vec<u32>`, Go `[]uint32`, TS
`Uint32Array`) — *not* a bignum library. Rust has no bignum and zero runtime deps (it
hand-rolls CRC32 and the int codec for the same reason); using Go `math/big` and TS `bigint`
while Rust hand-rolls would be **three** different implementations of multiply/divide/round,
and division rounding is exactly where two correct-but-different bignums silently diverge —
the CLAUDE.md §2/§5 "no reference implementation, code drifts N ways" hazard at its sharpest.
So the limb algorithm is the spec, pinned by the shared conformance corpus
([../conformance/suites/expr/decimal_arithmetic.test](../conformance/suites/expr/decimal_arithmetic.test),
[../conformance/suites/types/decimal.test](../conformance/suites/types/decimal.test)) and the
byte-exact golden [../fileformat/fixtures/decimal_table.jed](../fileformat/fixtures/) — all
three cores must produce identical results; `math/big`/`bigint` are permitted only as a
per-core *test oracle*, never on the value path.

**Normalization invariants** (a single `normalize` step every constructor ends with):

- No leading-zero limbs; the empty coefficient is the integer zero.
- **No negative zero**: `neg` is forced `false` whenever the coefficient is zero. A stray
  negative zero would produce different value/key bytes in one core — a determinism break.
- Scale is **carried**, never derived from the coefficient: `+ − *` preserve/compute it
  exactly (§4), so `1.50` round-trips as `1.50`.

Base 10⁹ (not 10⁴) in-core because a `u32` limb holds 9 decimal digits with a `u64`
accumulator; the **on-disk** codec regroups to base-10⁴ to match PG's digit size and keep the
golden fixtures reviewable ([format.md](../fileformat/format.md)). The regroup is a pure
power-of-ten split, authored once with fixtures.

## 2. Precision model — `numeric`, `numeric(p)`, `numeric(p,s)`

Three forms, matching PostgreSQL:

- **`numeric`** (no typmod) — *unconstrained*: a value carries whatever precision/scale an
  operation produces, bounded only by the format caps below. Required: division and decimal
  literals naturally produce unconstrained values.
- **`numeric(p)`** ≡ `numeric(p, 0)`.
- **`numeric(p, s)`** — `p` = total significant digits (**precision**), `s` = digits after the
  point (**scale**). Constraint **`1 ≤ p ≤ max_precision`** and **`0 ≤ s ≤ p`**
  (`max_precision = 1000`, [../types/scalars.toml](../types/scalars.toml)). A malformed typmod
  — `numeric(0)`, `numeric(1001)`, `numeric(5,7)`, non-integer — traps **`22023`**
  (`invalid_parameter_value`), PostgreSQL's SQLSTATE. (PG 15+ allows negative scale and
  `s > p`; jed keeps the classic SQL `0 ≤ s ≤ p`, a documented narrowing — TODO.)

**Storing a value into `numeric(p,s)`** (assignment on INSERT/UPDATE, or `CAST(x AS
numeric(p,s))`): **(1)** round the value to scale `s` (§3); **(2)** check that the *integer-part*
digit count `≤ p − s`; if not, trap **`22003`** ("numeric field overflow"). The stored value's
display scale becomes exactly `s` — trailing zeros are materialized, so `1.5::numeric(10,2)`
is `1.50`.

**Value-format caps — PostgreSQL's numeric format limits.** An unconstrained value is bounded
by the same two limits PG's storage format imposes (`numeric.c`): at most
**`max_int_digits = 131072` integer-part digits** (PG: `(NUMERIC_WEIGHT_MAX + 1) · DEC_DIGITS`,
the i16 base-10⁴ weight) and at most **`max_scale = 16383` fractional digits** (PG:
`NUMERIC_DSCALE_MAX`, the 14-bit dscale). Both live in
[scalars.toml](../types/scalars.toml). Exceeding either traps **`22003`** — at literal/input
resolve (PG `numeric_in`) and at every operation result (PG `make_result`) — with **one**
exception, faithful to PG `numeric_mul`: a multiplication whose exact product has more than
`max_scale` fractional digits **rounds** the product to scale 16383 (half away, §3) instead of
trapping, then applies the integer-digit check (oracle-confirmed: `1e-10000 * 1e-10000 = 0` at
dscale 16383). A value at both caps is ~147 455 digits ≈ 74 KB encoded — far over a page;
storable because large values spill to overflow chains and compress transparently
([large-values.md](large-values.md)), the mechanism the **original 1000/1000 cap was waiting
on** (this cap was lifted to PG's limits when that landed). The `SUM`/`AVG` accumulator
**checks the cap only on the *final* result, never an intermediate** — faithful to PG, whose
accumulator holds intermediates beyond the storable range and checks once at `make_result`. So
a fold whose running sum transiently exceeds the cap but lands back in range succeeds (and
`AVG`, whose final divide brings the value back, never traps on an over-cap intermediate sum);
only a final result over the cap traps `22003`. This is the **`add_uncapped` accumulator path**
([determinism.md](determinism.md) §7): folding through ordinary cap-checking `add` instead would
trap at *whichever* intermediate first crossed the cap, making the trap depend on summation
order — an order-dependent error this avoids (the in-fold `decimal_work` cost still meters each
add, charged before it, so a cost ceiling aborts an unbounded accumulation — §6, cost.md §6).
Declarability is unchanged: `numeric(p,s)` still requires
`p ≤ max_precision = 1000` (also PG's rule), so a *constrained* column never approaches the
format caps — only unconstrained `numeric` values can.

**Finite only — no NaN, no ±Infinity** (a deliberate, documented PG divergence). PG `numeric`
has `'NaN'` and `±'Infinity'`; jed's does not. There is **no source** for a non-finite value:
no float type exists, and `x / 0` / `x % 0` trap `22012` rather than producing ∞/NaN. Excluding
them keeps comparison, ordering, the codecs, and rendering free of special cases and fully
deterministic (CLAUDE.md §8). If ever needed they are an additive later feature.

## 3. Rounding — half away from zero

When a value is coerced to a target scale `t` (storing into `numeric(p,s)`, `CAST`, the
division result §4), it is **rounded half away from zero** — PG `numeric`'s mode, confirmed:
`0.125 → 0.13`, `−0.125 → −0.13`, `2.5 → 3`, `−2.5 → −3`. This is the **one** rounding mode in
the engine. (Banker's / half-to-even was considered for monetary bias but rejected: PG is the
behavioral default and the differential oracle, and one mode is simpler to keep byte-identical
across three hand-rolled cores — if half-to-even is ever wanted it is a separately-named
function, never the implicit mode.)

**Algorithm** (rounding magnitude coefficient `C` at scale `s` down to scale `t`, `t < s`):
let `d = s − t`, split `C = q·10^d + r` with `0 ≤ r < 10^d` (operate on the magnitude; the
sign is untouched — "away from zero" means *increase magnitude*). Then

```
if 2·r ≥ 10^d:  q += 1      // at or past halfway → round up in magnitude
else:           q stays      // below halfway → truncate
```

`10^d` is even for `d ≥ 1`, so the exact integer test `2·r ≥ 10^d` decides the exactly-half
case (`2r == 10^d`) as "up", with **no float and no division**. A carry may grow the
coefficient (`9.5 → 10`); re-normalize after. Rounding up to scale `t < s` then materializes
exactly `t` fractional digits (pad with zeros conceptually: the coefficient becomes `q`, scale
`t`). Coercing to a *larger* scale only appends zeros (exact).

## 4. Arithmetic — exact computation, PG-faithful result scale

Operands are aligned/combined in limbs **exactly**; the result's *display scale* follows these
rules (all confirmed against PG). Let operand scales be `s1, s2`. A mixed `integer ⊕ decimal`
pair promotes the integer to decimal (scale 0) first ([compare.toml](../types/compare.toml)),
so every case below is decimal ⊕ decimal. `neg`-result = `neg1 XOR neg2` unless noted.

| op | result scale | rule |
|---|---|---|
| `+` `−` | **`max(s1, s2)`** | align both to `s = max(s1,s2)` (multiply the lower-scale coefficient up — exact), add/subtract magnitudes by sign. `1.50 + 1.5 = 3.00`; `1.234 − 1.2 = 0.034`. |
| `*` | **`s1 + s2`** | `coeff = C1·C2`, exact, no rounding — unless `s1 + s2 > max_scale` (16383), where the exact product **rounds** to scale 16383 (§2, PG `numeric_mul`). `1.50 * 1.5 = 2.250`; `2.0 * 3.000 = 6.0000`. |
| `/` | **`select_div_scale`** (below) | long-divide to that scale, **rounded half away** (§3). `1/3 = 0.33333333333333333333`; `10.0/4.0 = 2.5000000000000000`. |
| `%` | **`max(s1, s2)`** | truncated-division remainder; sign of the **dividend**. `5.5 % 2 = 1.5`; `−5.5 % 2 = −1.5`. |
| unary `−` | scale unchanged | flip `neg` (forced false if zero). Never overflows. |

**`select_div_scale` — reproduce PG's rule exactly** (so the differential oracle needs no
overrides). For each operand compute, from its normalized `(coefficient C, scale s)`: the
**leading decimal exponent** `e = digits(C) − 1 − s` (the power of ten of the most-significant
digit; e.g. `123.45` → `e = 2`, `0.0034` → `e = −3`); the **base-10⁴ weight** `w = ⌊e / 4⌋`
(floor toward −∞); and the **leading base-10⁴ digit** `f = ⌊C / 10^(4w + s)⌋` (the top group of
1–4 decimal digits, `0 ≤ f ≤ 9999`). For a zero operand use `w = 0, f = 0`. Then

```
qweight = w1 − w2
if f1 ≤ f2:  qweight −= 1                  // quotient < 1 (or unsure) ⇒ one more weight
rscale = 16 − 4·qweight                     // 16 = NUMERIC_MIN_SIG_DIGITS, 4 = DEC_DIGITS
rscale = max(rscale, s1, s2, 0)
rscale = min(rscale, max_precision)         // 1000 — PG NUMERIC_MAX_DISPLAY_SCALE (= NUMERIC_MAX_PRECISION)
```

The final clamp is PG's **display-scale** limit (1000 = `max_precision`), deliberately *not*
the §2 `max_scale` value cap (16383): division never produces more than 1000 fractional
digits, even from max-scale operands (oracle-confirmed: `1e-16383 / 1` is `0` at scale 1000 —
the `max(…, s1, s2)` step is applied *before* the clamp). PG's `round(x, n)` likewise clamps
its scale argument, but at `max_scale` = 16383 (`numeric_round`); jed matches.

The `4·` granularity and the `f1 ≤ f2` adjustment are PG's (`numeric.c select_div_scale`); they
make `1/3 → rscale 20` (f1=1 ≤ f2=3 ⇒ qweight −1 ⇒ 16+4) and `10.0/4.0 → rscale 16`
(f1=10 > f2=4 ⇒ qweight 0, then max with s=1). This is the single hardest function to keep
byte-identical — it is pinned with division fixtures (`1/3, 2/3, 1/7, 10/4, 1/8, 100/7`).

**Division itself** (`value1 / value2`, value2 ≠ 0): with `E = rscale + s2 − s1` (always `≥ 0`,
since `rscale ≥ s1`), form `N = |C1| · 10^E`; then `q = N div |C2|`, `r = N mod |C2|`, and
**`if 2·r ≥ |C2|: q += 1`** (half away, §3). Result = `(neg1 XOR neg2, q, rscale)`, normalized.
Division/modulo by zero traps **`22012`** (the integer trap, reused).

**Overflow.** A constrained result (a `numeric(p,s)` column/CAST target) traps `22003` by the
§2 store check. An unconstrained result traps `22003` only at the §2 format caps — too many
integer digits for any operation; too large a scale for input (multiplication instead rounds,
§2). So `+ − * /` carry `errors = [22003]` and `div`/`mod` also `22012`; unary `neg` cannot
overflow (`errors = []`). `round(x, n)`'s carry can push a value at the integer-digit cap over
it (`round` of a 131072-nines integer), trapping `22003` like PG. The trap boundary is the
*result*, mirroring the integer rule ([functions.md](functions.md) §7).

## 5. Comparison and ordering

`decimal × decimal` compares by **exact value** ([compare.toml](../types/compare.toml),
`via = "none"`): align scales (multiply the lower-scale coefficient up — exact, no rounding),
then compare by sign then magnitude. So **`1.5 = 1.50` is true**, and order is numeric
(`−10 < −1 < 0 < 0.5 < 1 < 10`), NULLs last (the PG model — [types.md](types.md) §4). Equality
is **not** structural: two values equal in value but different in display scale are equal,
which is why dedup/`DISTINCT`/`GROUP BY` must key on a **value-canonical** form (strip trailing
*fractional* zeros, reducing scale: `1.50 → 1.5`, `100 → 100` unchanged), never the stored
`(coeff, scale)`. `integer × decimal` promotes the integer to decimal first
(`via = "promote-to-decimal"`), so `int_col = 1.5` is well-typed and simply never matches; a
`decimal × text` pair is a `42804` type error. `IS [NOT] DISTINCT FROM` is the same value
comparison with NULL treated as a comparable value (always definite).

## 6. Literals, casts, rendering, keys, cost

- **Literals** ([grammar.md](grammar.md) §14): a `.`-bearing numeric literal — or any
  significand carrying a scientific `e`-notation exponent (`5e2`, `1.5e3`, `1.5e-3`; an exponent
  makes even a point-free literal a decimal) — is an *untyped decimal constant* with its written
  scale, adapting to context like an integer literal ([types.md](types.md) §6). For an exponent
  the scale is `max(0, frac_digits − exponent)` and the value shifts by `10^exponent` (PG
  `numeric.c`). Into a `numeric(p,s)` target it rounds to `s` + precision-checks; with no decimal
  context it keeps its scale; a literal over the §2 format caps (integer part over
  `max_int_digits` digits, or scale over `max_scale`) traps `22003` at resolve (PG `numeric_in`).
- **Casts** ([casts.toml](../types/casts.toml)): `int → decimal` **implicit** (lossless, scale
  0); `decimal → int` **explicit** only (round to scale 0 half-away, then range-check, `22003`)
  — **stricter than PG**, which assignment-casts numeric→int; jed forbids the silent narrowing.
  Re-scaling to a target typmod (`CAST(x AS numeric(p,s))`) is the §2 store coercion.
- **Rendering** (conformance tag **`D`**, [conformance.md](conformance.md) §1): the canonical
  text is an optional `-`, the integer digits (no leading zeros beyond a single `0`), and —
  iff `scale > 0` — a `.` and **exactly `scale`** fractional digits (zero-padded). So `1.50`
  renders `1.50`, `0.00` renders `0.00`, `-0.013` renders `-0.013`, scale-0 `123` renders
  `123` (no point). Matches PG's display; a determinism surface every core must reproduce.
- **Key encoding** ([encoding.md](encoding.md) §2.5, `decimal-order-preserving`): authored but
  **unexercised** this slice — a decimal `PRIMARY KEY`/index key is rejected `0A000` (the
  text-PK precedent). The on-disk **value** codec (type code 6,
  [format.md](../fileformat/format.md)) is what lands now.
- **Cost** ([cost.md](cost.md) §3 "`decimal_work`"): a decimal compare/arith node charges its
  uniform `operator_eval` **plus `decimal_work` × (W − 1)**, W being the operation's work in
  base-10⁴ digit groups (add/sub/compare/mod scale with the larger aligned operand; mul/div
  with the *product* of the operands' group counts). Small values (≤ 4 aligned digits) have
  W = 1 and charge nothing extra — pre-existing `# cost:` assertions are unchanged — while
  the quadratic big-value operations the §2 caps now allow accrue cost proportional to their
  real limb work, charged *before* the work runs so a cost ceiling (cost.md §6) aborts ahead
  of it (CLAUDE.md §13).

## 7. Determinism traps (the cross-core checklist)

1. **Display-scale preservation** — `1.50 ≠ 1.5` in display; carry scale in the value, render
   exactly `scale` fractional digits. A core that "normalizes away" trailing zeros diverges.
2. **Division scale + rounding** — the #1 risk; reproduce `select_div_scale` (§4) and the
   `2·r ≥ |C2|` half-away test identically; pin with fixtures.
3. **Negative zero** — force `neg = false` on any zero coefficient in *every* constructor
   (literal, `1.0 − 1.0`, `−0.4` rounded to `0`, `×0`).
4. **Half-away edge** — `2·r == 10^d` rounds up in magnitude for both signs (`±0.125 → ±0.13`);
   use the exact-integer test, never a float compare.
5. **TS** — use `Uint32Array` limbs, never `bigint`, on the value path (bigint only as a test
   oracle), so all three cores run the same steps.
6. **Limb ⇄ base-10⁴ regroup** — exact and identical for the on-disk codec.
7. **Literal parse** — `1.50`, `1.`, `.5`, leading/trailing zeros, the digit cap → identical
   `(neg, coeff, scale)` in all three lexers; cap overflow → `22003`.
8. **Value vs structural equality** — `WHERE`/`DISTINCT`/`GROUP BY` use value equality (§5),
   never the stored `(coeff, scale)`; the value-canonical key gives `1.5` and `1.50` one bucket.
9. **Mixed int/decimal promotion** — `int ⊕ decimal` promotes identically and is
   order-independent for the result scale.
10. **`%` sign & truncation** — remainder takes the dividend's sign with a toward-zero
    quotient, matching the integer `%` convention (one mental model).
11. **`decimal_work` group counts** — W is computed from the *logical* significant-digit
    counts in base-10⁴ groups ([cost.md](cost.md) §3), never from a core's internal limb
    count (Rust/Go hold base-10⁹ limbs, TS base-10⁴ — limb counts differ, group counts do
    not); division's W uses the same `select_div_scale` as its result.
