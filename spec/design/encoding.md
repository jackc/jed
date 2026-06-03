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

### 2.3 Composition and descending

- **Composite keys** are the **concatenation** of their components' encodings, left to
  right. Each component is either fixed-width (the integer types) or self-delimiting, so the
  concatenation stays order-preserving without separators.
- **Descending order** is the **bitwise inversion (one's complement)** of a component,
  *tag byte included*. Inverting every byte reverses `memcmp` order exactly, so a descending
  component sorts as the mirror of its ascending form. Under inversion the nullable tag
  flips `0x00 ↔ 0xFF` and `0x01 ↔ 0xFE`, so **NULL (`0xFE`) sorts before every present
  value (`0xFF…`)** — i.e. descending lifts NULL to **first**, the exact mirror of §2.2.

## 3. Where this is used today

The bare integer rule is exercised by every stored key. The on-disk **value codec**
([../fileformat/format.md](../fileformat/format.md)) reuses the §2.2 nullable encoding to
serialize each row value (the tag marks NULL); for a stored *value* the tag's sort order is
irrelevant, but reusing one codec keeps key and value bytes consistent and is what lets the
seam diverge cleanly if a future type ever needs distinct key/value forms. Composite keys
and the non-integer scalars (`decimal`, `text`, `bytea`, …) will add their own §2 rules and
fixtures when those features land; nullable *secondary indexes* — the first place §2.2's
sort order becomes load-bearing rather than spec-only — follow then too.

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
