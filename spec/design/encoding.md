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
`0A000` — unlike text/decimal/bytea/interval (whose narrowings have all since lifted), float's
rejection is **permanent**, reinforced by the
**contamination** rule ([determinism.md](determinism.md) §4): keeping an exempted-value type out
of *keys* bounds float non-determinism to *query-time* order, never *stored* order. With interval
keyable (§2.10), `float` is now the **lone** non-keyable scalar — the only one whose key narrowing
is principled (the determinism carve-out) rather than merely pending. Stored float
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

### 2.10 Interval — `interval-span-i128`

`interval` is the engine's first type whose **comparison key differs from its stored
representation** ([interval.md §2](interval.md)): the three independent fields `months` (i32),
`days` (i32), `micros` (i64) are stored separately (so `+ 1 month` stays calendar-aware), but
comparison/ordering/dedup collapse them into a single signed **128-bit microsecond span**

```
span(iv) = (iv.months · 30 + iv.days) · 86_400_000_000 + iv.micros        # signed 128-bit
```

(1 month = 30 days, 1 day = 24 h — PG `interval_cmp_value`). The key must sort by that span, so it
is simply the span run through the **`int-be-signflip` rule (§2.1) at i128 width**:

1. Compute `span(iv)` (a signed 128-bit value — `(i32·30 + i32)·86.4e9 + i64` overflows i64 but fits
   i128 with vast headroom).
2. **Add the bias `2^127`** and emit the sum as a **16-byte big-endian unsigned integer**, mapping
   the signed span range monotonically onto `[0, 2^128)` so negatives sort below positives.
3. **No escape, no terminator, no length prefix** — every value is exactly 16 bytes, so it is
   self-delimiting by width alone (exactly like uuid §2.7 and the bare integers §2.1).

Because the key is the **span**, two field-distinct but span-equal intervals (`1 mon` / `30 days` /
`720:00:00` all have span `2_592_000_000_000`) produce **identical key bytes** — they index as
equal, so a `UNIQUE` interval index treats them as one (`1 mon = 30 days` is also `TRUE`,
[interval.md §2](interval.md)). This is the **"equal but not identical"** wrinkle: the exact analogue
of decimal's scale-independence (`1.5` / `1.50`, §2.5) — the key encodes the canonical value, while
the stored *value* preserves each interval's own three fields (and renders them distinctly). Worked
bytes:

| value | span | encoded key bytes |
|---|---|---|
| `00:00:00` (zero) | `0` | `80000000000000000000000000000000` |
| `00:00:00.000001` | `1` | `80000000000000000000000000000001` |
| `-00:00:00.000001` | `-1` | `7fffffffffffffffffffffffffffffff` |
| `1 day` | `86_400_000_000` | `8000000000000000000000141dd76000` |
| `1 mon` = `30 days` | `2_592_000_000_000` | `80000000000000000000025b7f3d4000` |
| `-1 day` | `-86_400_000_000` | `7fffffffffffffffffffffebe228a000` |

The **nullable** slot is the §2.2 tag (`0x00` present ‖ the 16 bytes, or `0x01` for NULL) and
**descending** is the §2.3 whole-component bitwise inversion — both unchanged.

**Status — EXERCISED.** `interval` **is** a valid `PRIMARY KEY` / ordered secondary index /
`UNIQUE` key ([interval.md §6](interval.md)) — the **third** fixed-width non-integer key (after uuid
§2.7 and boolean §2.9), and the first whose 16-byte key body is *not* its value body (the value
codec stores the three raw fields — `months ‖ days ‖ micros`, [../fileformat/format.md](../fileformat/format.md)
type code 11 — while the key stores the derived span; the §3 key/value seam genuinely diverging). An
interval PK stores the bare 16-byte span (a PK is NOT NULL, so no presence tag); an index entry /
composite member wraps it in the §2.2 nullable slot, and because it is fixed-width it qualifies as a
**GIN element** too ([gin.md §3](gin.md) — span-equal elements share a term, matching the `@>`/`&&`
element-equality). The `(value → bytes)` vectors are in [../encoding/interval.toml](../encoding/interval.toml)
and the on-disk image is pinned by the `interval_pk_table.jed` golden. Only `float` (the determinism
carve-out §2.8) and the recursive containers composite/array remain `0A000` keys.

### 2.11 Range — `range-bounds` (the first container key)

`range` is the engine's first **container** key — a structural type over a scalar element
([ranges.md §2](ranges.md)), so its key is **recursive**: it frames the range's shape (empty, the two
bound infinities, inclusivity) and embeds the **element type's own order-preserving key** (§2.1 for
the integers, §2.5 for `decimal`, the i32 day rule for `date`, the i64 instant rule for the
timestamps) for each finite bound. The layout mirrors PG `range_cmp` exactly
([ranges.md §6](ranges.md), `range_total_cmp`): **empty sorts below every non-empty range**, then by
**lower bound**, then by **upper bound**.

```
empty range:      0x00
non-empty range:  0x01 ‖ <lower bound> ‖ <upper bound>

bound (per side):
  infinite:  0x00  (−∞, lower side only)  |  0x02  (+∞, upper side only)
  finite:    0x01 ‖ <element key> ‖ <inclusivity byte>
```

1. **Empty discriminator.** A leading `0x00` for the empty range (the *entire* key — no bounds
   follow) vs. `0x01` for a non-empty range. `0x00 < 0x01`, so empty sorts first, and **all empty
   ranges share the one-byte key `00`** (they are all `==`, [ranges.md §4](ranges.md)).
2. **Bound infinity marker, ordered −∞ < finite < +∞.** Each bound opens with a marker: a lower
   bound is `0x00` (−∞, unbounded) or `0x01` (finite); an upper bound is `0x01` (finite) or `0x02`
   (+∞, unbounded). A lower bound never uses `0x02` and an upper never `0x00`, so the markers totally
   order the three bound kinds — the unbounded-lower range sorts below every finite-lower one, the
   unbounded-upper above every finite-upper one, exactly as `range_cmp_bounds` ranks an infinite
   bound.
3. **Finite bound = element key ‖ inclusivity byte.** After the `0x01` finite marker comes the bound
   value's **element key** (the same bytes a column of that element type would store —
   self-delimiting: fixed-width for int/date/timestamp, `0x00`-terminated for decimal §2.5), then a
   one-byte **inclusivity tie-break**. For equal element values PG breaks the tie by inclusivity, and
   the side decides the direction: on the **lower** side an inclusive bound sorts *before* an
   exclusive one (`[5,` starts at 5, `(5,` just after) → inclusive `0x00`, exclusive `0x01`; on the
   **upper** side an exclusive bound sorts *before* an inclusive one (`,5)` ends just before 5, `,5]`
   at 5) → exclusive `0x00`, inclusive `0x01`. (Equivalently the byte is `0x00` when `inclusive ==
   is_lower`, else `0x01`.)
4. **No length prefix, no whole-key terminator** — every component is fixed-width or
   self-terminating, so the concatenation is self-delimiting and `memcmp` reproduces
   `range_total_cmp`. Keys never round-trip (the row body holds the full range value), so the key
   need only *sort*.

Discrete ranges (`i32range`/`i64range`/`daterange`) are stored in PG's canonical `[)` form, so
`[1,4]` and `[1,5)` over `i32range` are the *same* canonical value and encode identically — not a key
wrinkle but genuine equality ([ranges.md §4](ranges.md)). The continuous ranges carry the two element
wrinkles through unchanged: a `numrange` bound inherits decimal scale-independence (`[1.5,…` and
`[1.50,…` share a key, §2.5), and inclusivity is significant (`[1.5,2)` ≠ `(1.5,2)` → distinct keys).
Worked structure for `'[1,5)'::i32range` (lower inclusive 1, upper exclusive 5):

```
01   01 80000001 00    01 80000005 00
└┬┘  └────┬────┘ └┬┘   └────┬────┘ └┬┘
non-   lower=1   lower-    upper=5  upper-
empty  finite    incl      finite   excl
                 (0x00)              (0x00)
```

= `01 01 80000001 00 01 80000005 00`.

The **nullable** slot (a range secondary index / composite member) is the §2.2 tag (`0x00` present ‖
the bytes above, or `0x01` NULL); **descending** is the §2.3 whole-key inversion — both unchanged. A
range key whose element bytes overflow a node trips the existing oversized-item `0A000` (§2.4).

**Status — EXERCISED.** `range` **is** a valid `PRIMARY KEY` / ordered secondary index / `UNIQUE` key
/ FK target ([ranges.md §10](ranges.md)) over all six built-in range types — the **first container
key** (composite §2.3 is a flat tuple concatenation; range recurses into the element codec with shape
framing). Point-lookup pushdown stays **deferred** for ranges (a range PK/index `WHERE k = …`
full-scans + residual-filters — correct, just unindexed — matching the container precedent), and a
range is **not** a GIN element. The `(value → bytes)` vectors are in
[../encoding/range.toml](../encoding/range.toml); the on-disk image is pinned by the
`range_pk_table.jed` golden. With range exercised, only `float` (the determinism carve-out §2.8) and
the recursive **composite/array** containers remain `0A000` keys.

### 2.12 Collated text — `text-collated-sortkey` (a key *form*, not a new type)

A `text` column carrying a **non-`C` collation** ([collation.md](collation.md)) does not key by its
raw UTF-8 bytes (the `C` rule §2.4) — that would store dictionary words in byte order, the whole
point collation fixes. Instead the key body is the column collation's **UCA sort key**
([../collation/README.md §4](../collation/README.md), [collation.md §8](collation.md)), whose
`memcmp` order **is** the collation's logical order by construction:

```
sort_key = L1-weights ‖ 0x0000 ‖ L2-weights ‖ 0x0000 ‖ L3-weights ‖ 0x0000 ‖ Ckey(original)
```

This is **not a new key type** — `text` was already a key (§2.4) — but a per-column *form* selected
by the column's frozen collation ([collation.md §1](collation.md)): a `C` text key uses §2.4
verbatim (the unchanged fast path, zero collation data), a non-`C` text key uses the sort key. The
executor reuses the *same* sort key the comparison/`ORDER BY` evaluator already emits (slice 1b), so
one routine drives ordering everywhere.

- **Self-delimiting + total + reversible — for free.** The sort key **appends the original string's
  `C`-key** (the §2.4 `text-terminated-escape`) as its identical level. That trailer does three jobs
  at once: it is the **identical-level tie-break** (totality — so a deterministic collation's
  equality is byte-identity, [collation.md §6/§7](collation.md)); it makes the key **self-delimiting**
  (it ends in the §2.4 terminator `0x00 0x01`, so the key composes in a composite key / index suffix
  exactly like a plain text key); and it makes the original **recoverable from the key** — required
  for a `PRIMARY KEY`. The level separators are `0x0000` and every emitted weight is `≥ 0x0001`, so a
  level that is a prefix of another's sorts first (the `"a" < "ab"` behaviour at every level,
  [../collation/README.md §4](../collation/README.md)).
- **One uniform component encoding.** jed encodes a collated text key component as the **full** sort
  key (identical level included) **everywhere** — PK body, secondary-index entry, `UNIQUE` probe
  prefix. A secondary index could store `sort_key ‖ pk` *without* the identical level (the row is
  fetched via the PK, [collation.md §8](collation.md)); jed keeps the trailer there too, so the
  storage-key suffix only refines a genuine collation tie. The small redundancy (the trailer plus the
  appended storage key) buys a single component codec and zero special-casing.
- **Descending / nullable** reuse the existing whole-component rules unchanged: a descending collated
  key is the §2.3 bitwise inversion of the whole sort key (trailer included), and an index /
  composite member wraps the sort key in the §2.2 nullable slot (`0x00` present ‖ sort key, or `0x01`
  NULL).

Worked body for `"a"` under the dev-root collation ([../collation/README.md §5](../collation/README.md)),
the value pinned in [../collation/vectors/sortkey.toml](../collation/vectors/sortkey.toml):

```
1C47   ‖ 0000 ‖ 0020 ‖ 0000 ‖ 0002 ‖ 0000 ‖ 61 00 01
(L1: a)  (sep)  (L2)   (sep)  (L3)   (sep)  (identical: Ckey "a")
```

= `1C47 0000 0020 0000 0002 0000 61 00 01`. `"A"` differs only at L3 (`0008`) and the trailer
(`41 00 01`), so it sorts immediately after `"a"` — the deterministic "adjacent, not equal" property,
now realised in *stored* order.

**Status — EXERCISED (slice 1e).** A non-`C` collated `text` column **is** a valid `PRIMARY KEY` /
ordered secondary index / `UNIQUE` key — the keys store sort-key bytes, so the B-tree iterates in
collation order with no runtime comparator. The collation table is **baked** into the file (slice 1d,
[collation.md §3](collation.md)), so the key bytes are self-contained and cross-core byte-identical
(`rust == go == ts == ruby`); the on-disk image is pinned by the `collation_pk_table.jed` golden
([../fileformat/format.md](../fileformat/format.md)) and the key body bytes by
[../collation/vectors/sortkey.toml](../collation/vectors/sortkey.toml). Two key-path notes,
documented in [collation.md §8/§14](collation.md): (a) **collated-key pushdown is a skew-aware bound
(✅ landed)** — a collated PK/index `WHERE k = …` / `k < …` pushes down by encoding the probe as the
column collation's UCA sort key (the stored key form), so it seeks/range-scans exactly as a `C` key
does (equality sound via the injective identical level, ordering via the sort key's `memcmp` order),
gated on the comparison's collation MATCHING the key column's frozen collation and on the collation
being non-skewed (a *version-skewed* index is never seeked — the read-safety rule, collation.md §12;
the slice-1e key path originally deferred this, contributing no bound, which is what
`suites/collation/skew.test` guards); (b) **a collated key value whose UCA sort
key would exceed a node** trips the existing over-`RECORD_MAX` oversized-item `0A000` (the sort key is
~2–3× the source, so the cap bites sooner than for a `C` key — the documented price of one `memcmp`
order). An unmapped code point under the (dev) collation fails the sort-key build (`0A000`) at the
write, the same point and code the comparison path raises ([collation.md §6](collation.md)). Stored
text *values* are unaffected — they still use the compact length-prefixed value codec
([../fileformat/format.md](../fileformat/format.md)); only the *key* takes the collated form, the §3
key/value seam diverging exactly as it does for `decimal` (§2.5) and `interval` (§2.10).

### 2.13 `jsonb` — `jsonb-order-preserving` (authored; unexercised — deferred follow-on)

`jsonb` has a total btree order ([json.md §5](json.md)) but is **not** a key type in the first
JSON slices (`0A000`, the staged-key narrowing text/decimal/bytea/array all carried). The
order-preserving encoding is authored here ahead of use — the float-§2.8 precedent — and a
follow-on slice exercises it ([json.md §12](json.md)). The rule recurses over the node tree,
mirroring the §2.11 range-bounds container recursion:

1. A leading **type-rank discriminator byte** encoding the §5 order `Null < String < Number <
   Boolean < Array < Object` (one byte, ascending, so the rank dominates the sort exactly as the
   range empty/±∞ markers dominate §2.11).
2. Then the per-kind body, each in its **own order-preserving, self-delimiting** form:
   `null`/`false`/`true` carry no body (the rank byte suffices, with `false`/`true` split into
   two rank values); a **string** uses the `text-terminated-escape` rule (§2.4); a **number**
   uses `decimal-order-preserving` (§2.5); an **array** frames its element count then each
   element body recursively; an **object** frames its member count then, in canonical key order
   ([json.md §2.3](json.md)), each key (`text-terminated-escape`) then value body recursively.
   Count-first framing reproduces the §5 "fewer elements/members sort first" rule.

This composes with the §2.2 nullable slot and §2.3 descending inversion unchanged. **Status —
AUTHORED, UNEXERCISED.** Like float (§2.8) it is written but not yet a live key; `json` (never
comparable) and `jsonpath` get no key rule at all.

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
keys (the `…-terminated-escape` rules §2.4/§2.6), `decimal` keys (the
`decimal-order-preserving` rule §2.5, `decimal_pk_table.jed`), `interval` keys (the
`interval-span-i128` span rule §2.10, `interval_pk_table.jed`), and `range` keys (the recursive
`range-bounds` container rule §2.11, `range_pk_table.jed` — the first *container* key). A **non-`C`
collated `text` key** (the `text-collated-sortkey` *form* §2.12) has since landed too — the same
`text` key type, but its body is the column collation's baked UCA sort key rather than the raw UTF-8,
pinned by the `collation_pk_table.jed` golden. The lone
remaining non-integer scalar, `float`, adds its own §2 key path only if the determinism carve-out
(§2.8) ever lifts; the recursive `composite`/`array` containers stay `0A000` keys.

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
