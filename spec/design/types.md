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

## 1. Scope: the storable scalar set

The storable scalar types are three signed integers (CLAUDE.md §4) plus `text`, `boolean`,
`decimal`, `bytea`, `uuid`, the temporal types `timestamp`/`timestamptz`/`interval`, and the
binary floats `f32`/`f64`:

| Canonical id | Aliases | Bits | Range |
|---|---|---|---|
| `i16` | `smallint` | 16 | −32768 … 32767 |
| `i32` | `int`, `integer` | 32 | −2147483648 … 2147483647 |
| `i64` | `bigint` | 64 | −9223372036854775808 … 9223372036854775807 |
| `text` | `varchar`, `character varying`, `string` | — | variable-width UTF-8 (collation `C`) |
| `boolean` | `bool` | — | `{false, true}`, ordered false `<` true |
| `decimal` | `numeric`, `dec` | — | exact base-10 (`numeric(p,s)`, `1≤p≤1000`, `0≤s≤p`) |
| `bytea` | — | — | variable-width raw bytes (unsigned byte order) |
| `uuid` | — | 128 | fixed 16-byte value, RFC 4122 (unsigned byte order) |
| `timestamp` | `timestamp without time zone` | 64 | zoneless wall clock, i64 microseconds ([timestamp.md](timestamp.md)) |
| `timestamptz` | `timestamp with time zone` | 64 | UTC instant, i64 microseconds ([timestamp.md](timestamp.md)) |
| `interval` | — | — | span of months/days/micros, 128-bit canonical ([interval.md](interval.md)) |
| `f32` | `real` | 32 | IEEE 754 binary32 ([float.md](float.md)) |
| `f64` | `double precision` | 64 | IEEE 754 binary64 ([float.md](float.md)) |

The integers are signed, two's-complement. **`text`** is the first storable non-integer
scalar — a variable-width UTF-8 string with one defined collation, `C` (byte / code-point
order); see §11 for the collation decision and its deferred features. **`boolean`** (aliases
`bool`) is the second storable non-integer scalar: it is the result type of comparisons and the
logical connectives and the type of the `TRUE`/`FALSE` literals, and is now **storable** as a
column (`storable = true` in [../types/scalars.toml](../types/scalars.toml)) — `CREATE TABLE
t(flag boolean)`, INSERT/store/retrieve, `boolean × boolean` comparison and `ORDER BY` all work
(§9); a boolean **PRIMARY KEY**/index is **supported** — its fixed-width `bool-byte` key encoding
is exercised (§9, [encoding.md §2.9](encoding.md)), making boolean the second non-integer key type
after uuid — while `CAST(x AS boolean)` and boolean⇄integer casts stay deferred `0A000` (§9, §10).
**`decimal`** (aliases `numeric`, `dec`)
is the third storable non-integer scalar — an exact base-10 numeric (§12,
[decimal.md](decimal.md)); its landing **binds the decimal-rounding decision** of CLAUDE.md §8
(settled: round **half away from zero**) and keeps binary floats out of the compare/text paths
entirely. **`bytea`** (§13) is the fourth storable non-integer scalar — a variable-width binary
string (raw bytes), compared by unsigned byte order. **`uuid`** (§14) is the fifth — a fixed
16-byte value (RFC 4122), compared by unsigned byte order, and the **first non-integer type
usable as a `PRIMARY KEY`** (its fixed-width key encoding is exercised, lifting the key narrowing
the other non-integer types still defer). The temporal types
(`timestamp`/`timestamptz`/`interval`) and the binary floats (`f32`/`f64`) have since
landed, each with its own design doc ([timestamp.md](timestamp.md), [interval.md](interval.md),
[float.md](float.md)); `boolean`, `timestamp`, and `timestamptz` join `uuid` as non-integer
`PRIMARY KEY` types, while `interval`/`f32`/`f64` stay non-key for now. The remaining scalars (`json`/`jsonb`,
and the composite `array` container) are still **deferred**. The float-formatting and NaN/∞
decisions of CLAUDE.md §8 are now **settled** by the landed floats ([float.md](float.md)): they
keep their own PG total order and the `R` render tag (ledgered in the determinism exceptions),
and stay off the *decimal* compare/text path (decimal is finite and exact, never NaN/∞ — §12).
The **collation** decision (§8) is settled in §11: one collation, `C`. Boolean, text, decimal,
bytea, and uuid each add real divergence-prone behavior (a render form beyond `I`, three-valued
Kleene connectives — §10; UTF-8 vs. UTF-16 ordering — §11; exact base-10 arithmetic +
display-scale — §12; a hex literal/render form — §13; and PG-flexible uuid input + a fixed-width
non-integer key — §14) on the smallest possible surfaces.

## 2. Canonical names vs. aliases

Each type has one **canonical id** (`i16`/`i32`/`i64`) plus accepted SQL aliases. The
canonical id is the single name that appears in error messages, the catalog, and the
conformance corpus's `query` column-type tags. Why one canonical name: determinism
(CLAUDE.md §10). If two implementations could each pick a different spelling — `smallint`
vs `i16` — in output, the conformance corpus would spuriously diverge. Aliases are an
input convenience only; they normalize to the canonical id immediately at parse time.

We name the canonical types by their width **in bits** under the **`i`/`f` prefix**
(`i16`/`i32`/`i64`, `f32`/`f64`) — the convention common across programming languages
(Rust `i32`, Go/Zig `i64`, `f64`) — rather than PostgreSQL's byte-count spellings
(`int2`/`int4`/`int8`, `float4`/`float8`). Accepted **aliases** are the SQL-standard words
(`smallint`, `integer`/`int`, `bigint`; `real`, `double precision`, `float`) **and**
PostgreSQL's byte-shorthand (`int2`/`int4`/`int8`, `float4`/`float8`), so both ordinary SQL
(`CREATE TABLE t (x smallint)`) and pasted PG DDL (`x int8`) work.

The **`i`/`f` prefix is load-bearing**, not cosmetic. It makes jed's bit-namespace
(`i8`…`i64`) **lexically disjoint** from PostgreSQL's byte-namespace (`int2`…`int8`): the
two `8`s can never collide, because one is spelled `i8` and the other `int8`. That is what
lets jed accept the full PG byte-shorthand *and* keep the door open for a future 8-bit type:

- `int8` → `i64` (PG's bigint shorthand), unambiguously, while
- `i8` stays **free** for a future 8-bit integer.

Had we kept PostgreSQL's own `int`-prefix for the bit-names (`int16`/`int32`/`int64`), the
shared prefix would have forced a choice between accepting `int8` (= 64-bit, foreclosing an
8-bit type) and rejecting it (losing PG compatibility) — the classic `int8` ambiguity. The
prefix dissolves it. This also extends cleanly to future **range types**: PG's `int8range`
becomes an atomic alias for the canonical `i64range` with no collision against a future
`i8range`. The canonical choice is arbitrary-but-fixed; what matters is that it is fixed.

The old jed names `int16`/`int32`/`int64`/`float32`/`float64` are a **clean break** — no
longer accepted (an unknown type, 42704), replaced wholesale by the `i`/`f` prefix.

## 3. Integer overflow: trap, never wrap

When an operation would produce a value outside a type's range, the engine **traps** —
raises `22003` (`numeric_value_out_of_range`,
[../errors/registry.toml](../errors/registry.toml)) — rather than wrapping.

CLAUDE.md §8 left this as "defined wrap vs. trap." We choose **trap** because silent
wraparound is exactly the runtime reinterpretation a strict static type system exists to
prevent (CLAUDE.md §4): `i16` holding `32767` plus `1` must not become `−32768`. Trap is
also PostgreSQL's behavior, which §1 lets us borrow where principled. Wrap is the rejected
alternative; if a wrapping operation is ever wanted it will be a *distinct, explicitly
named* operator, not the default `+`.

This applies uniformly to arithmetic, to literals that don't fit their target column, and
to narrowing casts (§5). For arithmetic the trap boundary is the operator's **result type**,
not i64: `i16 + i16` yields `i16`, so `30000 + 30000` traps `22003` at the i16
range even though the sum fits i64 (the type-faithful boundary — see
[functions.md](functions.md) §7 and the promotion tower in §4). Each core computes in 64-bit
and traps both if the 64-bit operation itself overflows and if the in-range 64-bit result
falls outside the declared result type. `division`/`modulo` by zero is a distinct defined
trap, `22012` (`division_by_zero`), not a wrapped or platform-dependent value.

One subtlety at the negative boundary: dividing the most-negative value by `-1`
(`int64_min / -1`) **traps `22003`** — the true quotient is `-int64_min`, which has no
positive counterpart in the type (the same overflow as negating `int64_min`). But the
**modulo** counterpart `x % -1` is **`0` for every `x`** (the remainder is mathematically
zero), so it **never traps** — even at `int64_min % -1`, where a naive 64-bit `IDIV` would
fault. Each core special-cases divisor `-1` in modulo to yield `0`, matching PostgreSQL and
keeping the three integer widths consistent (the `i16`/`i32` cases already compute `0`
cleanly when widened to 64-bit).

## 4. Comparison, promotion, three-valued NULL

See [../types/compare.toml](../types/compare.toml).

**Promotion tower.** The three integer types form one ordered family by `rank`:
`i16 (1) < i32 (2) < i64 (3)`. When two integers meet, both promote to the
higher-ranked type (`strategy = "max-rank"`) and are compared there. Widening is always
lossless, so promotion never loses information or traps.

**Comparability.** Only listed `(family, family)` pairs may be compared; everything else
is a type error (`42804`). There are three rules: `integer × integer` (`via = "promote"` —
widen to the common type, then compare), `text × text` (`via = "none"` — both are already text,
compare by the `C` collation; §11), and `boolean × boolean` (`via = "none"` — compare by value,
false `<` true; §9). The comparison operators (`= < > <= >=` and `IS [NOT] DISTINCT FROM`) are
**overloaded** across these families (one catalog row per signature —
[../functions/catalog.toml](../functions/catalog.toml)). A **mixed** pair is a `42804` type
error: `text = int` is not comparable (no such `comparable` pair), exactly as `bool = int` is.
So `(a = b) = (c = d)` (two booleans) now compares fine, but `(a = b) = 1` (boolean vs integer)
is a `42804`. This table is where the remaining cross-family rules (integer ↔ decimal, text ↔
other) will be added deliberately, rather than falling out of implicit coercions.

**Three-valued NULL logic** (CLAUDE.md §4). Any comparison with a NULL operand is
`UNKNOWN`, never TRUE/FALSE. Notably `NULL = NULL` is `UNKNOWN`: equality is **not**
reflexive across NULL. With the `boolean` type, `UNKNOWN` has a concrete carrier — a **NULL
boolean** — so `{true, false, NULL}` *is* the three-valued domain; there is no separate
non-storable "truth" value (functions.md §2). Testing for NULL is done with `IS [NOT]
NULL`; `IS [NOT] DISTINCT FROM` (NULL-safe equality) is **authored** (functions.md §3) —
it treats NULL as a comparable value, so `NULL IS NOT DISTINCT FROM NULL` is TRUE and the
result is always a definite boolean, never UNKNOWN. This is the PG model, borrowed because
it is principled. The Kleene truth tables
for the `AND`/`OR`/`NOT` connectives over this domain are in §10.

**Value ordering & NULL position.** Non-NULL integers use plain signed numeric ascending
order, which is exactly what the key encoding (§7) reproduces in raw bytes. NULL's position
in the physical total order is **ratified** to the PostgreSQL model (it was deferred to the
key-encoding step, first ratified NULL-smallest, then re-ratified here): **NULLs sort last**
(after every present value) in ascending order, via a 1-byte presence tag on a nullable key
slot (`0x00` present `<` `0x01` NULL); descending inverts this (NULLs first). See
[encoding.md §4](encoding.md) and `null_ordering` in
[../types/compare.toml](../types/compare.toml). The SQL-level `ORDER BY ... NULLS
FIRST|LAST` override **layers on top of** this physical order (grammar.md §10): with no
explicit clause a key's default NULL placement *follows the physical order* — `ASC` → NULLs
last, `DESC` → NULLs first — so a plain `ORDER BY col` mirrors index-iteration order. Because
NULL is the largest value, this is the **PostgreSQL** model (PG defaults `ASC` to NULLs last)
and a deliberate **divergence from SQLite** (where NULL is the smallest, so SQLite defaults
`ASC` to NULLs first); an explicit `NULLS FIRST|LAST` overrides the default regardless of
direction.

**Set-operation column unification.** A set operation (`UNION`/`INTERSECT`/`EXCEPT` —
grammar.md §25) must reconcile each output column's type across its operands into one result
type. This is the **result-type** analogue of the comparability matrix above (and of `CASE`'s
arm unification), folded over all operands of a column position: two integer types give the
**max-rank** integer (`promote`); integer with `decimal` gives `decimal` (`promote-to-decimal`,
the integer's values converted scale-0 before row matching); a `NULL` type takes the other
operand's type; a column that is `NULL`-typed in **every** operand resolves to **`text`** (the
PostgreSQL unknown-literal rule); a same-family non-integer pair (`text`/`boolean`/`bytea`/
`uuid`/`timestamp`/`timestamptz`) gives that type; anything else is `42804`. The *set* of
unifiable pairs is exactly the `comparable` matrix in [../types/compare.toml](../types/compare.toml)
(plus the all-NULL→`text` rule), so unification never admits a pairing the engine could not also
compare. The full contract — value conversion, per-value display scale, NULL-safe row identity —
is in grammar.md §25 and [cost.md](cost.md) §3.

## 5. Coercion / casts

See [../types/casts.toml](../types/casts.toml). The matrix is **strict**: any `(from, to)`
not listed is forbidden. Identity casts are implicit and always succeed (implied, not
listed).

- **Widening** (`i16→i32`, `i16→i64`, `i32→i64`) is lossless, so it is
  **implicit** — inserted automatically wherever a wider integer is wanted.
- **Narrowing** (`i64→i32`, `i64→i16`, `i32→i16`) is lossy in general, so it
  requires an **explicit** `CAST(...)` and **traps** (`22003`) when the value does not fit.
- **Text casts** split by operand. A **string LITERAL** coerces to a named type — the
  `type 'string'` typed literal and `CAST(<string literal> AS T)` ([grammar.md](grammar.md) §36) —
  for every scalar `T`, folded at resolve (`INTEGER '42'`, `NUMERIC '1.5'`, `BOOLEAN 'true'`,
  `CAST('42' AS int)`). The coercion is the type's own parse (the §3 datetime parse, the §13/§14
  bytea/uuid input, a decimal/integer/boolean parse matching jed's literal grammar), trapping
  `22P02` (malformed) / `22003` (out of range) / `42704` (unknown type name). A **runtime** text→`T`
  cast on a *non-literal* text expression (`CAST(text_col AS int)`) stays **deferred** (`0A000`) —
  the general string-function slice (§11). `CAST(1 AS text)` (casting *to* text) is likewise
  deferred. The `text → text` identity is implicit, like any identity cast.
- **Strictness is preserved.** The string→number/bool coercion fires only when the type is
  **named** (a literal or CAST). A **bare** string in a numeric context does **not** silently
  become a number: `WHERE int_col = '42'` is `42804` (§4), and a bare string adapts *only* to the
  string-native types (bytea/uuid/timestamp/timestamptz/interval, where the string is the only
  literal form) — never to int/decimal/boolean. So `type 'string'` admits the *explicit* spelling
  of a text→scalar cast without weakening the implicit rules.

Cast modes are `implicit` / `assignment` / `explicit`. `assignment` (allowed on
INSERT/UPDATE into a column but not in general expressions) is part of the vocabulary for
future types; for the integer set, widenings are fully implicit so no edge needs the
weaker `assignment` mode yet.

**Two spellings, one cast.** An explicit cast is written either `CAST(expr AS type)` or, with
PostgreSQL's postfix operator, `expr :: type` ([grammar.md](grammar.md) §37). The two are
identical — the parsers desugar `::` to the same `CAST` node — so the matrix above, the
string-literal coercion, the deferred narrowings, and every resolve code apply unchanged to both.
`::` binds tighter than unary minus (so `-5 :: int` is `-(5 :: int)`), and a bind-parameter operand
takes the cast target as its type (`$1 :: int` types `$1` as int — [api.md](api.md) §5).

## 6. Integer-literal typing

A bare integer literal (`1000`, `-32768`) is an **untyped integer constant** — in the
spirit of Go's and Rust's untyped constants. It has no intrinsic type; it acquires one from
its **context**, and **traps `22003`** if its value does not fit that context type. This
keeps a literal from silently forcing a width, and makes `WHERE small = 100000` (where
`small` is `i16`) a type error rather than a value that silently never matches.

- **Lexing first.** A literal is an unsigned magnitude of digits (the sign is the
  unary-minus operator); a magnitude beyond `2^63` is a *syntax* error (`42601`,
  [../grammar/grammar.ebnf](../grammar/grammar.ebnf) §4), decided before any typing applies.
  The value `2^63` is representable only as the operand of unary minus (folding to
  `i64`'s minimum); a bare `2^63` fits no type and traps `22003` at resolve time.
- **Assignment context** (`INSERT ... VALUES`, `UPDATE ... SET col = lit`): the context is
  the target column's type. The literal adapts to it — accepted iff in range, else `22003`.
- **Cast context** (`CAST(lit AS T)`): the context is `T`; range-checked, else `22003`.
- **Comparison context** (`col <op> lit`): the context is the column's type. The literal
  adapts to it; if out of range the predicate **traps `22003` deterministically, before any
  row is scanned** — a literal that cannot be represented in the column's type is a type
  error, not a silent non-match, for every operator (`=`, `<`, `>`, `<=`, `>=`). In range,
  the comparison proceeds within that type (no promotion needed). A `NULL` literal is exempt
  (it is the absence of a value).
- **No context** (a bare projected literal, `SELECT 1000`): defaults to **i64**, the
  widest integer.

- **Arithmetic context** (`a <op> lit`): an untyped literal operand adapts to the *other*
  operand's type — `small + 1000` types the literal `1000` as `i16` and traps `22003` at
  resolve if it does not fit (here it does not: `1000` fits, but `small + 100000` traps).
  A literal meeting only literals (`1000 + 1`) has no column context, so both default to
  `i64`. The result type is then the promotion of the operand types (§3, §4), and a
  *computed* result outside that type still traps `22003` at run time (`30000 + 30000` over
  `i16`). The unary-minus fold (`-lit`) is one negative literal, range-checked against its
  context like any literal.

**Why this, not "smallest fitting" or "always i64".** Smallest-fitting makes ordinary
arithmetic overflow surprisingly (`30000 + 30000` would be `i16 + i16`). Always-i64
removes the type error in a comparison (an out-of-range literal would silently never match).
Context-adaptation gives each literal exactly the type its use demands and surfaces an
impossible literal as a `22003` the moment it is resolved — consistent with the strict cast
matrix (§5: adaptation is a value-checked coercion, never a silent reinterpret), the
promotion tower (§4: once typed, a literal participates like any value), and trap-on-overflow
(§3).

**Deliberate PostgreSQL divergence (the no-context default).** PostgreSQL does *not* treat a
bare integer constant as untyped: it assigns the **smallest fitting** type at parse time —
`int4` if it fits, else `int8`, else `numeric` — independent of context. jed's untyped-constant
model adapts to context instead, and where there is no context it defaults to **`i64`**, *not*
PG's smallest-fitting `int4`. Two observable consequences, both intentional: (a) a context-free
integer literal — including the elements of a bare `ARRAY[…]` constructor, which is a no-context
position — is `i64`, so `ARRAY[1,2,3]` is `i64[]` where PG infers `int4[]`
([array-functions.md §2/§5 #8](array-functions.md)); (b) literal-only arithmetic is more
permissive than PG — `2000000000 + 2000000000` computes to `i64` `4000000000`, where PG
overflows `int4` (`22003`). This is the one place jed's literal typing diverges from PG by
default (CLAUDE.md §1/§8); the strict comparison/assignment behavior above matches the *intent*
of a strict type system and is stricter than PG (PG silently returns no rows for
`int2_col = 100000`, jed raises `22003`).

**String literals adapt to a `bytea` context (the same principle).** A single-quoted literal
is a `text` value by default (it has the one collation `C`; unlike an integer literal it has no
width to choose among — §11). But once `bytea` exists (§13) a *string* literal, like an integer
literal, has a context it can adapt to: in a **bytea** context — `INSERT`/`UPDATE` into a bytea
column, or a comparison against a bytea column (`WHERE b = '\xab'`) — the string literal is
read as a **bytea** value via the bytea hex input form (`\x` + an even count of hex digits),
exactly as PostgreSQL applies bytea's input function to a string constant in a bytea context.
An
ill-formed hex literal in a bytea context (no `\x` prefix, odd digit count, a non-hex
character) is a **`22P02`** (`invalid_text_representation`) raised **deterministically at
resolve time, before any row is scanned** — the precise analogue of the `22003` an out-of-range
integer literal raises in a comparison context. A string literal in a *non-bytea* context is
never decoded, so `'\xZZ'` compared with a `text` column is the ordinary 4-character text value
`\xZZ`, never a `22P02`. The decode is a value-checked coercion at resolve time, never a silent
reinterpretation — the same discipline as the integer rule above.

A string literal adapts to a **`uuid`** context the same way (§14): in a uuid context —
`INSERT`/`UPDATE` into a uuid column, or a comparison against one (`WHERE id = '550e8400-…'`)
— the string is read as a **uuid** value via uuid's input function (PostgreSQL-flexible —
optional surrounding `{}`, hyphens optional/at any position, or hyphen-less 32-hex, any case),
trapping `22P02` at resolve time on malformed input. With no uuid context the string stays
`text`. So `bytea` and `uuid` are the two types a single-quoted literal adapts to; the decode is
a value-checked coercion either way.

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
- ✅ NULL's physical total-order position — ratified NULLs-last (ascending, the PostgreSQL
  model), see §4.
- ✅ Boolean renders as a fixed canonical form (`true`/`false`, NULL as `NULL`) — see §10 and
  [conformance.md](conformance.md); no host-dependent boolean spelling may leak.
- ✅ Kleene `AND`/`OR`/`NOT` truth tables are fixed data (§10), identical across cores.
- ✅ Text orders by the `C` collation — `memcmp` over UTF-8 = code-point order — identical
  across cores; the TS UTF-16-vs-UTF-8 ordering trap is avoided by comparing encoded bytes (§11).

## 9. The boolean type, its storage, and three-valued connectives

`boolean` (aliases `bool`) is the truth type: a comparison *produces* a boolean, so
`SELECT a = b` projects one and `WHERE <expr>` consumes one (keeping a row iff the expression
is TRUE; FALSE and NULL/unknown both exclude). The domain is `{false, true}` plus NULL
(= unknown), ordered `false < true`.

**Storage.** boolean is now a **storable** column type (`storable = true`): `CREATE TABLE
t(flag boolean)`, INSERT/store/retrieve of `false`/`true`/`NULL`, `boolean × boolean`
comparison (`= < > <= >=`, `IS [NOT] DISTINCT FROM` — §4), and `ORDER BY` (false `<` true,
NULLs last — the PostgreSQL model) all work. A stored boolean uses the value codec's 1-byte
`bool-byte` body (`0x00` false, `0x01` true) behind the shared presence tag (on-disk type code
`5` — [../fileformat/format.md](../fileformat/format.md)); the same order-preserving `bool-byte`
is the key encoding rule (scalars.toml), false sorting below true.

**boolean PRIMARY KEY / index — supported.** boolean is the **second non-integer key type**
(after uuid): a `boolean PRIMARY KEY`, a boolean member of a composite key, and a secondary
index on a boolean column all work. The stored key is the bare `bool-byte` (`0x00` false `<`
`0x01` true — a PK is NOT NULL, so no presence tag; an index slot tags it per
[encoding.md §2.9](encoding.md)/§2.2). Like uuid, boolean is fixed-width (1 byte), so its key is
self-delimiting with no escape/terminator; the executor key path that already generalized to uuid
extends to boolean unchanged, and the bytes are pinned by the `bool_pk_table.jed` golden and the
`encoding/integers.toml` boolean vectors. (A boolean key admits at most two distinct rows, so it is
rarely a *useful* PK, but it is well-defined and supported — strictness over special-casing.) One
narrowing remains, relaxable and mirroring text:

- **boolean casts** — `CAST(x AS boolean)` and boolean⇄integer casts are rejected `0A000` /
  `42804` (not in the cast matrix — §5, [../types/casts.toml](../types/casts.toml)). PostgreSQL's
  boolean↔integer casts are asymmetric, so they are authored deliberately in a later cast slice
  rather than falling out of making boolean storable.

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

- **NULL sort position** — ✅ ratified NULLs-last (ascending, the PostgreSQL model) — see
  the key-encoding spec (§4, [encoding.md §4](encoding.md)). No longer open.
- **Operator result types** — ✅ authored in [../functions/](../functions/): comparisons and
  connectives yield `boolean`, arithmetic yields the promoted operand type (functions.md §7).
- **Storable boolean** — ✅ landed (§9): boolean is a column type with on-disk type code `5`,
  the `bool-byte` value codec, a golden round-trip fixture (`bool_table.jed`), and
  `boolean × boolean` comparison + `ORDER BY`. **boolean in a key / PRIMARY KEY** — ✅ has since
  landed (§9): the `bool-byte` key encoding is exercised (the second non-integer key after uuid),
  with boolean key byte-fixtures (`encoding/integers.toml`) and the `bool_pk_table.jed` golden.
  One sub-feature remains deferred: **boolean⇄integer casts** (rejected; PG's are asymmetric, so a
  dedicated cast slice — §5, casts.toml).
- **`IS [NOT] DISTINCT FROM`** — ✅ authored (NULL-safe equality; functions.md §3), now
  overloaded over the integer, text, and boolean families (§4).
- **`boolean × boolean` comparability** — ✅ landed (§4, §9): comparing two booleans
  (`(a = b) = (c = d)`) is now allowed; a boolean vs a non-boolean is still `42804`.
- **`assignment`-mode casts** — vocabulary reserved; first used by non-integer types.
- **`text`** — ✅ landed as the first storable non-integer scalar, with one collation `C`
  (§11). Its deferred sub-features (`varchar(n)` length limits, text⇄other casts, string
  functions / `||` / `LIKE`, text in keys, and locale/ICU multi-collation) are enumerated in §11.
- **`decimal`** — ✅ landed (§12, [decimal.md](decimal.md)): exact base-10 numeric, the first
  parameterized type (`numeric(p,s)`), rounding half-away (settling the §8 decimal-rounding
  hotspot), comparison + casts + storage + arithmetic. The original 1000-digit absolute cap
  has been **lifted to PostgreSQL's numeric-format limits** (131072 integer / 16383 fractional
  digits — [decimal.md](decimal.md) §2) now that over-page values land via
  [large-values.md](large-values.md), with the size-scaled `decimal_work` cost unit bounding
  big-value arithmetic ([cost.md](cost.md) §3). Scientific `e`-notation literals have since
  landed ([decimal.md](decimal.md) §6). Deferred sub-features: decimal in a key (`0A000`; the
  order-preserving encoding is authored, [encoding.md](encoding.md) §2.5) and
  negative/over-precision scale typmods.
- **`bytea`** — ✅ landed as the fourth storable non-integer scalar — variable-width raw bytes,
  unsigned byte-order comparison (§13). Its deferred sub-features (the traditional escape input
  format, bytea⇄other casts, binary functions, and bytea in keys) are enumerated in §13.
- **`uuid`** — ✅ landed as the fifth storable non-integer scalar (§14) — a fixed 16-byte value,
  unsigned byte-order comparison, PostgreSQL-flexible input + canonical `8-4-4-4-12` lowercase
  output, and the **first non-integer type usable as a `PRIMARY KEY`** (its `uuid-raw16` key
  encoding is exercised, [encoding.md §2.7](encoding.md)). Deferred sub-features (uuid⇄other casts,
  uuid functions like `gen_random_uuid()`) are enumerated in §14.
- **`timestamp` / `timestamptz`** — ✅ landed ([timestamp.md](timestamp.md)): the instant
  model (i64 microseconds), no time-zone database, infinity sentinels, and usable as a
  `PRIMARY KEY` (key encoding = the i64 rule).
- **`interval`** — ✅ landed ([interval.md](interval.md)): a months/days/micros span with
  PostgreSQL arithmetic; non-key only (`0A000`).
- **`f32` / `f64`** — ✅ landed ([float.md](float.md)): IEEE 754 binary32/binary64,
  the PostgreSQL total order, a trapping arithmetic kernel, and the `R` render tag exempting
  computed/rendered values from cross-core byte-identity (settling the CLAUDE.md §8 float
  hotspots); non-key only (`0A000`).
- **Everything else non-integer** — the rest of the scalar set, per CLAUDE.md §4
  (`json`/`jsonb`).
- **Composite `array` type** — a *container* over the scalar set, a separate later type
  axis rather than another scalar (CLAUDE.md §4): its own value codec, order-preserving key
  encoding, element-type and `NULL`-element rules, and equality/ordering. Deferred; match
  PostgreSQL array semantics by default (§1).

## 11. The text type and its collation

`text` (aliases `varchar`, `character varying`, `string`) is a **variable-width UTF-8 string**
and the first storable non-integer scalar. The empty string `''` is a distinct, non-NULL value
(a zero-length string), separate from `NULL`.

**One collation: `C` (byte / code-point order over UTF-8).** A *collation* is the rule for
ordering and equating text, layered on the *encoding* (which maps characters to bytes — the
engine commits to UTF-8 everywhere). CLAUDE.md §8 calls for **one** defined collation to start,
"byte/codepoint order is simplest," with ICU/locale collation an explicit later feature. We
adopt **`C`**: compare the raw UTF-8 bytes lexicographically (`memcmp`). This is the one place
where the PostgreSQL-default rule (§1/CLAUDE.md §1) and the determinism rule (CLAUDE.md §8/§10)
point the same way:

- It **is** PostgreSQL's `C`/`POSIX` collation (and SQLite's default `BINARY`), so "match PG"
  is satisfied with no tension.
- It needs **zero data tables** and is a fixed algorithm — identical on every platform, every
  core, every version, forever. Nothing must be embedded in the database file for ordering to
  be stable. (This is *why* `C` is the right starting collation for a no-reference-implementation,
  byte-exact, multi-core engine — CLAUDE.md §2/§8.)
- For UTF-8, byte order **equals Unicode code-point order** (a UTF-8 design property), so the
  comparator and the order-preserving key encoding (encoding.md §2.4) are order-preserving for
  free.

The price is that `C` is not "human": `'B' < 'a'` (`0x42 < 0x61`), digits sort before letters,
and accented/non-ASCII characters sort by code point, after all ASCII — exactly PostgreSQL `C`'s
behavior, and documented as such.

**Cross-core determinism trap (load-bearing).** Comparing text is *not* as trivially identical
across cores as comparing integers. Rust (`str` `Ord`) and Go (`string` `<`) compare by **bytes**
— correct. But JavaScript/TypeScript `<` and `localeCompare` compare by **UTF-16 code units**,
which **disagrees with UTF-8 / code-point order for any character above U+FFFF** (e.g. `😀`
U+1F600 sorts before U+E000–U+FFFF in UTF-16 but after them by code point). So the TS core MUST
compare encoded UTF-8 bytes (or iterate code points) — never the raw JS string. This is pinned
by a conformance case containing an astral character (CLAUDE.md §8).

**Why not locale/linguistic collation (ICU/CLDR) now.** Locale collations (`en_US`, `de_DE`,
case/accent folding, language tailoring) are linguistically correct but (a) require large data
tables and (b) **vary by library version** — an ICU or glibc upgrade can reorder the same
strings, the well-known cause of silent index corruption in PostgreSQL after an OS upgrade. For
this engine that is doubly disqualifying: relying on each host's ICU/glibc would make the
several cores (Rust, Go, TS, …) disagree byte-for-byte, violating cross-core identity (CLAUDE.md
§8). A linguistic collation here would therefore have to **vendor and version-pin** the UCA/CLDR
tables as shared spec data (CLAUDE.md §5) — a large, deliberate later feature, exactly as §8
files it.

**Deferred text sub-features** (relaxable narrowings, each its own follow-up):

- **Text in a key / `PRIMARY KEY`** — rejected `0A000` this slice; the order-preserving key
  rule is authored (encoding.md §2.4) and the executor path + byte fixtures land when lifted.
- **`varchar(n)` length limits** — `varchar`/`character varying`/`string` are accepted as
  aliases for **unbounded** `text`; a length parameter `varchar(n)` is not supported yet
  (rejected `0A000`). When added, an over-length value traps `22001` (string_data_right_truncation).
- **Text ⇄ other casts** (§5), **string functions** (`length`, `lower`/`upper`, `substring`),
  **concatenation `||`**, and **`LIKE`** — separate slices; this slice is comparison + storage
  only (`= < > <= >=`, `IS [NOT] DISTINCT FROM`).
- **Multi-collation / ICU** — a second collation, a per-column collation field in the catalog
  (the on-disk format reserves room for it — format.md), and `COLLATE` clauses.

**Practical size note.** A text value is unbounded by type, but a single stored value (or row)
larger than one page trips the existing whole-image `feature_not_supported` (`0A000`) narrowing
(format.md "Oversized item") — with integers that was unreachable; with text it becomes a real,
documented limit until overflow pages land ([large-values.md](large-values.md)).

## 12. The decimal type

`decimal` (aliases `numeric`, `dec`) is the exact base-10 numeric and the headline of the type
system (CLAUDE.md §4) — the full reasoning and the precise arithmetic are in
[decimal.md](decimal.md); this section records the type-system-level facts and the §8 decisions
it settles.

- **Exact, base-10, finite.** A value is `(sign, base-10⁹ coefficient, scale)` =
  `(−1)^sign · coefficient · 10^(−scale)`; no binary float touches it (CLAUDE.md §8). It is
  **always finite** — no NaN/±Infinity (a documented PG divergence: no float source exists and
  `x/0` traps `22012`).
- **The first parameterized type.** `numeric` (unconstrained), `numeric(p)`, and
  `numeric(p,s)` — `1 ≤ p ≤ 1000`, `0 ≤ s ≤ p`; a bad typmod traps `22023`. This adds an
  optional type modifier to the grammar's `type_name` ([grammar.md](grammar.md) §6, §14).
- **Rounding (settles CLAUDE.md §8).** Coercing to a scale rounds **half away from zero** (PG
  `numeric`): `0.125 → 0.13`, `2.5 → 3`. One rounding mode engine-wide (decimal.md §3).
- **Comparison/promotion.** `decimal × decimal` compares by exact value (scale-aligned, so
  `1.5 = 1.50`); `integer × decimal` is the first **cross-family** comparable pair, resolved by
  promoting the integer to decimal ([../types/compare.toml](../types/compare.toml)). Decimal
  forms no integer-style promotion tower (one type; a value carries its scale).
- **Casts (stricter than PG).** `int → decimal` is implicit (lossless); `decimal → int` is
  **explicit CAST only** (rounds half-away, traps `22003`) — jed forbids the silent decimal→int
  narrowing PG allows, consistent with the strict matrix (§5, [../types/casts.toml](../types/casts.toml)).
- **Storage.** On-disk value codec, type code 6 ([../fileformat/format.md](../fileformat/format.md));
  rendered under the new **`D`** conformance tag. A decimal **key** (`PRIMARY KEY`/index) is
  rejected `0A000` this slice — the order-preserving rule is authored in
  [encoding.md](encoding.md) §2.5 but unexercised, the text-PK precedent.

## 13. The bytea type

`bytea` (no aliases) is a **variable-width binary string** — a sequence of raw bytes — and the
fourth storable non-integer scalar. It is *not* text: it carries no collation and no character
encoding, and a value may contain any byte, including embedded `0x00`. The empty byte string is
a distinct, non-NULL value (a zero-length string), separate from `NULL`. This is PostgreSQL's
`bytea`, borrowed because §1 makes PG the default and binary data is a common storable need that
the strict type system should model explicitly rather than smuggle through `text`.

**Comparison and ordering: unsigned byte order (`memcmp`).** Two bytea values compare by their
raw bytes, lexicographically, as **unsigned** bytes — exactly PostgreSQL's `bytea` comparison.
A shorter byte string that is a prefix of a longer one sorts first (`\x61 < \x6161 < \x62`).
`bytea` is its own comparison family: `bytea` vs `text` and `bytea` vs an integer are **not**
comparable — each is a `42804` type error (compare.toml lists only `bytea × bytea`), exactly as
`text` is not comparable with an integer. The ordering operators (`= < > <= >=`) and the
NULL-safe `IS [NOT] DISTINCT FROM` are another of the catalog's comparison operator overloads,
alongside integer, text, boolean, and decimal (catalog.toml).

**Cross-core determinism — simpler than text.** The text type's load-bearing trap is that
JavaScript compares strings by UTF-16 code units, which disagrees with UTF-8 byte order above
U+FFFF (§11), so the TS core must compare encoded bytes. `bytea` has no such trap: it **is** raw
bytes in every core (Rust `Vec<u8>`, Go `[]byte`, TS `Uint8Array`), and a byte-wise unsigned
`memcmp` is natively identical across all three. There are no code points and no encoding to get
wrong.

**Literals: a string in a bytea context, hex input only.** There is no distinct bytea literal
token; a bytea value is written as a single-quoted string literal that **adapts to a bytea
context** (§6) — `INSERT INTO t VALUES (1, '\xdeadbeef')`, `UPDATE t SET b = '\xff'`, and
`WHERE b = '\xab'`. The string is decoded via the **hex input form**: a literal `\x` followed
by an **even** count of hexadecimal digits (case-insensitive), each pair one byte; `'\x'` alone
is the empty byte string. This matches PostgreSQL's hex input and, by being the same form as the
render output (below), round-trips exactly. An ill-formed hex literal in a bytea context (no
`\x` prefix, an odd digit count, or a non-hex character) traps **`22P02`**
(`invalid_text_representation`) deterministically at resolve time, before any row is scanned
(§6). An integer or boolean literal in a bytea context, and a string literal into a non-bytea
column, are `42804` type errors, as for text.

**Rendering: lowercase hex.** A bytea value renders in the conformance corpus as `\x` followed
by the **lowercase** hex of its bytes (the empty value renders as exactly `\x`) — PostgreSQL's
default `bytea_output = hex`. This reuses the `T` render tag (the tag is a *rendering* tag, not a
type assertion — conformance.md §1; a bytea renders as a printable ASCII hex string). Every core
must emit the identical lowercase spelling, or the corpus diverges (a CLAUDE.md §8 decision, like
the boolean `true`/`false` spelling).

**On-disk.** Stable type code `7` (format.md). The stored value uses the same compact value
codec as text — a presence tag, then a `u16` big-endian byte-length followed by that many **raw**
bytes (no UTF-8 validation, the one difference from text's branch) — because a stored value never
needs to sort. The empty value is the tag plus a zero length. A value longer than `0xFFFF` bytes,
like an oversized text value, trips the whole-image oversized-item `0A000` narrowing.

**Deferred bytea sub-features** (relaxable narrowings, each its own follow-up):

- **bytea in a key / `PRIMARY KEY`** — rejected `0A000` this slice, exactly matching the text-PK
  narrowing (§11). The order-preserving key rule (`bytea-terminated-escape`) is authored
  (encoding.md §2.6) and the executor path + byte fixtures land when lifted.
- **The traditional escape input format** (`\047`, `\\`, literal printable bytes) — not accepted;
  the hex form `\x…` is the only input this slice. A deliberate, documented divergence from PG
  (which also accepts the escape format on input), justified by determinism and a smaller surface;
  the hex form is the modern canonical spelling and matches the render output.
- **bytea ⇄ other casts** (§5) and **binary functions** (`length`, `||`, `substring`,
  `encode`/`decode`, `get_byte`/`set_byte`, …) — separate slices; this slice is comparison +
  storage only (`= < > <= >=`, `IS [NOT] DISTINCT FROM`).

**Practical size note.** As for text (§11), a single stored bytea value (or row) larger than one
page trips the whole-image `0A000` oversized-item narrowing until overflow pages land.

## 14. The uuid type

`uuid` (no aliases) is a **fixed 16-byte value** (RFC 4122) and the fifth storable non-integer
scalar. It is *not* text and *not* bytea: it is its own type and family, with a fixed width and a
canonical textual spelling. Like bytea it carries no collation; unlike bytea it is fixed-width
(always exactly 16 bytes). This is PostgreSQL's `uuid`, borrowed because §1 makes PG the default
and a UUID is an extremely common key/identifier the strict type system should model explicitly
(a fixed 16-byte value) rather than smuggle through `text` or `bytea`.

**Comparison and ordering: unsigned byte order (`memcmp`) over the 16 bytes** — exactly
PostgreSQL's `uuid` comparison. Because every value is the same width there is no prefix/length
case. `uuid` is its own comparison family: `uuid` vs `text`, `bytea`, or an integer are **not**
comparable — each is a `42804` type error (compare.toml lists only `uuid × uuid`); a uuid is not
a bytea even when the 16 bytes coincide. The ordering operators (`= < > <= >=`) and the NULL-safe
`IS [NOT] DISTINCT FROM` are another of the catalog's comparison operator overloads, alongside
integer, text, boolean, decimal, and bytea (catalog.toml).

**Cross-core determinism — like bytea, no UTF-16 trap.** A uuid **is** 16 raw bytes in every core
(Rust `[u8; 16]`, Go a 16-byte string, TS `Uint8Array`), and unsigned `memcmp` is natively
identical across all three. The one determinism surface is the **input parser** and the **output
spelling**, both pinned below and in the corpus.

**Literals: a string in a uuid context, PostgreSQL-flexible input.** There is no distinct uuid
literal token; a uuid value is written as a single-quoted string literal that **adapts to a uuid
context** (§6) — `INSERT INTO t VALUES ('550e8400-e29b-41d4-a716-446655440000', NULL)`,
`UPDATE t SET ref = '…'`, and `WHERE id = '…'`. Input replicates **PostgreSQL's `uuid_in`**: an
optional surrounding `{ }`, then the 16 bytes as two hex digits each in **any case**, with an
**optional hyphen permitted after each whole pair of bytes** (every 4 hex digits). So the canonical
`8-4-4-4-12` form, a fully hyphen-less 32-hex run, the every-4-digit grouping
(`550e-8400-…-0000`), and any `{}`-wrapped variant all normalize to the same 16 bytes — but a
hyphen at a *non*-group position (e.g. `5-50e…`) is **rejected**, exactly as PG rejects it (this is
PG's algorithm, not a looser strip-all). (This is a deliberate contrast with bytea, whose
alternative input format we deferred: for uuid, matching PG's lenient `uuid_in` is the §1 default
and the spellings are common in practice.) Malformed input (wrong digit count, a non-hex
character, a misplaced hyphen, an unbalanced brace) traps **`22P02`**
(`invalid_text_representation`) deterministically at resolve time, before any row is scanned (§6).
An integer/boolean literal in a uuid context, and a string literal into a non-uuid column, are
`42804` type errors. A string literal in a *non-uuid* context is never decoded.

**Rendering: canonical lowercase `8-4-4-4-12`.** A uuid renders in the conformance corpus as the
canonical RFC 4122 spelling — five lowercase-hex groups of 8-4-4-4-12 digits joined by hyphens
(e.g. `550e8400-e29b-41d4-a716-446655440000`) — PostgreSQL's `uuid_out`. Input is flexible but
**output is always this one form**, so the corpus is deterministic. This reuses the `T` render tag
(a *rendering* tag, not a type assertion — conformance.md §1; a uuid renders as a printable ASCII
string). Every core must emit the identical spelling (a CLAUDE.md §8 decision, like bytea's hex
and boolean's `true`/`false`).

**uuid IS a valid `PRIMARY KEY` — the first non-integer key.** Unlike text (§11), boolean (§9),
decimal (§12), and bytea (§13) — all of which reject a PK `0A000` and leave their key encoding
authored-but-unexercised — a `uuid` column **may be a `PRIMARY KEY`** this slice. Its
order-preserving key encoding (`uuid-raw16`, encoding.md §2.7) is the bare 16 bytes: fixed-width,
unsigned, no escape/terminator/sign-flip, so unsigned `memcmp` over the stored key bytes *is* the
logical order and the sorted store iterates uuid PKs correctly with no comparator. This makes uuid
the proof that the executor key path generalizes beyond integers (the narrowing lift those other
types defer). A uuid PK is NOT NULL (every PK is), so its key carries no nullable presence tag.

**On-disk.** Stable type code `8` (format.md). The stored value uses a **fixed 16-byte** body
behind the presence tag — **no `u16` length prefix** (the width is implied by the type), the first
fixed-width non-integer value. For uuid the stored value body and the key body coincide (both the
raw 16 bytes), so reusing one codec is exact. A uuid is always 16 bytes, far below the page limit,
so the oversized-item narrowing never applies.

**Deferred uuid sub-features** (relaxable narrowings, each its own follow-up):

- **uuid ⇄ other casts** (§5) — `text ⇄ uuid` and `bytea ⇄ uuid` casts are deferred to a cast
  slice (PostgreSQL has `text ⇄ uuid`); `CAST(x AS uuid)` and casting from a uuid trap `0A000` /
  `42804` this slice, exactly as bytea's casts are deferred.
- **uuid functions** — `gen_random_uuid()` (generation), `uuid_generate_v*`, and any uuid
  accessor functions are deferred; this slice is comparison + storage only (`= < > <= >=`,
  `IS [NOT] DISTINCT FROM`), with values supplied as literals.
