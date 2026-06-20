# Range types — design

> The six built-in PostgreSQL range types: a **structural** range over a scalar element
> (subtype) — `i32range`, `i64range`, `numrange`, `tsrange`, `tstzrange`, `daterange` —
> constructed with the `'[1,5)'` literal or the `i32range(lo, hi)` constructor, with
> inclusive/exclusive and unbounded endpoints and a distinguished **empty** value; compared
> by PostgreSQL's range btree order; stored compactly; rendered/parsed as `[lo,hi)` text.
> Ranges are the **third container axis** after composite (nominal) and array (structural),
> and reuse the array foundation: the open `Type`, the recursive value codec's `tag-byte +
> body` split, the polymorphic `anyrange`/`anyelement` resolution, and the
> authored-but-rejected-as-key narrowing. This doc is the contract all three cores implement
> in lockstep (CLAUDE.md §2); the type set is data in [../types/ranges.toml](../types/ranges.toml)
> (codegen'd to the per-core `RANGES` table), the grammar in
> [../grammar/grammar.ebnf](../grammar/grammar.ebnf) + [grammar.md](grammar.md), the byte
> layout in [../fileformat/format.md](../fileformat/format.md) (`format_version` 15), the
> operator/function surface in [range-functions.md](range-functions.md), and the container
> framing in [array.md](array.md) + [extensibility.md §4.1](extensibility.md). PostgreSQL
> semantics are the default (CLAUDE.md §1) and are pinned against the live `postgres:18`
> oracle — several canonicalization / empty / ordering / text-quoting rules are subtle (§4–§7).

> **Status: R0–R3 landed (the type axis is complete) AND the full function/operator surface landed —
> RF1 (accessors), RF2 (constructors), RF3 (boolean operators), RF4 (set operators + `range_merge`)
> are all in (§11). Range types are feature-complete.** R0 (this doc +
> `ranges.toml` + the codegen'd `RANGES` table) is the spec/data
> foundation; **R1** threaded the open-`Type` `Range` arm + the `'[1,5)'::i32range` literal/cast
> through all three cores; **R2** made range **columns** declarable + storable — the value codec
> (`type_code 17`, `format_version 16`), canonicalization / empty normalization at store, text I/O,
> and the cross-core golden `range_table.jed` (`rust == go == ts == ruby`); **R3** added the
> `range_cmp` total order (§6) driving `=` / `<` / `<=` / `>` / `>=` / `ORDER BY` / `DISTINCT` /
> `GROUP BY` — a range compares only with a range over the **same** element type (a cross-element
> pair is `42804`). The naming, the structural-type decision, the value model, canonicalization,
> text I/O, comparison, and the deferred narrowings below are ratified spec-first; the per-slice
> delivery is §11.

## 1. Surface

```sql
CREATE TABLE reservations (id i32 PRIMARY KEY, during tsrange, span i32range)
INSERT INTO reservations VALUES (1, '[2020-01-01 09:00,2020-01-01 17:00)', '[1,10)')
INSERT INTO reservations VALUES (2, tsrange('2020-02-01 00:00', '2020-03-01 00:00'), int4range(5, 15))
SELECT id, span, lower(span), upper(span) FROM reservations ORDER BY span
SELECT * FROM reservations WHERE span @> 7 AND during && tsrange('2020-01-01','2020-02-15')
```

- **Six types, jed-spelled, PG-aliased.** Canonical names `i32range`, `i64range`,
  `numrange`, `tsrange`, `tstzrange`, `daterange`. PostgreSQL's `int4range`/`int8range` are
  **aliases** of `i32range`/`i64range` (the `i`-prefix keeps jed's bit-namespace disjoint from
  PG's byte-namespace, so `int8range` aliases `i64range` with no `i8range` collision — the
  reason the i/f-prefix rename was done, CLAUDE.md §4). The other four keep PG's names verbatim.
- **One range per element; no others.** A range type exists for exactly the six element
  (sub)types in [../types/ranges.toml](../types/ranges.toml) — `i32`, `i64`, `decimal`,
  `timestamp`, `timestamptz`, `date` — the same set PostgreSQL ships built-in ranges for.
  There is no `textrange`/`booleanrange`/etc.; such a name is `42704` (undefined type). A
  range over a range, or over an array/composite, does not exist.
- **Three ways to make one.** The text literal coerced by a named type
  (`'[1,5)'::i32range`, `i32range '[1,5)'` — landed R1/R2); the constructor functions
  `i32range(lo, hi)` / `i32range(lo, hi, '[]')` (RF2); and operator results (`+`/`*`/`-`,
  `range_merge` — RF4). The literal cast alone makes the foundation (R0–R3) testable; no
  constructor is needed to declare/store/compare a range.
- **Endpoints: inclusive/exclusive and unbounded.** `[` / `]` = inclusive, `(` / `)` =
  exclusive; an omitted bound (`[1,)`, `(,5]`, `(,)`) is **unbounded** (infinite) on that
  side. An unbounded bound is always exclusive (PG). `empty` is a distinguished, non-NULL
  value (a range containing no points) — distinct from a NULL range.
- **Comparison** `= <> < <= > >=` is PostgreSQL's total range btree order (§6): `empty`
  sorts below every non-empty range, then by lower bound, lower inclusivity, upper bound,
  upper inclusivity. `IS NULL` tests only the whole value.
- **Declarable + storable from R2; keyable since the range-PK slice.** A range is a valid
  `PRIMARY KEY` / ordered (btree) secondary index / `UNIQUE` key / FK target — its
  order-preserving `range-bounds` key ([encoding.md §2.11](encoding.md)) mirrors the §6 total
  order (the first *container* key). A range column `DEFAULT` stays `0A000` (the array narrowing,
  §8), and point-lookup pushdown is **deferred** (a range PK/index `WHERE k = …` full-scans +
  residual-filters); a PG-style GiST/SP-GiST range index is a deferred follow-on (§10).

## 2. The open type system — `Type { Scalar | Composite | Array | Range }`

Composite opened the type system, array added a structural arm; range adds a **fourth** arm,
also structural:

- `Type = Scalar(ScalarType) | Composite(CompositeRef) | Array(Box<Type>) | Range(Box<Type>)`.
  (Rust `enum Type` gains `Range(Box<Type>)`; Go a `Range *Type` field on the tagged `Type`;
  TS a `{kind:"range", elem:Type}` union arm.)
- `Range` carries its **element `Type` inline** (boxed/owned), *not* a catalog reference —
  structural like array: two `i32range` are the same type because their elements are equal.
  This matches PostgreSQL observably (PG materializes a companion range type per subtype, but
  range-type identity is a bijection on `(subtype, opclass)` and jed ships one opclass per
  subtype, so a structural representation computes the identical identity; the materialization
  is an implementation detail jed need not mirror).
- The element is constrained to the **six scalar subtypes** in `RANGES` (§1) — *not*
  `Composite`, `Array`, or another `Range`. The element `Type` resolved for a column lives in
  the column's catalog entry (§3); the `ScalarType` accessors never receive a range.
- A range **value** is `Value::Range(RangeVal)` (Rust) / a `ValRange` kind holding a pointer
  so the flat Go `Value` stays `==`-comparable — range equality/hashing forced through the
  structural path, never raw `==` (the rule `Decimal`/`Composite`/`Array` already follow) / a
  `{kind:"range", …}` TS arm. The value model is §4.

A `Type::Range(elem)` resolves its element-derived facts — canonical name, discreteness,
codec, comparator — from the `RANGES` descriptor keyed by the element scalar id, never by
re-deriving per call.

## 3. Catalog & on-disk format (`format_version` 15)

Ranges need **no new catalog section** (the structural payoff, like array): a range column
encodes its element type **inline** in its catalog column entry. The on-disk shape (full byte
layout in [../fileformat/format.md](../fileformat/format.md), landed R2):

- **A new `type_code = 17` ("range").** Codes 1–13 are scalars, 14 composite, 15 array, 16
  `date`; **17** is the next free. The full column entry is `type_code = 17 ‖ flags ‖
  element_type_code` — the `type_code`, then the **column flags byte** (bit1 `not_null`; bits
  0/2/3 reserved 0, since a range column carries no `DEFAULT` this slice — §8), then a single
  `u8 element_type_code` (one of the six scalar codes: 2, 3, 6, 9, 10, 16). **No typmod is
  stored even when the element is `decimal`**: `numrange`'s element is the *unconstrained*
  `decimal` (there is no `numrange(p,s)`), and the type-code alone fully determines which of the
  six ranges the column is — so the descriptor is self-describing with the element code only,
  the same inline-descriptor framing an array column uses one level down. No persisted
  range-type entry, no range-type id; a range file is self-describing.
- **`format_version` 16** is a clean break (the v16 reader rejects v15 and earlier, as
  v9/v10/v13 did). All existing `.jed` goldens regenerate at the bump (the version byte);
  range columns appear only in new goldens.

## 4. Value model — bounds, flags, canonicalization, empty

A range value is one of:

- **empty** — the range containing no points (a non-NULL value), or
- **non-empty** — `{ lower: Option<elem>, upper: Option<elem>, lower_inc: bool, upper_inc:
  bool }`, where a `None` bound is **unbounded/infinite** on that side. An infinite bound's
  inclusivity flag is always **false** (PG; an open infinity).

The on-disk **value body** (after the shared `0x00` present / `0x01` whole-value-NULL
presence tag) is a single flags byte + the present bound bodies:

```
flags u8:
   bit 0  RANGE_EMPTY   1 = the empty range; when set, NO further bytes follow
   bit 1  LB_INF        lower bound is unbounded (infinite)
   bit 2  UB_INF        upper bound is unbounded (infinite)
   bit 3  LB_INC        lower bound is inclusive
   bit 4  UB_INC        upper bound is inclusive
   bits 5–7 reserved, must be 0
lower bound body   present ONLY if !RANGE_EMPTY && !LB_INF: the element value-codec BODY
                   (no presence tag — the same tag-byte+body split array/composite use)
upper bound body   present ONLY if !RANGE_EMPTY && !UB_INF: the element value-codec BODY
```

- A **fixed-width** element bound (`i32`/`i64`/`timestamp`/`timestamptz`/`date`) is its raw
  fixed-width body, no prefix; a **variable-width** element (`decimal`) carries only its own
  intrinsic framing — the element's codec, not range overhead. An **empty** range is the lone
  flags byte `0x01`; a **whole-value-NULL** range is the lone `0x01` presence tag.
- **Stored form is canonical** (so the golden bytes and the comparator agree). Two rules,
  applied at construction / parse / cast, **before** the value is stored or compared:

  1. **Range order check.** If both bounds are finite and `lower > upper` → **`22000`**
     "range lower bound must be less than or equal to range upper bound" (oracle-pinned).

  2. **Discrete canonicalization** (`i32range`/`i64range`/`daterange`; `discrete = true` in
     `RANGES`). Convert to the canonical `[)` form: an inclusive-exclusive normalization that
     at the representation level is just "step the element's underlying integer by ±1":
     - lower `(x` (exclusive) → `[x+1` (inclusive); lower `[x` unchanged.
     - upper `x]` (inclusive) → `x+1)` (exclusive); upper `x)` unchanged.
     - `date` is i32 days, so its +1 day **is** +1 on the stored int — the step is uniform
       across all three discrete subtypes.
     - A step past the element domain (e.g. `int4range(1, 2147483647, '[]')` → upper 2³¹) →
       **`22003`** "integer out of range" (oracle-pinned, "integer out of range" message).
     - Infinite bounds are not stepped (they stay `LB_INF`/`UB_INF`).
     Continuous subtypes (`numrange`/`tsrange`/`tstzrange`; `discrete = false`) keep their
     bounds and inclusivity **verbatim** — `numrange(1.0,2.0,'[]')` stays `[1.0,2.0]`.

  3. **Empty normalization** (applied after canonicalization). A range whose canonical bounds
     are finite and **equal with not-both-inclusive** collapses to `empty`:
     `int4range(5,5)` (default `[)`) → `empty`; `int4range(5,5,'(]')` → `empty`;
     `numrange(1,1)` → `empty`; but `int4range(5,5,'[]')` → `[5,6)` (singleton) and
     `numrange(1,1,'[]')` → `[1,1]` (singleton). For discrete ranges this falls out of
     canonicalization (a one-point `(x,x)`/`[x,x)`/`(x,x]` canonicalizes to `lower == upper`
     exclusive → empty); for continuous it is the explicit equal-bounds-not-both-inclusive
     test.

Worked bytes, `i32range '[1,5)'` (already canonical `[)`, both finite): presence `00`, flags
`0b0_1000 = 0x08` (LB_INC set; LB_INF/UB_INF/UB_INC/EMPTY clear), then the `i32` body of `1`
(`80 00 00 01`, sign-flipped BE) and of `5` (`80 00 00 05`). Empty `i32range`: presence `00`,
flags `0x01`.

## 5. Text I/O — `range_in` / `range_out`

`range_out` (render) and `range_in` (parse) match PostgreSQL (oracle-pinned):

- **Output.** `empty` for the empty range. Otherwise `‹lb›‹lower›,‹upper›‹ub›` where `‹lb›`
  is `[` (inclusive) or `(` (exclusive/infinite), `‹ub›` is `]` or `)`, and `‹lower›`/`‹upper›`
  are the element's text rendering — **omitted** for an infinite bound: `(,5)`, `[1,)`, `(,)`.
  A bound whose element text contains a special char (`,` `[` `]` `(` `)` `"` `\`, whitespace,
  or empty) is **double-quoted** with `"` doubled and `\` escaped — so a `tsrange` bound
  (a timestamp has a space) renders quoted: `["2020-01-01 00:00:00","2020-02-01 12:30:00")`,
  while `daterange` bounds (no special char) are bare: `[2020-01-01,2020-02-02)`.
- **Input.** Optional leading/trailing whitespace; `empty` (case-insensitive) is the empty
  range; otherwise a `[`/`(`, the lower text (possibly quoted/empty-for-infinite), a `,`, the
  upper text, a `]`/`)`. Each bound text is fed to the **element type's** own input function
  (so `'[1,5)'::i32range` parses `1`/`5` as `i32`, `'["2020-01-01",...]'::tsrange` parses the
  quoted timestamp). A malformed range literal (bad brackets, missing comma, element-parse
  failure) is **`22P02`** invalid_text_representation. After parsing, the canonicalization /
  order-check / empty rules of §4 apply.

## 6. Comparison & ordering — `range_cmp`

Range comparison is a **total order** (PostgreSQL range btree, `range_cmp`), so ranges
compare/order/dedup like any scalar (and unlike composite, it is never 3-valued — a definite
result always):

- `empty` is **less than** every non-empty range (and equal only to `empty`). Oracle-pinned:
  `'empty'::int4range < int4range(1,5)` is true.
- Two non-empty ranges compare by **lower bound first, then upper bound**, where each bound
  comparison accounts for infinity and inclusivity: an infinite lower bound is below any
  finite lower; for equal lower *values*, an **inclusive** lower bound sorts before an
  **exclusive** one (`[1` < `(1`); symmetrically for the upper bound an inclusive upper sorts
  *after* an exclusive one, and an infinite upper is above any finite upper. (The exact
  bound-with-inclusivity ranking is oracle-pinned, with `eq3`/`lt3`/`gt3` carrying the
  `Value::Range` arm — equality is structural over the canonical form, ordering is `range_cmp` —
  and the resolver's `classify_comparable` accepting a same-element range pair only; a range
  `ORDER BY` / `DISTINCT` / `GROUP BY` rides the same total order.)
- Because discrete ranges are stored canonical (§4), `[1,5)` and `[1,4]` over `i32range` are
  **equal** (both canonicalize to `[1,5)`) — equality is on the canonical form, oracle-pinned
  (`int4range(1,5,'[]') = int4range(1,6,'[)')` is true).
- A range is comparable only to a range over the **same element type**; `i32range × i64range`
  and `i32range × i32` are `42804` (no implicit cross-type range comparison this slice).

## 7. NULL semantics

- A **NULL range** (the whole value is SQL NULL), an **empty range** (`empty`, a non-NULL
  value), and a range with infinite bounds (`(,)`) are three distinct things. `IS NULL` /
  `IS NOT NULL` test only the **whole value** (like array, unlike composite's all-fields 3VL):
  `empty IS NULL` and `'(,)' IS NULL` are both FALSE.
- The constructors and operators are **strict** (NULL propagates) on the *range* arguments;
  a NULL element bound passed to a constructor means **infinite** on that side, not NULL — PG:
  `int4range(NULL, 5)` is `(,5)`, not NULL (range-functions.md RF2 pins this).

## 8. A key (since the range-PK slice), no DEFAULT — staged narrowings

A range **is** a valid `PRIMARY KEY` / ordered (btree) secondary index / `UNIQUE` key / FK
target via its order-preserving `range-bounds` key ([encoding.md §2.11](encoding.md)) — the
first *container* key, recursing into the element codec with empty/±∞/inclusivity framing that
mirrors the §6 total order. Two narrowings remain (the array narrowings verbatim, CLAUDE.md §4;
array.md §1/§8): a range-typed column `DEFAULT` is still **`0A000`**, and a **GIN** index over a
range column is **`0A000`** (a range is not a GIN element — that is a GiST job, §10). Two
deliberate scopings on the *key*, both relaxable: **point-lookup pushdown is deferred** (a range
PK/index `WHERE k = …` is correct via full-scan + residual filter, just unindexed — the
container precedent), and a PG-style **GiST/SP-GiST** range index (accelerating `@>`/`&&`/…)
is a deferred follow-on (§10).

## 9. Determinism

Ranges are exact and deterministic. `numrange` is over the exact `decimal` (no float); the
discrete subtypes are integer/day counts; the timestamp subtypes are i64 µs. The stored form
is canonical, the text render is canonical, and `range_cmp` is a total order — so the value
multiset, render, comparison, and on-disk bytes are byte-identical cross-core. No new
determinism exception (CLAUDE.md §10).

## 10. Deferred follow-ons (none foreclosed)

- **Range indexing (GiST) + key-bounded point lookup.** A GiST/SP-GiST-style range index
  accelerating `@>` / `&&` / `<<` / `>>` / `-|-`, and point-lookup pushdown for a range
  PK/ordered-index `WHERE k = …` (the ordered `range-bounds` key already sorts correctly — the
  scoping is the planner side only, [encoding.md §2.11](encoding.md)). PG uses GiST (not the GIN
  `array_ops` jed shipped for arrays); this is its own slice with its own NoREC obligation. Until
  then range operators and a range equality `WHERE` are full-scan predicates, and cannot mis-fire
  the existing integer-array GIN `gin_match` planner (a range operand never matches a GIN index).
- **Multirange types** (`int4multirange` etc., PG 14+) — a separate axis, not scheduled.
- **Custom range types** via `CREATE TYPE … AS RANGE` — jed ships only the six built-ins;
  the structural model would need a nominal escape hatch, deferred.
- **The `@>`/`<@` with the element on the left** beyond what RF3 ships, `range_agg`,
  `int4range`-style casts from/to the element, and `range` subscripting — not this surface.

## 11. Slice delivery

The type axis (R0–R3) lands first and is independently useful/testable (CLAUDE.md §10); the
function/operator surface (RF1–RF4, [range-functions.md](range-functions.md)) builds on it.

| Slice | Content |
|---|---|
| **R0** | this doc + `ranges.toml` + the codegen'd `RANGES` table + `type_code 17`/`format_version 16` reservation + grammar/CLAUDE notes |
| **R1** | the open-`Type` `Range` arm threaded through parser/resolver/evaluator + type-name parse (`i32range` + aliases) + element restriction (`42704`); `'[1,5)'::i32range` resolves & renders |
| **R2** | declarable + storable range **column** + `type_code 17` value codec + canonicalization/empty (§4) + text I/O (§5) + the `range_table.jed` golden (`rust == go == ts == ruby`) |
| **R3** | comparison / ordering / DISTINCT / GROUP BY / `IS NULL` via `range_cmp` (§6) |
| **RF1** | `anyrange`/`anyelement` polymorphism + accessors (`lower`/`upper`/`isempty`/`lower_inc`/`upper_inc`/`lower_inf`/`upper_inf`) |
| **RF2** | the six constructors (`i32range(lo,hi)` / `(lo,hi,text)`, …) |
| **RF3** | boolean operators `@>` `<@` `&&` `<<` `>>` `&<` `&>` `-\|-` (+ the new lexer tokens) |
| **RF4** | set operators `+` `*` `-` + `range_merge` (reuse the arithmetic tokens; `+`/`-` raise `22000` on a non-contiguous result) — **range surface complete** |
| **R4** | range as a **key**: the order-preserving `range-bounds` encoding ([encoding.md §2.11](encoding.md)) + lift the PK/index/`UNIQUE`/FK gate in all three cores + `range.toml` encoding vectors + the `range_pk_table.jed` golden (`rust == go == ts == ruby`). Point-lookup pushdown + GiST deferred (§10) — **the first container key** |

## 12. Divergence ledger (jed ≠ PostgreSQL)

- **Type names.** Canonical `i32range`/`i64range` (PG `int4range`/`int8range` are aliases);
  `numrange`/`tsrange`/`tstzrange`/`daterange` match PG. A `query` column-type tag and
  `pg_typeof`-style report read `i32range`, not `int4range` (one canonical name per type).
- **Element domains.** `daterange`/`tsrange` inherit jed's **wider** `date`/`timestamp`
  domains (date.md / timestamp.md), so a range over a date PG would reject is accepted — the
  same documented divergence the element types already carry, not a range-specific one.
- **No GiST / range index, no key-bounded point lookup** (§8, §10) — a relaxable narrowing, not
  a permanent divergence. A range **is** a key (PK/ordered-index/`UNIQUE`/FK) since R4, but a
  range equality `WHERE` full-scans + residual-filters rather than seeking, and PG's GiST range
  index has no jed equivalent yet.
- **Strict static element.** No implicit cross-element range comparison/cast (§6) — PG also
  has no implicit `int4range`↔`numrange` cast, so this matches PG; listed for completeness.
