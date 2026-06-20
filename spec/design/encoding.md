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
width (`i16` → 2 bytes, `i32` → 4, `i64` → 8); the value is assumed already
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

**Status — EXERCISED.** `text` **is** a valid `PRIMARY KEY` / ordered secondary index / `UNIQUE`
key — the first *variable-width* non-integer key (uuid §2.7 and boolean §2.9 were fixed-width).
A text PK stores the bare `text-terminated-escape` body (a PK is NOT NULL, so no presence tag);
an index entry / composite-key member wraps it in the §2.2 nullable slot, and because the
terminator makes it self-delimiting it composes with following components and the index
storage-key suffix. The `(value → bytes)` vectors are in
[../encoding/text.toml](../encoding/text.toml) and the on-disk image is pinned by the
`text_pk_table.jed` golden ([../fileformat/format.md](../fileformat/format.md)). A text value too
large to fit a node has an over-`RECORD_MAX` key (keys cannot spill to overflow pages), rejected
`0A000` at the insert that stored it — the deliberate narrowing (PostgreSQL caps btree keys
similarly). Stored text *values* still use the separate, simpler **value codec** (length-prefixed
UTF-8, no order-preservation needed — [../fileformat/format.md](../fileformat/format.md)).

### 2.5 Decimal — `decimal-order-preserving`

`decimal` is variable-width and signed with a varying exponent, so its order-preserving key —
like text's — must be self-delimiting and sort byte-for-byte by **numeric value**, independent
of stored display scale (`1.5` and `1.50` must encode identically — they are equal). The rule
follows CockroachDB's decimal encoding (CLAUDE.md §8/§12). Normalize the value first to
`(sign, mantissa, E)` where the mantissa is the coefficient's significant decimal digits with
**trailing zero digit-pairs removed** and `E` is the base-100 exponent of the most-significant
digit-pair (value = `0.dd dd … × 100^E`, mantissa in `[0.01, 1)`). The normalization, from the
stored `(sign, coefficient-digits, scale)`: let `decpt = precision − scale` (the base-10
decimal-point exponent — `value = 0.<digits> × 10^decpt`); strip trailing zero *digits* (decpt
unchanged, since a trailing zero is least-significant); then `E = ⌊(decpt + 1) / 2⌋`, prepend a
`'0'` digit when `decpt` is **odd** (so the leading base-100 pair is `0 d₁`), and pad the digit
string right to an even length. Encode **ascending**:

1. **Sign/class byte**: `0x03` negative, `0x04` zero, `0x05` positive — so
   `neg < zero < pos` by raw byte. (Zero is the single byte `0x04`; `0x02`/`0x06` are reserved
   should ±∞ ever be needed — decimal has neither, §12 of [types.md](types.md).)
2. **Exponent `E`**: a **fixed-width 4-byte `int-be-signflip` `i32`** (§2.1), so it sorts
   ascending and, for positives, a larger `E` (larger magnitude) sorts later. `i32` (not `i16`)
   because `E` ranges over roughly `[−8192, 65536]` — `decpt` reaches `MAX_INT_DIGITS = 131072`
   at scale 0 (`MAX_SCALE` on the negative side), which overflows an `i16`. (A bias+big-endian
   *varint* would also sort, but a fixed `i32` is simpler, allocation-free, and the cross-core
   byte contract is easier to pin — the deliberate refinement over CockroachDB's varint.)
3. **Mantissa**: the digit-pairs most-significant first, each emitted as a byte in
   `[0x01, 0x64]` (`pair + 1`, reserving `0x00` for the terminator), so big-endian pair order
   is `memcmp` order within an exponent. (Stripping trailing zero *digits* before pairing means
   no trailing `[00]` pair can arise — the last base-10 digit is nonzero — so "trailing zero
   digit-pairs removed" holds by construction.)
4. **Terminator `0x00`** (a shorter mantissa sorts before a longer one that extends it, since
   `0x00 <` any pair byte).

For **negative** values steps 2–4 are **bitwise-complemented** (so "more negative" sorts
first), the same mirror §2.3 uses for descending; the sign/class byte itself is **not**
complemented (it is chosen directly so `neg < zero < pos`). This composes with the §2.2 nullable
presence tag (`0x00` present ‖ encoding, or `0x01` NULL) and the §2.3 descending inversion
unchanged. Because the key encodes the **value, not the display scale**, `1.5` and `1.50`
produce identical key bytes — they index as equal, matching `1.5 = 1.50`. Worked bytes:

| value | encoded key bytes |
|---|---|
| `0` (any scale) | `04` |
| `1.5` = `1.50` | `05 80 00 00 01 02 33 00` |
| `100` | `05 80 00 00 02 02 00` |
| `-1.5` | `03 7F FF FF FE FD CC FF` |

`1.5` = `0.[01][50] × 100¹`: class `05`, `E = 1` (`i32` sign-flip → `80 00 00 01`), pairs `01+1`
and `50+1` (`02 33`), terminator `00`. `-1.5` is that body bitwise-complemented under class `03`.

**Status — EXERCISED.** Like text (§2.4), `decimal` **is** a valid `PRIMARY KEY` / ordered
secondary index / `UNIQUE` key — its variable-width `decimal-order-preserving` body is
self-delimiting, so it composes in composite keys / index suffixes. A decimal PK stores the bare
body (a PK is NOT NULL, so no presence tag); an index entry / composite member wraps it in the
§2.2 nullable slot. Because the key encodes the value (not the scale), a `UNIQUE` decimal index
treats `1.5` and `1.50` as one. The `(value → bytes)` vectors are in
[../encoding/decimal.toml](../encoding/decimal.toml) and the on-disk image is pinned by the
`decimal_pk_table.jed` golden ([../fileformat/format.md](../fileformat/format.md)). A decimal
value too large to fit a node has an over-`RECORD_MAX` key (keys cannot spill), rejected `0A000`
at the insert that stored it — the same node-fit narrowing as text. Stored decimal *values* still
use the separate, simpler **value codec** (sign + scale + base-10⁴ groups, no order-preservation
— [../fileformat/format.md](../fileformat/format.md)).

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

**Status — EXERCISED.** Exactly like text (§2.4): `bytea` **is** a valid `PRIMARY KEY` / ordered
index / `UNIQUE` key, storing the bare `bytea-terminated-escape` body. The escape matters *more*
here — raw `0x00` bytes are common in binary data — so the embedded-`0x00` case is routinely hit.
The `(value → bytes)` vectors are in [../encoding/bytea.toml](../encoding/bytea.toml) and the
on-disk image is pinned by the `bytea_pk_table.jed` golden. An over-`RECORD_MAX` bytea key is
rejected `0A000` (the same node-fit narrowing as text). Stored bytea *values* still use the
compact length-prefixed **value codec** (raw bytes, no order-preservation needed —
[../fileformat/format.md](../fileformat/format.md)).

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

### 2.8 Float (`f32` / `f64`) — `float-order-preserving` (authored; unexercised this slice)

Both binary floats are fixed-width (`f64` = 8 bytes, `f32` = 4 bytes) but, unlike the
integers, the IEEE 754 bit pattern does **not** sort by `memcmp` in numeric order: negatives have
the sign bit set (so they would sort *above* positives), and within negatives larger magnitudes
sort later (backwards). The standard transform maps the type's **total order**
([compare.toml](../types/compare.toml) `float = "float-total-order"`;
[../design/float.md](float.md) §3) onto unsigned byte order — identical for both widths, over a
`u32` (`f32`) or `u64` (`f64`):

1. **Canonicalize** first, so equal values encode identically: `-0.0 → +0.0`, and every NaN → one
   canonical NaN bit pattern (`NaN = NaN` and `-0 = +0` in the total order — §3).
2. Take the IEEE bits as a big-endian unsigned integer (`u64`/`u32`); **if the sign bit is set
   (negative) flip all bits, else flip just the sign bit.** Negatives then sort below positives
   and in correct (magnitude-reversed) order; the canonical NaN, having the largest payload above
   `+Infinity`, lands last — matching `−Inf < finite < +Inf < NaN`.

This composes with the §2.2 nullable presence tag (`0x00` present ‖ the 8 bytes, or `0x01` NULL)
and the §2.3 descending inversion unchanged.

**Status — authored, not yet exercised.** A `f32`/`f64` `PRIMARY KEY`/index is rejected
`0A000` this slice — the text/decimal/bytea/interval precedent, reinforced by the
**contamination** rule ([determinism.md](determinism.md) §4): keeping an exempted-value type out
of *keys* bounds float non-determinism to *query-time* order, never *stored* order. Stored float
*values* use the simpler fixed value codec ([../fileformat/format.md](../fileformat/format.md),
type code 12 for `f64` / 13 for `f32`), which preserves the bits verbatim (no
canonicalization) because a stored value never needs to sort.
Lifting the narrowing adds the `(value → bytes)` fixtures and the executor key path then.

### 2.9 Boolean — `bool-byte` (the second EXERCISED non-integer key)

`boolean` is a fixed **1-byte** value (the two-element domain `{false, true}`, ordered
`false < true` — [types.md §9](types.md)). Like `uuid` (§2.7) — and unlike the variable-width
text/decimal/bytea — it needs **no escape, terminator, or length prefix**: a single byte is
self-delimiting by width alone. The rule is the simplest in this doc:

1. The value's byte is **`0x00` for `false`, `0x01` for `true`**.
2. **No sign-flip** (the domain is unsigned), **no escape, no terminator.**

Because `0x00 < 0x01`, unsigned `memcmp` over the one byte **is** the type's logical order
(`false < true`), so the byte already sorts correctly with no transformation. The body is
byte-identical to the boolean *value* codec (a stored boolean is the same `bool-byte` behind
the §2.2 presence tag — [../fileformat/format.md](../fileformat/format.md), type code 5), the
simplest case of the §3 key/value seam coinciding (as for uuid). Worked bytes:

| value | encoded key bytes |
|---|---|
| `false` | `00` |
| `true` | `01` |

The **nullable** slot is the §2.2 tag (`0x00` present ‖ the 1 byte, or `0x01` for NULL — so
`false`→`00 00`, `true`→`00 01`, NULL→`01`) and **descending** is the §2.3 whole-component
bitwise inversion (NULL→`fe`, `true`→`ff fe`, `false`→`ff ff`) — both unchanged.

**Status — EXERCISED.** Like uuid (§2.7), `boolean` **is** allowed in a `PRIMARY KEY` / index
([types.md §9](types.md)), making it the **second non-integer key type**. So boolean key vectors
are authored in [../encoding/integers.toml](../encoding/integers.toml) and the executor encodes a
boolean PK to the bare 1 byte (a PK is NOT NULL, so no presence tag), pinned by the
`bool_pk_table.jed` golden ([../fileformat/format.md](../fileformat/format.md)).

## 3. Where this is used today

The bare integer rule is exercised by every stored key. The on-disk **value codec**
([../fileformat/format.md](../fileformat/format.md)) reuses the §2.2 nullable encoding to
serialize each row value (the tag marks NULL); for a stored *value* the tag's sort order is
irrelevant, but reusing one codec keeps key and value bytes consistent and is what lets the
seam diverge cleanly if a future type ever needs distinct key/value forms. `text` is the first
type where the key and value forms genuinely diverge: text *values* are stored with a compact
length-prefixed value codec (format.md), while a text *key* uses the order-preserving
`text-terminated-escape` rule (§2.4) — both now exercised (a text `PRIMARY KEY` / index /
`UNIQUE`, pinned by `text_pk_table.jed`). `bytea` (§2.6) is the same: raw-byte *values* via the
compact value codec, order-preserving *keys* via `bytea-terminated-escape`
(`bytea_pk_table.jed`). `uuid` (§2.7) is
the **exception and the first non-integer key actually exercised**: a uuid `PRIMARY KEY` stores
the bare 16 bytes as its key (so the BTree/sorted store iterates uuid PKs in correct logical
order with no comparator), proving the executor key path generalizes beyond integers. `boolean`
(§2.9) is the **second** such key — a boolean `PRIMARY KEY`/index stores the bare `bool-byte`
(`0x00`/`0x01`), pinned by the `bool_pk_table.jed` golden.
**Composite keys are exercised too**: a composite `PRIMARY KEY` ([constraints.md §3](constraints.md))
concatenates its fixed-width components per §2.3, pinned by the `composite_pk_table.jed`
golden. Nullable **secondary indexes** have since **landed** ([indexes.md](indexes.md),
`index_table.jed` golden) — the first place §2.2's presence-tag sort order is load-bearing
rather than spec-only — as have `timestamp`/`timestamptz` keys (the i64 rule), `text`/`bytea`
keys (the `…-terminated-escape` rules §2.4/§2.6), and `decimal` keys (the
`decimal-order-preserving` rule §2.5, `decimal_pk_table.jed`). The remaining non-integer scalars
(`float`, `interval`) add their own §2 key paths when their in-key narrowings lift.

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
