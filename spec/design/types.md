# Type system — design

> The reasoning behind the type-system data tables. The **data is authoritative**
> ([../types/scalars.toml](../types/scalars.toml),
> [../types/compare.toml](../types/compare.toml),
> [../types/casts.toml](../types/casts.toml)); this doc is the *why*. When a decision here
> changes, change it in the data and here in the same edit, and update
> [CLAUDE.md](../../CLAUDE.md) if it revises a load-bearing commitment.

The type system is **the product** (CLAUDE.md §4): a deliberate, strict, static type
system — "like SQLite, but with a real type system." It is designed as data, before the
executor, so that every implementation tests against one shared contract instead of
discovering semantics in code.

## 1. Scope: signed integers only (for now)

Step 1 implements exactly three scalar types (CLAUDE.md §4):

| Canonical id | Aliases | Bits | Range |
|---|---|---|---|
| `int16` | `smallint` | 16 | −32768 … 32767 |
| `int32` | `int`, `integer` | 32 | −2147483648 … 2147483647 |
| `int64` | `bigint` | 64 | −9223372036854775808 … 9223372036854775807 |

All are signed, two's-complement. Every other scalar (`decimal`, `text`, `boolean`,
`timestamp`/`timestamptz`, `bytea`, `json`/`jsonb`) is **deferred**. A direct consequence:
the float-formatting, decimal-rounding, NaN/∞-ordering, and collation decisions in CLAUDE.md
§8 do **not** bind this step — there are no floats, decimals, or text yet. That is the
point of starting here: the first slice exercises the whole multi-core machinery against
the *smallest* type surface that still has real, divergence-prone behavior (overflow,
promotion, order-preserving encoding).

## 2. Canonical names vs. aliases

Each type has one **canonical id** (`int16`/`int32`/`int64`) plus accepted SQL aliases. The
canonical id is the single name that appears in error messages, the catalog, and the
conformance corpus's `query` column-type tags. Why one canonical name: determinism
(CLAUDE.md §10). If two implementations could each pick a different spelling — `smallint`
vs `int16` — in output, the conformance corpus would spuriously diverge. Aliases are an
input convenience only; they normalize to the canonical id immediately at parse time.

We name the canonical types by their width **in bits** (`int16`/`int32`/`int64`) — the
convention common across programming languages (Rust `i16`, Go `int32`, …) — rather than
PostgreSQL's byte-count spellings (`int2`/`int4`/`int8`). The SQL-standard names
(`smallint`, `integer`/`int`, `bigint`) are kept as **aliases** so ordinary SQL
(`CREATE TABLE t (x smallint)`) still works; PG's `int2`/`int4`/`int8` are **not** accepted
(we own our surface — CLAUDE.md §1). The canonical choice is arbitrary-but-fixed; what
matters is that it is fixed.

## 3. Integer overflow: trap, never wrap

When an operation would produce a value outside a type's range, the engine **traps** —
raises `22003` (`numeric_value_out_of_range`,
[../errors/registry.toml](../errors/registry.toml)) — rather than wrapping.

CLAUDE.md §8 left this as "defined wrap vs. trap." We choose **trap** because silent
wraparound is exactly the runtime reinterpretation a strict static type system exists to
prevent (CLAUDE.md §4): `int16` holding `32767` plus `1` must not become `−32768`. Trap is
also PostgreSQL's behavior, which §1 lets us borrow where principled. Wrap is the rejected
alternative; if a wrapping operation is ever wanted it will be a *distinct, explicitly
named* operator, not the default `+`.

This applies uniformly to arithmetic (when the operator catalog lands), to literals that
don't fit their target column, and to narrowing casts (§5).

## 4. Comparison, promotion, three-valued NULL

See [../types/compare.toml](../types/compare.toml).

**Promotion tower.** The three integer types form one ordered family by `rank`:
`int16 (1) < int32 (2) < int64 (3)`. When two integers meet, both promote to the
higher-ranked type (`strategy = "max-rank"`) and are compared there. Widening is always
lossless, so promotion never loses information or traps.

**Comparability.** Only listed `(family, family)` pairs may be compared; everything else
is a type error. Step 1 has a single family, hence a single rule (`integer × integer`).
This table is where cross-family comparison rules (e.g. integer ↔ decimal) will be added
deliberately, rather than falling out of implicit coercions.

**Three-valued NULL logic** (CLAUDE.md §4). Any comparison with a NULL operand is
`UNKNOWN` (represented as NULL), never TRUE/FALSE. Notably `NULL = NULL` is `UNKNOWN`:
equality is **not** reflexive across NULL. Testing for NULL is done with `IS [NOT] NULL`
and `IS [NOT] DISTINCT FROM` (the latter arrives with the operator catalog). This is the
PG model, borrowed because it is principled.

**Value ordering & NULL position.** Non-NULL integers use plain signed numeric ascending
order, which is exactly what the key encoding (§7) reproduces in raw bytes. NULL's position
in the physical total order is now **ratified** (it was deferred to the key-encoding step):
**NULLs sort first** (before every present value) in ascending order, via a leading `0x00`
presence tag on a nullable key slot; descending inverts this (NULLs last). See
[encoding.md §4](encoding.md) and `null_ordering` in
[../types/compare.toml](../types/compare.toml). A SQL-level `ORDER BY ... NULLS
FIRST|LAST` override is a separate, later feature that layers on top of this physical
order.

## 5. Coercion / casts

See [../types/casts.toml](../types/casts.toml). The matrix is **strict**: any `(from, to)`
not listed is forbidden. Identity casts are implicit and always succeed (implied, not
listed).

- **Widening** (`int16→int32`, `int16→int64`, `int32→int64`) is lossless, so it is
  **implicit** — inserted automatically wherever a wider integer is wanted.
- **Narrowing** (`int64→int32`, `int64→int16`, `int32→int16`) is lossy in general, so it
  requires an **explicit** `CAST(...)` and **traps** (`22003`) when the value does not fit.

Cast modes are `implicit` / `assignment` / `explicit`. `assignment` (allowed on
INSERT/UPDATE into a column but not in general expressions) is part of the vocabulary for
future types; for the integer set, widenings are fully implicit so no edge needs the
weaker `assignment` mode yet.

## 6. Integer-literal typing

A bare integer literal (`1000`, `-32768`) is an **untyped integer constant** — in the
spirit of Go's and Rust's untyped constants. It has no intrinsic type; it acquires one from
its **context**, and **traps `22003`** if its value does not fit that context type. This
keeps a literal from silently forcing a width, and makes `WHERE small = 100000` (where
`small` is `int16`) a type error rather than a value that silently never matches.

- **Lexing first.** A literal is parsed as a signed 64-bit value; a magnitude beyond int64
  is a *syntax* error (`42601`, [../grammar/grammar.ebnf](../grammar/grammar.ebnf)), decided
  before any typing applies.
- **Assignment context** (`INSERT ... VALUES`, `UPDATE ... SET col = lit`): the context is
  the target column's type. The literal adapts to it — accepted iff in range, else `22003`.
- **Cast context** (`CAST(lit AS T)`): the context is `T`; range-checked, else `22003`.
- **Comparison context** (`col <op> lit`): the context is the column's type. The literal
  adapts to it; if out of range the predicate **traps `22003` deterministically, before any
  row is scanned** — a literal that cannot be represented in the column's type is a type
  error, not a silent non-match, for every operator (`=`, `<`, `>`, `<=`, `>=`). In range,
  the comparison proceeds within that type (no promotion needed). A `NULL` literal is exempt
  (it is the absence of a value).
- **No context** (a bare projected literal, `SELECT 1000`): defaults to **int64**, the
  widest integer.

**Forward — arithmetic** (when the operator catalog's arithmetic lands): an untyped literal
operand adapts to the *other* operand's type (`small + 1000` → `int16`, and may overflow per
§3); a literal meeting only literals (`1000 + 1`) defaults to int64. Recorded here so the
arithmetic slice inherits the rule rather than re-deciding it.

**Why this, not "smallest fitting" or "always int64".** Smallest-fitting makes ordinary
arithmetic overflow surprisingly (`30000 + 30000` would be `int16 + int16`). Always-int64
removes the type error in a comparison (an out-of-range literal would silently never match).
Context-adaptation gives each literal exactly the type its use demands and surfaces an
impossible literal as a `22003` the moment it is resolved — consistent with the strict cast
matrix (§5: adaptation is a value-checked coercion, never a silent reinterpret), the
promotion tower (§4: once typed, a literal participates like any value), and trap-on-overflow
(§3).

## 7. Order-preserving key encoding

See [../encoding/](../encoding/); the per-type rule is the `encoding` field in
[../types/scalars.toml](../types/scalars.toml). Full `(value → bytes)` fixtures are
produced at CLAUDE.md §11 step 4; the **rule** is fixed now because it is a property of the
type.

Stored keys iterate in raw byte order (`memcmp`), so an encoding is correct only if it
sorts byte-for-byte identically to the values' logical order (CLAUDE.md §8). The rule for
integers (`method = "int-be-signflip"`):

1. **Fixed-width big-endian.** Lexicographic byte comparison reads the most-significant
   byte first, so the MSB must be stored first — that is big-endian. Little-endian would
   make `memcmp` decide on low-order bytes first and invert the order (e.g. `1` vs `256`).
   Big-endian is *forced* by "keys sort by raw bytes," not a preference.
2. **Sign-bit inversion.** Two's-complement negatives have the high bit set, so as raw
   unsigned bytes they would sort *above* positives. Inverting the sign bit (XOR `0x80` on
   the leading byte ≡ adding 2^(bits−1)) maps the signed range monotonically onto the
   unsigned range, so negatives sort below positives.

This composes: descending order is bitwise inversion of a component, and composite keys
are concatenation of fixed-width components. CockroachDB's `encoding` package is the
reference design (CLAUDE.md §8). The encoding is emitted big-endian regardless of host CPU
endianness, so the byte fixtures are identical across Rust and Go — that cross-language
byte-identity is the whole point.

## 8. Determinism checklist (this step)

- ✅ One canonical name per type in all output.
- ✅ Trap (deterministic error `22003`) instead of platform-dependent wraparound.
- ✅ Promotion is total and order-independent (`max-rank`).
- ✅ Value order == key byte order (no separate, possibly-divergent comparator).
- ✅ NULL's physical total-order position — ratified NULLs-first (ascending), see §4.

## 9. Open / deferred

- **NULL sort position** — ✅ ratified NULLs-first (ascending) with the key-encoding spec
  (§4, [encoding.md §4](encoding.md)). No longer open.
- **Operator result types** — `int + int`, etc. live in [../functions/](../functions/),
  authored when the operator catalog lands (needed by the step-5 vertical slice).
- **`assignment`-mode casts** — vocabulary reserved; first used by non-integer types.
- **Everything non-integer** — the rest of the scalar set, per CLAUDE.md §4.
