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

## 1. Scope: signed integers (storable) + boolean (expression-only)

The storable scalar types are exactly three signed integers (CLAUDE.md §4):

| Canonical id | Aliases | Bits | Range |
|---|---|---|---|
| `int16` | `smallint` | 16 | −32768 … 32767 |
| `int32` | `int`, `integer` | 32 | −2147483648 … 2147483647 |
| `int64` | `bigint` | 64 | −9223372036854775808 … 9223372036854775807 |

All are signed, two's-complement. The general-expression slice adds **`boolean`** (aliases
`bool`) as the first non-integer scalar — but **expression-only**: it is the result type of
comparisons and the logical connectives and the type of the `TRUE`/`FALSE` literals, yet it
is **not a storable column type** (`storable = false` in
[../types/scalars.toml](../types/scalars.toml)). You cannot declare `CREATE TABLE t(b
boolean)` or `CAST(x AS boolean)`; both trap `0A000` (feature_not_supported) — a deliberate
narrowing (§10), relaxable in the storable-boolean follow-on. The remaining scalars
(`decimal`, `text`, `timestamp`/`timestamptz`, `bytea`, `json`/`jsonb`) are still
**deferred**. A direct consequence: the float-formatting, decimal-rounding, NaN/∞-ordering,
and collation decisions in CLAUDE.md §8 still do **not** bind — there are no floats,
decimals, or text yet. Boolean adds real divergence-prone behavior of its own (a render
form beyond `I`/`T`/`R`, and three-valued Kleene connectives — §10) on the *smallest*
possible non-integer surface.

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

This applies uniformly to arithmetic, to literals that don't fit their target column, and
to narrowing casts (§5). For arithmetic the trap boundary is the operator's **result type**,
not int64: `int16 + int16` yields `int16`, so `30000 + 30000` traps `22003` at the int16
range even though the sum fits int64 (the type-faithful boundary — see
[functions.md](functions.md) §7 and the promotion tower in §4). Each core computes in 64-bit
and traps both if the 64-bit operation itself overflows and if the in-range 64-bit result
falls outside the declared result type. `division`/`modulo` by zero is a distinct defined
trap, `22012` (`division_by_zero`), not a wrapped or platform-dependent value.

## 4. Comparison, promotion, three-valued NULL

See [../types/compare.toml](../types/compare.toml).

**Promotion tower.** The three integer types form one ordered family by `rank`:
`int16 (1) < int32 (2) < int64 (3)`. When two integers meet, both promote to the
higher-ranked type (`strategy = "max-rank"`) and are compared there. Widening is always
lossless, so promotion never loses information or traps.

**Comparability.** Only listed `(family, family)` pairs may be compared; everything else
is a type error (`42804`). The one rule is `integer × integer`: the comparison operators
(`= < > <= >=`) accept **integer operands only**. Comparing a boolean — `(a = b) = (c = d)`,
`bool = int` — is a `42804` type error this slice; booleans are produced by comparisons and
consumed by the logical connectives and `WHERE`, but they are not themselves compared with
the comparison operators (a deliberate narrowing, relaxable by adding a `boolean × boolean`
comparability rule later). This table is where cross-family rules (integer ↔ decimal, the
boolean self-comparison) will be added deliberately, rather than falling out of implicit
coercions.

**Three-valued NULL logic** (CLAUDE.md §4). Any comparison with a NULL operand is
`UNKNOWN`, never TRUE/FALSE. Notably `NULL = NULL` is `UNKNOWN`: equality is **not**
reflexive across NULL. With the `boolean` type, `UNKNOWN` has a concrete carrier — a **NULL
boolean** — so `{true, false, NULL}` *is* the three-valued domain; there is no separate
non-storable "truth" value (functions.md §2). Testing for NULL is done with `IS [NOT] NULL`;
`IS [NOT] DISTINCT FROM` (NULL-safe equality) is now unblocked by boolean but not yet
authored. This is the PG model, borrowed because it is principled. The Kleene truth tables
for the `AND`/`OR`/`NOT` connectives over this domain are in §10.

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

- **Lexing first.** A literal is an unsigned magnitude of digits (the sign is the
  unary-minus operator); a magnitude beyond `2^63` is a *syntax* error (`42601`,
  [../grammar/grammar.ebnf](../grammar/grammar.ebnf) §4), decided before any typing applies.
  The value `2^63` is representable only as the operand of unary minus (folding to
  `int64`'s minimum); a bare `2^63` fits no type and traps `22003` at resolve time.
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

- **Arithmetic context** (`a <op> lit`): an untyped literal operand adapts to the *other*
  operand's type — `small + 1000` types the literal `1000` as `int16` and traps `22003` at
  resolve if it does not fit (here it does not: `1000` fits, but `small + 100000` traps).
  A literal meeting only literals (`1000 + 1`) has no column context, so both default to
  `int64`. The result type is then the promotion of the operand types (§3, §4), and a
  *computed* result outside that type still traps `22003` at run time (`30000 + 30000` over
  `int16`). The unary-minus fold (`-lit`) is one negative literal, range-checked against its
  context like any literal.

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
- ✅ Trap (deterministic error `22003`/`22012`) instead of platform-dependent wraparound or
  undefined divide-by-zero.
- ✅ Promotion is total and order-independent (`max-rank`); arithmetic result types and the
  trap boundary are fixed (§3, functions.md §7).
- ✅ Value order == key byte order (no separate, possibly-divergent comparator).
- ✅ NULL's physical total-order position — ratified NULLs-first (ascending), see §4.
- ✅ Boolean renders as a fixed canonical form (`true`/`false`, NULL as `NULL`) — see §10 and
  [conformance.md](conformance.md); no host-dependent boolean spelling may leak.
- ✅ Kleene `AND`/`OR`/`NOT` truth tables are fixed data (§10), identical across cores.

## 9. The boolean type and three-valued connectives

`boolean` (aliases `bool`) is the first non-integer scalar, **expression-only** this slice
(§1): a column cannot be declared boolean and `CAST(x AS boolean)` is rejected, both with
`0A000`. It exists so the value-world and the truth-world unify — a comparison *produces* a
boolean, so `SELECT a = b` projects one and `WHERE <expr>` consumes one (keeping a row iff
the expression is TRUE; FALSE and NULL/unknown both exclude). The domain is `{false, true}`
plus NULL (= unknown), ordered `false < true` (the `bool-byte` encoding, fixed now but
unexercised until storable — scalars.toml).

**Rendering.** A boolean renders in the conformance corpus as the literal text `true` or
`false`, and a NULL boolean as `NULL`, under a new render tag `B`
([conformance.md](conformance.md)). This is a CLAUDE.md §8 decision: every core must emit the
identical spelling (not `t`/`f`, not `0`/`1`, not host `True`/`true` casing), or the corpus
diverges.

**Three-valued (Kleene) connectives.** `AND`, `OR`, `NOT` operate over `{true, false,
NULL}`. The tables are canonical and identical across cores (functions.md §3 records only
that `and`/`or` are `kleene` and `not` is `propagates`; the tables themselves are here):

| `AND` | true | false | NULL |   | `OR` | true | false | NULL |   | `NOT` |
|---|---|---|---|---|---|---|---|---|---|---|
| **true**  | true  | false | NULL  |   | **true**  | true | true  | true |   | true → false |
| **false** | false | false | false |   | **false** | true | false | NULL |   | false → true |
| **NULL**  | NULL  | false | NULL  |   | **NULL**  | true | NULL  | NULL |   | NULL → NULL |

The non-propagating cells are the point: a *dominant* operand absorbs NULL — `false AND
NULL = false`, `true OR NULL = true` — so `AND`/`OR` are `kleene`, not plain propagation.
`NOT NULL = NULL` is genuine propagation. These follow from `<=`/`>=` being Kleene-OR of
`<` and `=` (functions.md §5) and keep `WHERE`'s "row kept iff TRUE" consistent.

## 10. Open / deferred

- **NULL sort position** — ✅ ratified NULLs-first (ascending) with the key-encoding spec
  (§4, [encoding.md §4](encoding.md)). No longer open.
- **Operator result types** — ✅ authored in [../functions/](../functions/): comparisons and
  connectives yield `boolean`, arithmetic yields the promoted operand type (functions.md §7).
- **Storable boolean** — boolean as a column type (on-disk type code, key/value encoding
  fixtures, boolean PK). Deferred to a follow-on slice; the `bool-byte` encoding rule is
  fixed now (scalars.toml) but unexercised.
- **`boolean × boolean` comparability** and **`IS [NOT] DISTINCT FROM`** — both unblocked by
  the boolean type, not yet authored (§4).
- **`assignment`-mode casts** — vocabulary reserved; first used by non-integer types.
- **Everything else non-integer** — the rest of the scalar set, per CLAUDE.md §4.
