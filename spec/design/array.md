# Array types — design

> `T[]`: a **structural** array type over any existing element type — constructed with
> `ARRAY[…]` or the `'{…}'` literal, subscripted `a[i]` (1-based), compared element-wise,
> stored compactly, and rendered/parsed as `{…}` text. Arrays are the **second container
> axis** after composite (row) types and reuse ~80% of that foundation (the open `Type`, the
> recursive value codec, element-wise comparison, the large-values spill path). This doc is the
> contract all three cores implement in lockstep (CLAUDE.md §2); the grammar is in
> [../grammar/grammar.ebnf](../grammar/grammar.ebnf) + [grammar.md](grammar.md), the byte layout
> in [../fileformat/format.md](../fileformat/format.md) (`format_version` 10), the key encoding
> in [encoding.md §2.11](encoding.md), the cost contract in [cost.md §3](cost.md), and the
> container framing in [composite.md](composite.md) + [extensibility.md §4.1](extensibility.md).
> PostgreSQL semantics are the default (CLAUDE.md §1) and must be pinned against the live
> `postgres:18` oracle — several array NULL/comparison rules are subtle (§5, §6).
>
> **Status: S0–S4 landed.** Declarable + storable `T[]` columns (scalar elements), the `ARRAY[…]`
> constructor and `'{…}'`/`::` literal, `array_out`, the compact value codec, subscripting `a[i]`,
> and btree-NULL comparison / ordering / DISTINCT / whole-value `IS NULL` are implemented across all
> three cores at **`format_version` 10** with byte-identical goldens (`rust == go == ts == ruby` —
> `array_table.jed`) and oracle-checked conformance suites (`types/array.test`, `types/subscript.test`).
> The decisions in §10 are ratified spec-first; the per-slice delivery is §12. **Remaining:** the §12
> `0A000` follow-ons (multidim values, custom lower bounds, slices `a[m:n]`, array-of-composite,
> arrays-in-keys, and the array function/operator surface).

Arrays are the **second user-facing container type** and the axis the composite (row) type
already cleared the way for. Where composite added a *nominal* arm to the open type system
(a `CREATE TYPE`-named, catalog-resident `Composite(ref)`), an array is a **structural** type
constructor — `int32[]` exists for every element type with no DDL, derived on demand, the
element type carried inline. The cross-core contract is the same one composite established: not
"the data table is byte-identical" (scalars) but "the **recursive** codec / comparator /
NULL-rule / text-I/O is byte-identical," hand-written per core (CLAUDE.md §5 forbids
codegenning it), policed by golden fixtures + corpus entries (CLAUDE.md §8). Because every
method is **derived** from an element type that is already cross-core-identical, that
byte-identity holds *by construction*, and an array value is **self-describing and portable**
(its element type lives in the column's catalog type, so any jed can read a file containing it).

## 1. Surface

```sql
CREATE TABLE t (id int32 PRIMARY KEY, xs int32[], tags text[])
INSERT INTO t VALUES (1, ARRAY[10, 20, 30], ARRAY['a', 'b'])
INSERT INTO t VALUES (2, '{40,50}', '{}')
SELECT id, xs, xs[1], tags FROM t ORDER BY xs
SELECT * FROM t WHERE xs = ARRAY[40, 50]
```

- **Structural type, dimension-agnostic.** `int32[]` is "array of `int32`" — there is no DDL to
  create it and no catalog entry for it; it exists the moment its element type does (§2). Matching
  PostgreSQL exactly (CLAUDE.md §1), **array shape (number of dimensions, per-dimension lengths,
  lower bounds) is a property of the *value*, not the type**: the same `int32[]` column may hold a
  3-element array in one row and (eventually) a 2×2 array in another, and a declared size like
  `int32[3]` enforces nothing. This is the one place arrays relax "strict static" (CLAUDE.md §4) —
  but only on *shape*; the **element type stays static and strictly enforced** (an `int32[]` never
  holds a `text` element), which is the part of §4 that matters. See §10.1.
- **Element type** is any *existing* type — a built-in scalar or a previously-defined composite —
  **except another array** (PG-faithful: multidimensionality is a value property, *not*
  array-of-array; §2). An unknown element type is `42704`.
- **Type-name spellings.** The canonical spelling is **`T[]`** (the element's canonical name +
  `[]`). PostgreSQL's other spellings are accepted and **normalize** to it, their size/dimension
  decorations being documentation-only (PG-faithful): `T[n]` (the `n` ignored), `T[][]` (a
  multidim declaration — same type), and the SQL-standard `T ARRAY` / `T ARRAY[n]`. The resolved
  type always **renders** and reports (`# types:`) as `T[]` regardless of any value's
  dimensionality — one canonical name per type (determinism, [types.md §2](types.md)).
- **Two constructors, both PG.** `ARRAY[e1, e2, …]` (the expression constructor — elements are
  arbitrary expressions, unified to one element type) and the `'{…}'` text literal coerced by a
  named type (`'{1,2,3}'::int32[]`, `int32[] '{1,2,3}'`). `ARRAY[]::int32[]` is the empty array;
  `'{}'` is the empty array (zero elements, `ndim = 0` — §4).
- **Subscripting** `a[i]` reads the *i*-th element, **1-based**, returning the element type;
  an out-of-bounds or NULL subscript yields **NULL**, never an error (PG; §6).
- **NULL at three levels, all distinct** (PG): a **NULL array** (the whole value is SQL-NULL), an
  **empty array** `{}` (a non-NULL array of zero elements), and an array with **NULL elements**
  (`{1,NULL,3}` — a non-NULL array; `IS NULL` is FALSE).
- **Comparison** `= <> < <= > >=` is element-wise (PG `array_cmp` / `array_eq`), with **btree
  semantics for NULL elements — NOT the 3VL rule composite uses** (§5). `IS NULL` / `IS NOT NULL`
  test only the whole value, **not** element-wise (the contrast with composite's all-fields rule;
  §5).
- **Arrays are declarable and storable from S2; never keyable this slice.** An array `PRIMARY
  KEY` / index / `UNIQUE` column is rejected `0A000` (§8) — the order-preserving key encoding is
  authored ([encoding.md §2.11](encoding.md)) but not exercised, the same staged narrowing
  text/decimal/bytea/composite keys carry.

## 2. The open type system — `Type { Scalar | Composite | Array }`

Composite opened the type system; arrays add the **third arm**, and a **structural** one:

- `Type = Scalar(ScalarType) | Composite(CompositeRef) | Array(Box<Type>)`.
- `Array` carries its **element `Type` inline** (a boxed/owned recursive `Type`), *not* a catalog
  reference. This is the structural↔nominal split decided in design: composite is nominal (two
  same-shaped composites are distinct types, identified by catalog id+name), arrays are structural
  (two `int32[]` are the same type because their element types are equal). It matches PostgreSQL
  observably — PG materializes a companion array type per element (`typarray`/`_int4`), but array
  type identity in PG is a **bijection on the element type** (exactly one array type per element,
  no surface to name or distinguish a second), so a structural representation computes the
  identical identity; the materialization is an implementation detail jed need not mirror (§10.2).
  (Rust `enum Type` gains `Array(Box<Type>)`; Go a tagged `Type` with an `Elem *Type`; TS a
  `{kind:"array", elem:Type}` union arm.)
- The element type is constrained to **`Scalar | Composite`** — *not* `Array` — because
  multidimensionality is a value property (§4), not array-of-array. `int32[][]` parses to
  `Array(Scalar(int32))` with a *value* `ndim` of 2, never `Array(Array(int32))`. (A future
  array-of-array, if ever wanted, would be a deliberate divergence from PG and is **not** this
  axis.)
- An array **value** is `Value::Array { ndim, dims, lbounds, elements }` (Rust) / a `ValArray`
  kind holding a pointer so the flat Go `Value` struct stays `==`-comparable — array equality and
  hashing forced through the structural path, never raw `==` (the rule `Decimal`/`Composite`
  already follow) / a `{kind:"array", …}` TS arm. `elements` is the flattened row-major element
  list (length = product of `dims`); NULL elements are real `Value` NULLs of the element type.

The element `Type` resolved for a column lives in the column's catalog entry (§3); the integer-only
`ScalarType` accessors never receive an array.

## 3. Catalog & on-disk format (`format_version` 10)

Arrays need **no new catalog section** (the structural payoff — contrast composite's type-definition
chain): an array column encodes its element type **inline** in its catalog column entry, recursively.
The on-disk shape (full byte layout in [../fileformat/format.md](../fileformat/format.md)):

- **A new `type_code = 15` ("array").** Codes 1–13 are scalars, 14 is composite-by-name
  ([composite.md §3](composite.md)); **15** is the next free. An array column stores `type_code =
  15` followed by an **element type descriptor**, recursively:
  - `element_type_code u8` — `1`–`13` for a scalar element, `14` for a composite element, `15` for
    a nested-array element (**deferred `0A000` this slice** — never emitted in v1).
  - then, only when the element code needs it: the decimal typmod (`precision u16`, `scale u16`)
    when the element is `decimal` (code `6`); or `name_len u16` + name when the element is a
    composite (code `14`).
  This reuses the existing per-type catalog framing exactly — the same slots a `decimal` column's
  typmod or a `composite` column's name occupy, one level down.
- **No persisted array-type entry, no array-type id.** Because the type is structural and the
  element descriptor is inline, an array file is self-describing with no extra catalog object —
  *more* portable than composite (which persists a field list) and far simpler than PG (no
  `typarray` id stability across the byte-exact cores).
- **`format_version` 10** is a clean break (the v10 reader rejects v9 and earlier, as v5/v6/v9 did).
  All existing `.jed` goldens regenerate at the bump (the version byte; array columns appear only in
  new goldens).

## 4. Value codec — the compact body

The design target is **compact**: an array of fixed-width elements pays **zero per-element
overhead** (no length prefix, no per-element presence tag, no inline element-type tag). This falls
out of reusing composite's "split the per-type codec into `tag-byte + body`, recurse on the body"
refactor ([composite.md §4](composite.md)): an array body is a small header + a shared null bitmap
+ concatenated element **bodies**. Because the element type comes from the **schema** (§3), the
value stores no element-type tag (unlike PG's per-array `elemtype` Oid), and fixed-width element
bodies are self-delimiting by width.

An array **value body** (after the shared `0x00` present / `0x01` whole-value-NULL presence tag) is:

```
ndim   u8        dimension count; 0 = empty array; ≤ 6 (PostgreSQL MAXDIM)
flags  u8        bit 0 = HAS_NULLS; bits 1–7 reserved, must be 0
per dimension d in [0, ndim):
   len_d  u32 BE    element count along dimension d (≥ 1)
   lb_d   i32 BE    lower bound of dimension d (default 1), two's-complement
null_bitmap        ceil(N/8) bytes, present ONLY if HAS_NULLS; N = product(len_d)
element bodies     each PRESENT element's value-codec BODY, no presence tag, row-major
```

- The **null bitmap** is `ceil(N/8)` bytes, **MSB-first** (element *i*'s NULL bit is `0x80 >> (i %
  8)` of byte `i / 8`) — the same bit order composite pins. A set bit = that element is NULL and
  contributes **zero** body bytes. Unlike composite (whose field bitmap is *always* present), the
  array bitmap is **gated behind `HAS_NULLS`**: the common all-non-null array pays no bitmap bytes —
  justified because array element counts are unbounded, so the no-null fast path matters (composite
  field counts are tiny and schema-bounded).
- A **present** element is written **without its own presence tag** (the bitmap carries presence) —
  the same `tag-byte + body` split composite uses, recursing on the body half. A **fixed-width**
  element (`int*`/`uuid`/`timestamp*`/`float*`/`bool`) is its raw fixed-width body — **no prefix**.
  A **variable-width** element (`text`/`bytea`/`decimal`) carries only its *own* intrinsic length
  framing (text/bytea's `u16` length, decimal's internal structure) — the element type's framing,
  not array overhead. A composite element body is its recursive composite body ([composite.md
  §4](composite.md)).
- An **empty array** (`{}`) is `ndim = 0`, `flags = 0` → the two bytes `00 00`, no dims, no bitmap,
  no elements.
- A **whole-value-NULL** array is the lone `0x01` presence tag, no body.

Worked examples, `int32[]` (element body = the `int32` value-codec body, 4 bytes BE):

| value | bytes (body, after the present tag) |
|---|---|
| `{}` | `00`(ndim 0) `00`(flags) |
| `{1,2,3}` | `01`(ndim 1) `00`(flags) `00000003`(len) `00000001`(lb) ‖ `<b(1)><b(2)><b(3)>` |
| `{1,NULL,3}` | `01` `01`(HAS_NULLS) `00000003` `00000001` `40`(bitmap: elem 1 NULL) ‖ `<b(1)><b(3)>` |
| whole-value NULL | (no body; the value is the lone `0x01` tag) |

An `int32[]` of 3 non-null elements is **22 bytes** (vs PG's ~32+); an `int32[]` of N elements is
`10 + 4N` bytes. An array is one opaque inline body that **spills via the existing large-values
overflow + LZ4 path** ([large-values.md](large-values.md)) when it exceeds `RECORD_MAX` — and a
repetitive numeric array compresses very well. Element-*internal* per-element spill is deferred; an
over-cap array uses the existing over-`RECORD_MAX` handling.

This is the **value** codec (a stored value never needs to sort), which is *why* it can use compact,
non-order-preserving fixed-width bodies. The separate order-preserving **key** encoding (§8) is what
needs self-delimiting + terminator framing.

## 5. Comparison, ordering, and NULL — PG btree semantics, NOT composite 3VL

Array comparison is **recursive / structural**, so — like composite — it is a hand-written special
case in the value module's `eq3` / `lt3` / `gt3`, not a [../functions/catalog.toml](../functions/catalog.toml)
operator row (the catalog cannot express "recurse over N elements"; CLAUDE.md §5 forbids
codegenning it). [compare.toml](../types/compare.toml) stays scalar-only. Two array values are
comparable iff they share the **same element type**; any other pair is `42804` at resolve time
(`int32[]` vs `text[]` is not comparable, exactly as `int32` vs `text` is not).

**The load-bearing difference from composite: arrays do NOT use 3VL for NULL elements.** PostgreSQL
array comparison (`array_eq` / `array_cmp`) is built on the element type's **btree** comparison, in
which **NULL is a comparable value** (NULLs are mutually equal and sort after non-NULLs — exactly
`IS NOT DISTINCT FROM` semantics), so an array comparison **always yields a definite boolean, never
UNKNOWN**. This is the opposite of composite/row comparison ([composite.md §5](composite.md)), which
*does* propagate UNKNOWN. Implementers must **not** reuse the composite 3VL path. Pin against the
oracle (the composite `IS NULL` rule was oracle-corrected — expect the same scrutiny):

- **Equality (`=`, `<>`):** TRUE iff same dimensions *and* every element pair is equal-or-both-NULL;
  else FALSE. So (proposed, oracle-pinned): `ARRAY[1,NULL] = ARRAY[1,NULL]` → **TRUE**,
  `ARRAY[1,NULL] = ARRAY[1,2]` → **FALSE**, `ARRAY[1,2] = ARRAY[1,2,3]` → **FALSE** (length differs).
  `<>` is the boolean negation (not a 3VL negation).
- **Ordering (`< <= > >=`, and the ORDER BY / DISTINCT / GROUP BY sort key):** element-wise over the
  linear element order — the first element pair that is not "equal" decides (NULL sorts after every
  non-NULL, NULLs mutually equal); if one array is a **prefix** of the other, the shorter sorts
  first; (multidim, when it lands: `ndim` then `dims` then `lbounds` as the final tiebreak, per
  `array_cmp`). This is a **total** order, so DISTINCT/GROUP BY/ORDER BY over array columns are
  well-defined, and equal-including-NULLs arrays group together. The recursion bottoms out in the
  per-element scalar comparators (so the TS UTF-8 text-ordering trap recurses correctly for
  `text[]`).
- **`IS NULL` / `IS NOT NULL` — whole-value only, NOT element-wise** (the contrast with composite's
  all-fields rule): `arr IS NULL` is TRUE iff the array value is SQL-NULL; a non-NULL array
  containing NULL elements (`{1,NULL}`) is `IS NULL` → **FALSE**, `IS NOT NULL` → **TRUE**. An empty
  array is non-NULL → `IS NULL` FALSE. (PG; oracle-pinned.)

## 6. Subscripting and element access

`a[i]` reads the *i*-th element (S-later; see §12). PostgreSQL exactly:

- **1-based.** `a[1]` is the first element (the SQL-standard / PG convention — no overriding reason
  to diverge to 0-based, and the corpus oracle is PG; §10.3). The index domain is `1..length`; PG's
  arbitrary lower bounds are accepted in the *value* header (§4) but v1 constructs only
  default-lower-bound-1 arrays, so the in-bounds domain is `1..len` (the custom-bound read path lands
  with multidim/custom-bounds construction, §12).
- **Out-of-bounds or NULL subscript → NULL, never an error** (PG; a documented divergence from the
  SQL standard, which mandates a data exception — §10.4). `a[100]` on a 3-element array is NULL;
  `a[NULL]` is NULL. The result type is the element type.
- **Subscripting a non-array base is `42804`** (`cannot subscript a non-array`), at resolve time.
- **Slices `a[m:n]` are deferred `0A000`** (a follow-on; §12).

## 7. Text I/O — `array_out` / `array_in`

The determinism/oracle surface (like composite's `record_out`/`record_in`, [composite.md
§8](composite.md)); equals PG byte-for-byte (CLAUDE.md §8), verified by `rake corpus:check`. Reuses
the **`T`** render tag (a *rendering* tag — an array prints as a printable-ASCII string, like
bytea/uuid/composite; [conformance.md §1](conformance.md)) — **no new tag**.

- **`array_out`** renders `{e1,e2,…}` with PG's element quoting: an element is double-quoted iff it
  is the empty string, the literal token `NULL` (case-insensitive), or contains a comma, brace,
  double-quote, backslash, or whitespace. Inside quotes, `"`→`\"` and `\`→`\\` (PG `array_out`
  *backslash-escapes* — the contrast with `record_out`, which *doubles*; pin against the oracle). A
  **NULL element** renders as the unquoted token `NULL` (the contrast with `record_out`, where a
  NULL field is the empty string). The empty array renders `{}`. Recurses for composite elements.
- **`array_in`** parses `{…}` into elements (top-level commas, respecting quotes/escapes/braces) and
  coerces each token to the element type — an **unquoted** `NULL` (any case) is a NULL element,
  `"NULL"` is the 4-char text string, `\x`→`x` un-escapes. It is the inverse of `array_out` (values
  round-trip). A malformed literal is `22P02`; a bad element value surfaces that element's own parse
  error.
- An array literal is `'{1,2,3}'::int32[]` or `int32[] '{1,2,3}'` — the cast / typed-literal
  machinery routes the **string-literal → array** coercion through `array_in`, the same
  out-of-matrix path string-literal → scalar/composite coercions use (so
  [../types/casts.toml](../types/casts.toml) stays scalar-only). A bare `NULL` casts to the array; a
  same-element-type array operand is the identity cast. The **runtime** (non-literal) text→array
  cast, the `array::text` cast, and an `array → other-element-array` cast (element-wise coercion) are
  each `0A000` this slice (relaxable; §12), mirroring the deferred runtime text→scalar/composite
  casts.

## 8. Key encoding — authored, deferred

The order-preserving array key encoding is **authored** ([encoding.md §2.11](encoding.md)) but **not
exercised** this slice: an array `PRIMARY KEY` / index / `UNIQUE` column is rejected `0A000` at the
DDL resolver (the site that already rejects a text/decimal/bytea/composite key). The rule, when
lifted: each element's order-preserving encoding wrapped in the [encoding.md §2.2](encoding.md)
nullable slot, concatenated in element order, then a **terminator** so a shorter array sorts before a
longer one that extends it (the variable-length, self-delimiting composition text/bytea already use
— §2.4/§2.6 — since an array, unlike a fixed-arity composite, has a variable element count). The
narrowing is doubly forced: most element types' own key encodings are themselves still deferred.
Array **values** remain fully storable, orderable, and groupable via the in-memory structural
comparator (§5) — no key bytes required.

## 9. Cost

No new cost units this slice. `ARRAY[…]` construction and `a[i]` subscript are interior expression
nodes — each rides one `operator_eval` (like every constructor/access-shaped node). An array `=` in
`WHERE`/`ON` is one compare node → one `operator_eval`, the per-element walk internal. Sort/dedup
comparisons follow the existing unmetered-sort boundary ([cost.md §3](cost.md)). A large array
spilling through the overflow path is metered by the existing
`value_compress`/`value_decompress`/`page_read` units ([large-values.md](large-values.md),
[cost.md](cost.md)) — no array-specific unit. (A future `unnest` set-returning function adds a
`generated_row`-style unit, [functions.md §10](functions.md), when it lands.)

## 10. Ratified decisions and deliberate PostgreSQL divergences

Default is "match PostgreSQL" (CLAUDE.md §1); each divergence below has an overriding reason and is
recorded in [../conformance/oracle_overrides.toml](../conformance/oracle_overrides.toml) when its
corpus lands.

1. **Match PG semantics; array shape is a *value* property** — dimensionality, per-dimension
   lengths, and lower bounds live in the value, declared sizes (`int32[3]`) enforce nothing, a column
   holds arrays of mixed dimensionality. This relaxes "strict static" (CLAUDE.md §4) **only on
   shape**; the **element type stays static and strictly enforced**. Matching PG *is* the §1 default,
   so this is the baseline, not a ledgered divergence.
2. **Structural typing (divergence from PG's *internal* model, invisible at the SQL level)** — `T[]`
   is a derived structural type (`Array(Box<Type>)`), with no catalog object and no array-type id,
   not a materialized nominal `pg_type` row. Observably identical because array type identity is a
   bijection on the element type (§2). The cost is `pg_catalog` introspection fidelity (`_int4`
   rows), which CLAUDE.md §1 explicitly disclaims; if catalog introspection ever becomes a product
   surface, array-type rows can be **synthesized** from the bijection without changing the type
   representation. (Contrast composite, which is nominal — correctly, since same-shaped composites
   are distinct types.)
3. **1-based subscripting, custom lower bounds shed from the *index domain*** — match the SQL
   standard / PG indexing base (no overriding reason to diverge to 0-based — "preference" is excluded
   by §1, and the corpus oracle is PG); the value header still carries lower bounds (PG-faithful, §4)
   but v1 constructs lower-bound-1 arrays.
4. **Out-of-bounds subscript → NULL (match PG; a divergence from the SQL standard)** — PG returns
   NULL where the standard mandates a data exception. Matched for oracle alignment and PG
   least-surprise; the type stays sound (`a[100]` is well-typed, it just yields NULL), so this is not
   a type-system violation.
5. **Array comparison uses btree NULL semantics, NOT 3VL** (§5) — NULL elements are comparable and
   mutually equal; an array comparison is always a definite boolean. This is PG's `array_eq`/
   `array_cmp` and a deliberate contrast with composite row-comparison 3VL. Oracle-pinned.
6. **`IS NULL` tests the whole value only, not element-wise** (§5) — the contrast with composite's
   all-fields rule. PG; oracle-pinned.
7. **Multidimensionality is a value property, not array-of-array** (§2) — `int32[][]` is
   `Array(Scalar(int32))` with value `ndim` 2, never `Array(Array(int32))`. v1 ships 1-D values;
   multidim construction is `0A000` (§12), with the codec header already carrying `ndim`/`dims`/
   `lbounds` so multidim is a pure unlock (no format bump).
8. **`array_out` matches PG byte-for-byte; `array_in` accepts ≥ what `array_out` emits** (§7),
   including PG's backslash-escaping (vs `record_out`'s doubling) and the unquoted `NULL` element
   token.
9. **Array-as-key deferred `0A000`** — encoding authored, not exercised (§8); the
   text/decimal/bytea/composite-PK precedent.

## 11. Errors

| Failure | Code |
|---|---|
| Unknown element type in a `T[]` declaration | `42704` undefined_object |
| Non-unifiable elements in `ARRAY[…]` / array vs array of a different element type | `42804` datatype_mismatch |
| Subscripting a non-array base | `42804` datatype_mismatch |
| Element value out of range (via element coercion) | `22003` numeric_value_out_of_range |
| Malformed array text literal (`array_in`) | `22P02` invalid_text_representation |
| Bad element value inside a literal | that element's own parse error (e.g. `22P02`) |
| Multidimensional construction/literal; array slice `a[m:n]`; array `PRIMARY KEY`/index/`UNIQUE`; nested array (array-of-array); runtime non-literal text→array cast, `array::text`, element-wise array→array cast; `unnest`/`\|\|`/`@>`/`&&`/`ANY`/`ALL`/`VARIADIC` | `0A000` feature_not_supported |
| (When multidim lands) non-rectangular multidim construction / dimension mismatch | `2202E` array_subscript_error |
| Corrupt array body (bad `ndim`/length/element) | `XX001` data_corrupted |

`2202E` is **not** registered this slice (multidim is `0A000`); it is added when the multidim
follow-on lands. All other codes above already exist in [../errors/registry.toml](../errors/registry.toml).

## 12. Delivery (sub-slices)

Arrays are **not a single vertical slice** — they land as ordered, independently-shippable sub-slices,
each passing `rake ci`, mirroring composite's S0–S6:

- **S0 ✅** — this doc + the CLAUDE.md §4 array-axis touch (shape is a value property; structural;
  second container axis) + the TODO.md array slices + the §10 decisions + the §11 error surface.
- **S1 ✅** — the open-`Type` `Array(Box<Type>)` arm threaded through parser/resolver/evaluator as a
  behavior-preserving extension (composite already opened `Type`, so this is additive, *smaller* than
  composite's S1 refactor).
- **S2 ✅** — a declarable + storable array **column** (scalar elements) + `type_code = 15` + the value
  codec (§4) + `format_version` 10 + new goldens (`array_table.jed`, `rust == go == ts == ruby`); the
  `ARRAY[…]` constructor and the `'{…}'`/`::` literal (`array_in`) in expression + INSERT position;
  INSERT/SELECT round-trip; `array_out` rendering — all three cores + the Ruby reference byte-identical.
  (1-D values only.)
- **S3 ✅** — subscripting `a[i]` (1-based, OOB/NULL → NULL; non-array base `42804`) — parsed as a
  postfix `[…]` on any base, resolved to the element type, evaluated 1-based with the OOB/NULL→NULL
  rule. All three cores + `types/subscript.test`. Slices `a[m:n]` stay deferred `0A000` (§6).
- **S4 ✅** — comparison / ordering / `IS NULL`: the resolver gate (same-element-type arrays
  comparable; `42804` otherwise), the **btree-NULL** element-wise `eq3`/`lt3`/`gt3` (§5 — *not* the
  composite 3VL path), the `ORDER BY` total-order arm, DISTINCT/GROUP BY array keys, the
  whole-value-only `IS NULL`. Oracle-pinned via `rake corpus:check`. (Landed with S1/S2.)

**Narrowed in v1, relaxed in later slices (each its own follow-on):** multidimensional values
(construction/literal `0A000`; the codec header already supports them); custom lower bounds in
construction; array slices `a[m:n]`; **array-of-composite** elements (a fast-follow — composite
already composes); arrays-in-keys (`0A000`, encoding authored §8); the array function/operator
surface (`array_length`/`cardinality`/`unnest`/`||`/`array_cat`/`@>`/`&&`/`array_append`/…, the
polymorphic `anyarray`/`anyelement` resolution, `ANY`/`ALL`, and `VARIADIC` — TODO.md); runtime
text→array, `array::text`, and element-wise array→array casts.
