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
  operation produces, bounded only by the absolute cap below. Required: division and decimal
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

**Absolute cap (the one deliberate divergence from PG).** An unconstrained value is capped at
`max_precision = 1000` significant digits and `max_scale = 1000`; exceeding it traps `22003`.
PG's limit is far higher (131072 integer + 16383 fraction digits). jed caps lower **because a
single value must fit one page** — the same whole-image "oversized item" `0A000` narrowing that
bounds over-page text ([format.md](../fileformat/format.md)). 1000/1000 covers every monetary
and scientific use and equals the *declarable* maximum, so there is one number to remember.
**Relax this** toward PG's limit once an over-page-value mechanism (overflow pages / TOAST-style
out-of-line storage) lands (CLAUDE.md §9, TODO.md Phase 6); note it here at the cap.

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
| `*` | **`s1 + s2`** | `coeff = C1·C2`, exact, no rounding. `1.50 * 1.5 = 2.250`; `2.0 * 3.000 = 6.0000`. |
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
rscale = min(rscale, max_scale)             // 1000
```

The `4·` granularity and the `f1 ≤ f2` adjustment are PG's (`numeric.c select_div_scale`); they
make `1/3 → rscale 20` (f1=1 ≤ f2=3 ⇒ qweight −1 ⇒ 16+4) and `10.0/4.0 → rscale 16`
(f1=10 > f2=4 ⇒ qweight 0, then max with s=1). This is the single hardest function to keep
byte-identical — it is pinned with division fixtures (`1/3, 2/3, 1/7, 10/4, 1/8, 100/7`).

**Division itself** (`value1 / value2`, value2 ≠ 0): with `E = rscale + s2 − s1` (always `≥ 0`,
since `rscale ≥ s1`), form `N = |C1| · 10^E`; then `q = N div |C2|`, `r = N mod |C2|`, and
**`if 2·r ≥ |C2|: q += 1`** (half away, §3). Result = `(neg1 XOR neg2, q, rscale)`, normalized.
Division/modulo by zero traps **`22012`** (the integer trap, reused).

**Overflow.** A constrained result (a `numeric(p,s)` column/CAST target) traps `22003` by the
§2 store check. An unconstrained result traps `22003` only at the absolute cap (§2). So `+ − *
/` carry `errors = [22003]` and `div`/`mod` also `22012`; unary `neg` cannot overflow
(`errors = []`). The trap boundary is the *result*, mirroring the integer rule
([functions.md](functions.md) §7).

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

- **Literals** ([grammar.md](grammar.md) §14): a `.`-bearing numeric literal is an *untyped
  decimal constant* with its written scale, adapting to context like an integer literal
  ([types.md](types.md) §6). Into a `numeric(p,s)` target it rounds to `s` + precision-checks;
  with no decimal context it keeps its scale; a coefficient over `max_precision` digits or
  scale over `max_scale` traps `22003` at resolve.
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
  text-PK precedent). The on-disk **value** codec (type code 5,
  [format.md](../fileformat/format.md)) is what lands now.
- **Cost** ([cost.md](cost.md)): a decimal compare/arith node charges **one** uniform
  `operator_eval`, independent of coefficient length — like integer/text, so the `# cost:`
  contract is unchanged.

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
