# Order-preserving key encoding — design

> The reasoning behind the key encoding: why keys are byte-comparable, the
> `int-be-signflip` rule for bare integers, the nullable presence tag, how components
> compose, and descending order — plus the **NULL sort-position decision**. The
> authoritative data is the per-type `encoding` field in
> [../types/scalars.toml](../types/scalars.toml) and the byte vectors in
> [../encoding/integers.toml](../encoding/integers.toml); this doc is the *why*. When a
> decision here changes, update [CLAUDE.md](../../CLAUDE.md) §8 and
> [../types/compare.toml](../types/compare.toml) (`null_ordering`) in the same edit.

## 1. The contract: stored order == logical order, by `memcmp`

Keys are stored sorted and iterated in **raw byte order** (CLAUDE.md §8). So the encoding
of a value must sort, byte-for-byte under `memcmp`, **identically to the value's logical
order** — and identically across *every* implementation. Get this right and the stored key
order needs no comparator: a forward scan *is* ascending order, a reverse scan *is*
descending. Get it wrong and every core silently disagrees the first time a key crosses a
sign boundary or meets a NULL.

Two properties make this verifiable rather than hoped-for:

1. The rule is **data** — a single `encoding` method name per type ([scalars.toml](../types/scalars.toml)).
2. Shared `(value → expected bytes)` **fixtures** ([integers.toml](../encoding/integers.toml))
   are re-derived from scratch by an independent reference encoder
   ([../encoding/verify.rb](../encoding/verify.rb)) and checked for round-trip,
   byte-exactness, and strict order. CockroachDB's `encoding` package is the reference
   design (CLAUDE.md §8/§12).

## 2. The encoding rules

### 2.1 Bare integers — `int-be-signflip`

Fixed-width **big-endian**, with the **sign bit inverted**. Big-endian is forced by
byte-wise comparison: `memcmp` reads the most-significant byte first, so the MSB must be
stored first. The sign-bit flip — equivalently, **add the bias `2^(bits-1)` and emit the
sum as an unsigned big-endian integer** — maps the two's-complement signed range
monotonically onto `[0, 2^bits)`, so negatives sort below positives. Width is the type's
width (`int16` → 2 bytes, `int32` → 4, `int64` → 8); the value is assumed already
range-checked by the caller.

### 2.2 Nullable key slot — the presence tag

A column that can hold NULL needs the *absence* of a value to be encodable and to sort at a
**defined** position. A nullable slot is a **1-byte presence tag** followed, when present,
by the bare value bytes:

| slot | bytes |
|---|---|
| present value `v` | `0x00` ‖ `int-be-signflip(v)` |
| NULL | `0x01` |

Because `0x00 < 0x01` and the tag is the first byte, **every present value sorts before
NULL** — so NULLs sort **last** in ascending order. See §4 for why that is the chosen
position. The tag is one byte, not a bit stolen from the value, so the value encoding in
§2.1 is reused verbatim and stays width-clean.

**Status — EXERCISED** (the secondary-index slice): every component of a **secondary-index
entry key** is a nullable slot — uniformly, even for a NOT NULL column — because an indexed
column, unlike a PK member, may hold NULL ([indexes.md §3](indexes.md)). The slot's sort
order is what places NULL last in the stored index order. Before that slice the tag was
authored but appeared only behind the value codec (§3), where its order is irrelevant.

### 2.3 Composition and descending

- **Composite keys** are the **concatenation** of their components' encodings, left to
  right. Each component is either fixed-width (the integer types) or self-delimiting, so the
  concatenation stays order-preserving without separators. **Status — EXERCISED:** a
  composite `PRIMARY KEY` ([constraints.md §3](constraints.md)) stores exactly this
  concatenation **in the constraint's list order** (which may differ from declaration order —
  the catalog persists the key order explicitly since `format_version` 5); every keyable
  component type is fixed-width today — integers, uuid, timestamps — so the widths come from
  the schema and `memcmp` equals the tuple's lexicographic order. The cross-core bytes are
  pinned by the `composite_pk_table.jed` golden
  ([../fileformat/format.md](../fileformat/format.md)). A **secondary-index entry key**
  ([indexes.md §3](indexes.md)) is the same composition with two twists: each indexed
  component is wrapped in the §2.2 **nullable slot** (tag + bare encoding), and the row's
  storage key is appended as the final component — the suffix that makes every entry unique
  and recoverable (each prefix component is self-delimiting, so the suffix needs no length
  field). Pinned by the `index_table.jed` golden.
- **Descending order** is the **bitwise inversion (one's complement)** of a component,
  *tag byte included*. Inverting every byte reverses `memcmp` order exactly, so a descending
  component sorts as the mirror of its ascending form. Under inversion the nullable tag
  flips `0x00 ↔ 0xFF` and `0x01 ↔ 0xFE`, so **NULL (`0xFE`) sorts before every present
  value (`0xFF…`)** — i.e. descending lifts NULL to **first**, the exact mirror of §2.2.

### 2.4 Text — `text-terminated-escape` (authored; unexercised this slice)

`text` is variable-width, so unlike the fixed-width integers it cannot be a self-delimiting
key component by raw bytes alone: a plain length prefix is **not** order-preserving (it sorts
by length first — `"b"` length 1 would sort before `"aa"` length 2, inverting the correct
`"aa" < "b"`). The order-preserving + self-delimiting rule (CockroachDB's `encoding` package,
CLAUDE.md §8/§12) is:

1. The value's bytes are its **UTF-8 encoding** — the `C` collation (byte / code-point order;
   §4 of [types.md](types.md)). For UTF-8, `memcmp` of the bytes equals Unicode code-point
   order, so raw content bytes already sort correctly.
2. **Escape** every `0x00` byte in the content to `0x00 0xFF`.
3. **Terminate** the whole string with `0x00 0x01`.

Because the only place a `0x00` is followed by a byte `< 0xFF` is the terminator (`0x00 0x01`),
the terminator sorts below any real continuation (`0x01 < 0xFF`, and `0x01` < any non-zero
content byte), so a string sorts before any string that extends it. Worked bytes (content as
UTF-8 hex):

| value | encoded key bytes |
|---|---|
| `""` | `00 01` |
| `"a"` | `61 00 01` |
| `"aa"` | `61 61 00 01` |
| `"ab"` | `61 62 00 01` |
| `"b"` | `62 00 01` |
| `"a\0b"` (literal NUL) | `61 00 FF 62 00 01` |
| `"é"` (U+00E9) | `C3 A9 00 01` |
| `"😀"` (U+1F600) | `F0 9F 98 80 00 01` |

`memcmp` then yields `"" < "a" < "aa" < "ab" < "b"`: the prefix case `"a" < "aa"` works because
`"a"`'s terminator byte `00` beats `"aa"`'s second content byte `61`; the length-prefix
counterexample `"aa" < "b"` works because content compares before any terminator (`61 < 62`).
The escape is what stops a literal `0x00` from masquerading as a terminator (`"a" < "a\0b"`).
**Descending** is the same §2.3 whole-component bitwise inversion (delimiters included:
`00 01 → FF FE`, `00 FF → FF 00`); a terminated shorter string then inverts to sort *after* a
longer one, the correct mirror. The **nullable** slot is the §2.2 tag (`0x00` present ‖ the
encoding above, or `0x01` for NULL).

**Status — authored, not yet exercised.** This slice (`text` as a storable column) keeps text
out of keys: a text PRIMARY KEY is rejected `0A000` (a documented, relaxable narrowing —
[types.md §11](types.md)). So no `text` key fixtures or executor key path exist yet; the rule
is recorded here as a property of the type, exactly as the `bool-byte` rule is recorded but
unexercised. Stored text *values* use a separate, simpler **value codec** (length-prefixed
UTF-8, no order-preservation needed — [../fileformat/format.md](../fileformat/format.md)).
Lifting the narrowing (text in a key / secondary index) adds the `(value → bytes)` fixtures to
[../encoding/](../encoding/) and the executor path then.

### 2.5 Decimal — `decimal-order-preserving` (authored; unexercised this slice)

`decimal` is variable-width and signed with a varying exponent, so its order-preserving key —
like text's — must be self-delimiting and sort byte-for-byte by **numeric value**, independent
of stored display scale (`1.5` and `1.50` must encode identically — they are equal). The rule
follows CockroachDB's decimal encoding (CLAUDE.md §8/§12). Normalize the value first to
`(sign, mantissa, E)` where the mantissa is the coefficient's significant decimal digits with
**trailing zero digit-pairs removed** and `E` is the base-100 exponent of the most-significant
digit-pair (value ≈ `0.dd dd … × 100^E`, mantissa in `[0.01, 1)`). Encode **ascending**:

1. **Sign/class byte**: `0x03` negative, `0x04` zero, `0x05` positive — so
   `neg < zero < pos` by raw byte. (Zero is the single byte `0x04`; `0x02`/`0x06` are reserved
   should ±∞ ever be needed — decimal has neither, §12 of [types.md](types.md).)
2. **Exponent `E`**, order-preserving: a bias+big-endian varint that sorts ascending, so for
   positives a larger `E` (larger magnitude) sorts later.
3. **Mantissa**: the digit-pairs most-significant first, each emitted as a byte in
   `[0x01, 0x64]` (`pair + 1`, reserving `0x00` for the terminator), so big-endian pair order
   is `memcmp` order within an exponent.
4. **Terminator `0x00`** (a shorter mantissa sorts before a longer one that extends it, since
   `0x00 <` any pair byte).

For **negative** values steps 2–4 are **bitwise-complemented** (so "more negative" sorts
first), the same mirror §2.3 uses for descending. This composes with the §2.2 nullable
presence tag (`0x00` present ‖ encoding, or `0x01` NULL) and the §2.3 descending inversion
unchanged. Because the key encodes the **value, not the display scale**, `1.5` and `1.50`
produce identical key bytes — they index as equal, matching `1.5 = 1.50`.

**Status — authored, not yet exercised.** This slice keeps decimal out of keys: a decimal
`PRIMARY KEY` is rejected `0A000` (a documented, relaxable narrowing — [types.md](types.md)
§12), exactly as text's. No decimal key fixtures or executor key path exist yet; lifting the
narrowing (decimal in a key / secondary index) adds `(value → bytes)` fixtures to
[../encoding/](../encoding/) and the executor path then. Stored decimal *values* use the
separate, simpler **value codec** (sign + scale + base-10⁴ groups, no order-preservation —
[../fileformat/format.md](../fileformat/format.md)).

### 2.6 Bytea — `bytea-terminated-escape` (authored; unexercised this slice)

`bytea` is variable-width, so it needs the **same** order-preserving + self-delimiting rule as
text (§2.4) — a plain length prefix sorts by length first, which is wrong. The rule is
identical in structure; only step 1 differs, because bytea has no character encoding:

1. The value's bytes are its **raw bytes** — no UTF-8, no collation, no transformation. Unsigned
   `memcmp` of the raw bytes **is** the type's logical order (`bytea = "byte-ascending"`,
   [types.md §13](types.md)), so the content bytes already sort correctly.
2. **Escape** every `0x00` byte in the content to `0x00 0xFF`.
3. **Terminate** the whole value with `0x00 0x01`.

The order-preservation argument is exactly §2.4's (the terminator `0x00 0x01` sorts below any
real continuation, so a value sorts before any value that extends it; the escape stops a literal
`0x00` from masquerading as a terminator) — and it matters **more** for bytea than for text:
raw `0x00` bytes are common in binary data and there is no UTF-8 validity constraint forbidding
them, so the escape is routinely exercised rather than an edge case. Worked bytes (content as
raw hex):

| value | encoded key bytes |
|---|---|
| `\x` (empty) | `00 01` |
| `\x61` | `61 00 01` |
| `\x6161` | `61 61 00 01` |
| `\x62` | `62 00 01` |
| `\x6100ff62` (embedded NUL) | `61 00 FF FF 62 00 01` |

`memcmp` yields `\x < \x61 < \x6161 < \x62`: the prefix case `\x61 < \x6161` works because
`\x61`'s terminator byte `00` beats `\x6161`'s second content byte `61`; the length-prefix
counterexample `\x6161 < \x62` works because content compares before any terminator
(`61 < 62`). **Descending** and the **nullable** slot are the §2.3 / §2.2 rules unchanged
(whole-component bitwise inversion; the `0x00` present / `0x01` NULL tag).

**Status — authored, not yet exercised.** Exactly like text (§2.4): a bytea `PRIMARY KEY` is
rejected `0A000` this slice ([types.md §13](types.md)), so no bytea key fixtures or executor
key path exist yet. Stored bytea *values* use the compact length-prefixed **value codec** (raw
bytes, no order-preservation needed — [../fileformat/format.md](../fileformat/format.md)).
Lifting the narrowing adds the `(value → bytes)` fixtures and the executor path then.

### 2.7 UUID — `uuid-raw16` (the first EXERCISED non-integer key)

`uuid` is a fixed **16-byte** value (RFC 4122 — [types.md §14](types.md)). Unlike the
variable-width types above, it needs **no escape, terminator, or length prefix**: it is
fixed-width, so it is self-delimiting by width alone, exactly like the bare integers (§2.1).
The rule is the simplest in this doc:

1. The value's bytes are its **16 raw bytes**, big-endian (RFC 4122 stores the fields in
   network byte order, so the canonical `8-4-4-4-12` text form's hex, read left to right, is
   already the big-endian byte order).
2. **No sign-flip** (uuid is unsigned), **no escape, no terminator.**

Unsigned `memcmp` over the 16 bytes **is** the type's logical order (`uuid = "byte-ascending"`,
[types.md §14](types.md); [../types/compare.toml](../types/compare.toml)), so the bytes already
sort correctly with no transformation. Because every value is exactly 16 bytes there is **no
prefix/length case** to worry about (the wrinkle §2.4/§2.6 solve for variable-width types
simply cannot arise). Worked bytes:

| value | encoded key bytes |
|---|---|
| `00000000-0000-0000-0000-000000000000` | `00000000000000000000000000000000` |
| `00000000-0000-0000-0000-000000000001` | `00000000000000000000000000000001` |
| `550e8400-e29b-41d4-a716-446655440000` | `550e8400e29b41d4a716446655440000` |
| `ffffffff-ffff-ffff-ffff-ffffffffffff` | `ffffffffffffffffffffffffffffffff` |

The **nullable** slot is the §2.2 tag (`0x00` present ‖ the 16 bytes, or `0x01` for NULL) and
**descending** is the §2.3 whole-component bitwise inversion — both unchanged.

**Status — EXERCISED.** This is the difference from §2.4–§2.6: uuid **is** allowed in a
`PRIMARY KEY` / key this slice ([types.md §14](types.md)), making it the **first non-integer
key type**. So uuid key vectors are authored in [../encoding/integers.toml](../encoding/integers.toml)
and the executor encodes a uuid PK to these bytes (the stored key is the bare 16 bytes — a PK
is NOT NULL, so no presence tag). A stored uuid *value* reuses the same 16 bytes behind the
value-codec presence tag ([../fileformat/format.md](../fileformat/format.md)); for uuid the key
and value bodies coincide (both the raw 16 bytes), the simplest case of the §3 key/value seam.

### 2.8 Float64 — `float-order-preserving` (authored; unexercised this slice)

`float64` is fixed-width (8 bytes) but, unlike the integers, its IEEE 754 bit pattern does **not**
sort by `memcmp` in numeric order: negatives have the sign bit set (so they would sort *above*
positives), and within negatives larger magnitudes sort later (backwards). The standard transform
maps the type's **total order** ([compare.toml](../types/compare.toml) `float = "float-total-order"`;
[../design/float.md](float.md) §3) onto unsigned byte order:

1. **Canonicalize** first, so equal values encode identically: `-0.0 → +0.0`, and every NaN → one
   canonical NaN bit pattern (`NaN = NaN` and `-0 = +0` in the total order — §3).
2. Take the 64 IEEE bits as a big-endian `u64`; **if the sign bit is set (negative) flip all 64
   bits, else flip just the sign bit.** Negatives then sort below positives and in correct
   (magnitude-reversed) order; the canonical NaN, having the largest payload above `+Infinity`,
   lands last — matching `−Inf < finite < +Inf < NaN`.

This composes with the §2.2 nullable presence tag (`0x00` present ‖ the 8 bytes, or `0x01` NULL)
and the §2.3 descending inversion unchanged.

**Status — authored, not yet exercised.** A `float64 PRIMARY KEY`/index is rejected `0A000` this
slice — the text/decimal/bytea/interval precedent, reinforced by the **contamination** rule
([determinism.md](determinism.md) §4): keeping an exempted-value type out of *keys* bounds float
non-determinism to *query-time* order, never *stored* order. Stored float *values* use the simpler
fixed 8-byte value codec ([../fileformat/format.md](../fileformat/format.md), type code 12), which
preserves the bits verbatim (no canonicalization) because a stored value never needs to sort.
Lifting the narrowing adds the `(value → bytes)` fixtures and the executor key path then.

## 3. Where this is used today

The bare integer rule is exercised by every stored key. The on-disk **value codec**
([../fileformat/format.md](../fileformat/format.md)) reuses the §2.2 nullable encoding to
serialize each row value (the tag marks NULL); for a stored *value* the tag's sort order is
irrelevant, but reusing one codec keeps key and value bytes consistent and is what lets the
seam diverge cleanly if a future type ever needs distinct key/value forms. The text type is
the first such divergence: text *values* are stored with a compact length-prefixed value codec
(format.md), while the order-preserving text *key* rule (§2.4) is authored but unexercised —
text is not yet allowed in a key. `bytea` (§2.6) is the same: raw-byte *values* stored via the
compact value codec, the order-preserving *key* rule authored but unexercised. `uuid` (§2.7) is
the **exception and the first non-integer key actually exercised**: a uuid `PRIMARY KEY` stores
the bare 16 bytes as its key (so the BTree/sorted store iterates uuid PKs in correct logical
order with no comparator), proving the executor key path generalizes beyond integers.
**Composite keys are exercised too**: a composite `PRIMARY KEY` ([constraints.md §3](constraints.md))
concatenates its fixed-width components per §2.3, pinned by the `composite_pk_table.jed`
golden. The remaining non-integer scalars (`decimal`, …) will add their own §2 key paths when
their in-key narrowings lift; nullable *secondary indexes* — the first place §2.2's sort
order becomes load-bearing rather than spec-only — follow then too.

## 4. NULL ordering — NULL is the largest value (the PostgreSQL model)

The SQL standard leaves the sort position of NULL **implementation-defined**, which is why
`ORDER BY … NULLS FIRST | LAST` exists at all. The two coherent choices are NULL-smallest
(SQLite: ascending → NULLs first) and NULL-largest (PostgreSQL: ascending → NULLs last).
**The engine chooses NULL-largest** — `null_ordering = "nulls-last-ascending"` in
[../types/compare.toml](../types/compare.toml):

- **Ascending** → present values, then NULL **last**.
- **Descending** → NULL **first**, then present values (the §2.3 inversion).

This is realized purely by the tag-byte assignment in §2.2 (`0x00` present `<` `0x01` NULL),
so the physical scan order and the logical `ORDER BY` default are the *same* fact, not two
that must be kept in sync: a plain `ORDER BY col` (no `NULLS` clause) **mirrors the
index-iteration order**, and its default follows direction — `ASC` → `NULLS LAST`, `DESC` →
`NULLS FIRST` ([grammar.md §10](grammar.md)). An explicit `NULLS FIRST | LAST` overrides
that default regardless of direction; the executor keeps NULL placement **decoupled** from
the value-direction flip so all cores order NULLs byte-identically (CLAUDE.md §8).

**Why NULL-largest.** Two reasons, both rooted in CLAUDE.md:

1. **PostgreSQL is the behavioral default (CLAUDE.md §1).** Where a decision has a
   PostgreSQL option and no overriding reason against it, the engine takes it. NULL ordering
   is a pure default with no principled tie-breaker, and PG is both the audience's mental
   model and the project's differential-testing **oracle** (CLAUDE.md §7) — matching it
   means `ORDER BY` corpus generated from PG needs no hand-overrides for NULL placement.
2. **It costs nothing extra.** NULL-largest and NULL-smallest are the same one-byte tag
   assignment with the values swapped; neither is simpler. So the §8 divergence hotspot is
   settled by the standing PostgreSQL-default rule.

> **History.** This was originally ratified NULL-**smallest** (the SQLite model) with the
> step-4 key encoding, on the reasoning that `NULL = 0x00` is the "natural" absent tag. That
> rationale was aesthetic, not load-bearing — no stored key actually depended on it yet — so
> it was re-ratified to the PostgreSQL model under the standing "match PostgreSQL unless
> there's an overriding reason" guideline (CLAUDE.md §1). The flip is a one-byte tag swap
> plus the `ORDER BY` default; it touched the fixtures, the golden on-disk images, and the
> three cores in lockstep.
