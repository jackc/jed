# Composite (row) types — design

> `CREATE TYPE name AS (field type, …)` / `DROP TYPE`: named, user-defined **composite types**
> (PostgreSQL row types) — a heterogeneous fixed-shape tuple of existing types, usable as a
> column type, constructed with `ROW(…)`, read field-by-field with `(expr).field`, compared
> element-wise, and rendered/parsed as `(a,b,…)` text. This doc is the contract all three cores
> implement in lockstep (CLAUDE.md §2); the grammar is in
> [../grammar/grammar.ebnf](../grammar/grammar.ebnf) + [grammar.md](grammar.md), the byte layout in
> [../fileformat/format.md](../fileformat/format.md) (`format_version` 9), the key encoding in
> [encoding.md §2.10](encoding.md), the cost contract in [cost.md §3](cost.md), and the
> extension framing in [extensibility.md §4.1/§6](extensibility.md). PostgreSQL semantics were
> pinned against the live `postgres:18` oracle (CLAUDE.md §1).

Composite types are the **first user-defined type** and the event that turns jed's **closed**
type enum into an **open** type system: a type is no longer only a compiled-in `ScalarType`
variant but can be *a fact about a database* — named, created and dropped at runtime, recursive,
persisted in the catalog. The cross-core contract shifts in kind accordingly: from "the data
table is byte-identical" (scalars, codegen'd from [../types/scalars.toml](../types/scalars.toml))
to "the **recursive** codec / comparator / NULL-rule / text-I/O is byte-identical" — hand-written
per core (§5 forbids codegenning it), policed by new golden fixtures + corpus entries (CLAUDE.md
§8). Because every method is **derived** from field types that are already cross-core-identical
([extensibility.md §4.1](extensibility.md)), that byte-identity holds *by construction* — no host
code, and a composite value is self-describing and portable (its field list lives in the type
catalog, so any jed can read a file containing it).

## 1. Surface

```sql
CREATE TYPE addr AS (street text, zip int32)
DROP TYPE [IF EXISTS] addr [RESTRICT]

CREATE TABLE person (id int32 PRIMARY KEY, home addr)
INSERT INTO person VALUES (1, ROW('Main', 90210))
SELECT id, (home).zip, home FROM person ORDER BY home
SELECT * FROM person WHERE home = ROW('Main', 90210)
```

- **Named composites only.** A composite type has a name in the database's type namespace and a
  fixed, ordered list of named fields. Anonymous `record` (the untyped row, e.g. a `record`-typed
  column) is **not** supported this slice (`0A000`). A `ROW(…)` *result* still has a structural
  (anonymous) type for typing purposes (§5); it just cannot be stored except into a named
  composite column.
- **Field types** are any *existing* type — a built-in scalar or a previously-defined composite.
  **Nested composites are supported** (`CREATE TYPE line AS (a addr, b addr)`): every derived
  method recurses. An unknown field type is `42704`.
- **`ROW(…)` constructor only.** The bare `(a, b, …)` row constructor (PostgreSQL's parenthesized
  form) is deferred (`0A000`) — it is parser-ambiguous with grouping/subqueries and adds no
  capability `ROW(…)` lacks. `ROW(x)` is a one-field row; `ROW()` is the zero-field row.
- **Field access** `(expr).field` and `(expr).*` (✅ S4). Field selection is **parens-required**,
  matching PostgreSQL: `.field` / `.*` is a postfix operator that applies only to a **parenthesized**
  base — `(home).zip`, `(t.home).zip`, `(ROW(1,2)).f1`, `('(…)'::addr).zip` — and chains on a prior
  field access (`(c).a.b`). The **unparenthesized** `home.zip` / `t.home.zip` is a (multi-part)
  column reference, **never** field access: its first identifier must name a relation, else `42P01`
  (`SELECT home.zip` where `home` is a composite column is `42P01`, exactly as PG — you must write
  `(home).zip`). This was an **oracle correction**: the original plan assumed a "bare `col.field`
  fallback" (table.column-then-column.field), but live PG rejects every unparenthesized field
  reference, so jed matches PG (no fallback). Field lookup is case-insensitive; an unknown field is
  `42703`, a non-composite base `42809`. `(expr).*` expands a composite into one output column per
  field (declaration order) and is a **projection-list** construct only — `.*` in a scalar position
  is `0A000`.
- **Comparison** `= <> < <= > >=` is element-wise lexicographic (PG row-comparison, §5).
  `IS NULL` / `IS NOT NULL` follow PG's all-fields rule (§5) — they are *not* negations.
- **Composite columns are declarable from S3 on; never keyable.** The composite-typed *column*
  arrives with the value codec in S3 (in S2 it is rejected `0A000` — §12); once declarable it is
  storable, orderable, and groupable, but a composite `PRIMARY KEY` / index / `UNIQUE` column stays
  rejected `0A000` (§6) — the order-preserving key encoding is authored
  ([encoding.md §2.10](encoding.md)) but not exercised, the same staged narrowing text/decimal/bytea
  keys still carry.

## 2. The open type system — `Type { Scalar | Composite }`

`ScalarType` stays exactly as it is (its variants, `from_name`/`canonical_name`/`all()`, and the
integer-only accessors that are unreachable for non-integers). Openness is a **new wrapper above
it**, threaded wherever a column/value type was a `ScalarType`:

- `Type = Scalar(ScalarType) | Composite(CompositeRef)`.
- `CompositeRef` carries the type's **id + name** only. The resolved field list lives **once** in
  the database's type catalog, keyed by id — mirroring how a `Table` holds primary-key *ordinals*,
  not column copies. (Rust `enum Type`; Go a tagged `Type` struct with a `Composite bool`
  discriminant per the `Value` idiom; TS a `{kind}` union.)
- A composite **value** is an ordered list of field values, recursive: `Value::Composite(Vec<Value>)`
  (Rust) / a `ValComposite` kind holding a `*[]Value` **pointer** so the flat Go `Value` struct
  stays `==`-comparable — composite equality and hashing are forced through the structural
  `eq3` / value-key path, never raw `==` (the rule `Decimal`/`Interval` already follow) / a
  `{kind:"composite", fields:Value[]}` TS union arm.

A `CompositeType` is `{ name, fields: [(name, Type)] }`, recursive, resident in the snapshot
catalog (§3). The integer-only `ScalarType` accessors never receive a composite.

## 3. Catalog & on-disk format (`format_version` 9)

Composite types are **database-level objects**, not per-table, so the catalog — today a chain of
**table** entries — becomes a chain of **kind-tagged** entries. The on-disk shape (full byte
layout in [../fileformat/format.md](../fileformat/format.md)):

- **Every catalog entry gains a leading `entry_kind u8`**: `0` = a table entry (the v8 shape,
  unchanged after the tag byte), `1` = a composite-type entry. Composite-type entries are emitted
  **first** (ascending lowercased-name order), then the table entries (ascending lowercased-name
  order); `item_count` per catalog page counts all entries, packed greedily exactly as before.
  This keeps the catalog a uniform "sequence of entries" — no special head page, no separate page
  chain, no meta-page change.
- **Composite-type entry:** `entry_kind = 1`, `name_len u16` + name, `field_count u16`, then per
  field — `field_name_len u16` + name, `field_type_code u8`, then (only when the field's code is
  `14`) `field_type_name_len u16` + the referenced composite type's name **or** (only when the
  field's code is `15` — an **array-typed field**, [array.md §12](array.md)) the inline array
  element-type descriptor (the same descriptor an array column uses, array.md §3), `field_flags u8`
  (bit 0 = NOT NULL), and (only when the field's code is `6`) the decimal typmod (`precision u16`,
  `scale u16`). Reuses the existing stable scalar type codes. The array field descriptor sits
  **before** the flags byte, exactly where a nested-composite name does.
- **Composite columns reference their type by NAME** — a composite column in a table entry stores
  `type_code = 14` followed by `type_name_len u16` + the composite type's name, occupying the slot
  a decimal column uses for its typmod. By-name (not a numeric id) is the boring, explicit choice
  (CLAUDE.md §10): self-describing, no id-stability/renumbering reasoning, matches how tables and
  the in-memory `CompositeRef` are referenced. The name is deterministic by construction.
- **Nested composites persist by reference** — a field whose type is itself a composite stores
  `field_type_code = 14` + the referenced type's name, never an inline nested definition (PG's
  `atttypid` model; keeps the byte stream non-recursive). The loader is therefore **two-pass**:
  collect every composite-type entry into a name→definition map, then validate that every
  referenced composite name exists and the reference graph is **acyclic**; only then build the
  tables (resolving each composite column's name). A dangling reference or a definition **cycle**
  is a malformed file — `XX001`.
- **`format_version` 9** is a clean break (the v9 reader rejects v8 and earlier, as v5/v6 did).
  All existing `.jed` goldens regenerate at the bump (the version byte, plus the new `entry_kind`
  byte on each table entry).

## 4. Value codec — recursive (the `(home).* ` round-trip)

A composite **value body** (after the shared `0x00` present / `0x01` NULL presence tag) is:

```
null-bitmap  ‖  each PRESENT field's value-codec body, in declaration order
```

- The **null bitmap** is `ceil(field_count / 8)` bytes, **MSB-first**: field *i*'s NULL bit is
  bit `0x80 >> (i % 8)` of byte `i / 8` (field 0 = byte 0's `0x80` bit). A set bit = that field is
  NULL and contributes **zero** body bytes; the reader consults the bitmap to know whether to
  decode a body. (Bit order is a hotspot — pin it MSB-first.)
- A **present** field is written **without its own presence tag** — the bitmap carries presence —
  so the existing per-type value codec is split into `tag-byte + body`, and the composite path
  recurses on the body half (a clean refactor across all three cores + the Ruby reference encoder).
- A **whole-value-NULL** composite is the lone `0x01` tag, no bitmap.

Worked example, `addr AS (street text, zip int32)`:

| value | bytes (body, after the present tag) |
|---|---|
| `('Main', 90210)` | `00`(bitmap) `00 04 4D 61 69 6E`(text) `80 01 60 62`(int32 = 90210 + 2³¹, BE) |
| `('Main', NULL)` | `40`(bitmap: field 1 NULL) `00 04 4D 61 69 6E`(text) — int field omitted |
| whole-value NULL | (no body; the value is the lone `0x01` tag) |

A composite is one opaque inline body that **spills via the existing large-values overflow + LZ4
path** ([large-values.md](large-values.md)) when it exceeds `RECORD_MAX`. Composite-*internal*
per-field spill (independently externalizing one big field) is deferred; an over-cap record uses
the existing pathological-record handling. At the default 8192-byte page size ordinary composites
never approach the cap.

## 5. Comparison, ordering, and NULL

Composite comparison is **recursive / structural**, so it is a hand-written special case in the
value module's `eq3` / `lt3` / `gt3` — **not** a [../functions/catalog.toml](../functions/catalog.toml)
operator row (the catalog cannot express "recurse over N heterogeneous fields"; CLAUDE.md §5
forbids codegenning it; the existing `Decimal`/`Interval` value-canonical comparisons set the
precedent). [compare.toml](../types/compare.toml) stays scalar-only. Two composite values are
comparable iff they share the **same type id**; any other pair is `42804` at resolve time.

- **3VL equality (`=`, `<>`):** field-by-field. `=` is FALSE if **any** field compares FALSE;
  else UNKNOWN if **any** field is UNKNOWN; else TRUE. So `ROW(1,NULL) = ROW(1,2)` → UNKNOWN,
  `ROW(1,NULL) = ROW(2,2)` → FALSE (a FALSE field dominates a NULL field). `<>` is the 3VL
  negation.
- **Ordering (`< <= > >=`, and the ORDER BY / DISTINCT / GROUP BY sort key):** lexicographic —
  the first field whose comparison is not "equal" decides. The *boolean operators* propagate
  UNKNOWN at the deciding field (PG row-comparison NULL rule); the *sort key* is a **total** order
  with NULLs-last per field (`null_ordering = "nulls-last-ascending"`, [compare.toml](../types/compare.toml)),
  so DISTINCT/GROUP BY/ORDER BY are well-defined. The recursion bottoms out in the per-field
  scalar comparators (so e.g. the TS UTF-8 text-ordering trap recurses correctly).
- **`IS NULL` / `IS NOT NULL` — PG's all-fields gotcha (they are NOT negations), and ONE LEVEL
  DEEP, NOT recursive:**
  - `row IS NULL` is TRUE iff the value is SQL-NULL **or every immediate field is SQL-NULL**.
  - `row IS NOT NULL` is TRUE iff the value is non-NULL **and every immediate field is non-NULL**.
  - A field counts as "null" only when it is itself **SQL-NULL**; a *composite-valued* field is a
    non-null value, so it counts as **present** and is **not descended into**. This was the most
    PG-subtle detail — and the **differential oracle corrected the original recursive assumption**:
    empirically (PG 18) `ROW(ROW(NULL,NULL), ROW(NULL,NULL)) IS NULL` is **FALSE** (the inner rows
    are non-null values), and `… IS NOT NULL` is **TRUE**. A partially-NULL row (`ROW(1, NULL)`) is
    **FALSE for both**. Implemented as `Value::is_null_test(negated)` — a dedicated composite branch
    that tests immediate fields for SQL-NULL only, pinned exhaustively (flat all-NULL, partial,
    nested all-NULL, and `NULL + composite` cases).

## 6. Key encoding — authored, deferred

The order-preserving composite-type key encoding is the concatenation of each field's
order-preserving encoding, each wrapped in the [encoding.md §2.2](encoding.md) nullable slot,
recursing for nested composites — i.e. exactly the composition the multi-column composite
`PRIMARY KEY` already uses ([encoding.md §2.3](encoding.md)). It is **authored**
([encoding.md §2.10](encoding.md)) but **not exercised** this slice: a composite-typed
`PRIMARY KEY`, index, or `UNIQUE` column is rejected `0A000` at the DDL resolver (the site that
already rejects a text/decimal/bytea PK). The narrowing is doubly forced — most field types' own
key encodings are themselves still deferred, so a composite key containing them could not be
exercised regardless. Composite **values** remain fully storable, orderable, and groupable via the
in-memory structural comparator (§5) — no key bytes required.

## 7. `DROP TYPE` and dependency tracking

`DROP TYPE` is **`RESTRICT` by default and RESTRICT-only** this slice (`CASCADE` is `0A000`). It
fails with **`2BP01`** if any table column or any other composite type's field still references
the type; otherwise it removes the type from the catalog. `DROP TYPE IF EXISTS` on a missing type
is a no-op success; without `IF EXISTS`, a missing type is `42704`. The check runs against the
writer's pending catalog under the single-writer staging model (CLAUDE.md §3) — the type set a
`DROP` sees is the same one a concurrent reader does not, until commit.

## 8. Text I/O — `record_out` / `record_in`

- **`record_out`** (✅ S6) renders a composite as `(f1,f2,…)` with PG's field quoting: a field is
  double-quoted iff it is the empty string or contains a comma, parenthesis, double-quote,
  backslash, or whitespace. Inside the quotes PostgreSQL **doubles** an embedded `"` → `""` and
  `\` → `\\` (rowtypes.c `record_out` — *not* backslash-escaping; the oracle corrected the initial
  S3 `\"` rendering). A **NULL** field renders as the empty string between delimiters (unquoted).
  The renderer recurses for nested composites. This is the determinism/oracle surface — it equals
  PG byte-for-byte (CLAUDE.md §8), verified by `rake corpus:check`.
- **`record_in`** (✅ S6) parses `(…)` into fields (top-level commas, respecting quotes/escapes/
  nesting) and recursively coerces each token to its field type — an empty **unquoted** token is
  NULL, `""` is the empty text string, `""`→`"` and `\x`→`x` un-escape inside quotes. It is the
  exact inverse of `record_out` (values round-trip). A malformed literal or a wrong field count is
  `22P02`; a bad field value surfaces that field's own parse error (e.g. `22P02` for a non-integer).
  The pure tokenizer is `value::parse_record_tokens`; the executor does the per-field coercion.
- A composite literal is written `'(Main,90210)'::addr` or `addr '(Main,90210)'` (✅ S6) — the
  cast / typed-literal machinery routes the **string-literal → composite** coercion through
  `record_in`, the same out-of-matrix path as a string-literal → scalar coercion (so
  [../types/casts.toml](../types/casts.toml), which is scalar-only, is unchanged). A bare `NULL`
  casts to the composite, and a same-named composite operand is the identity cast. Every other
  text↔composite cast — a **runtime** (non-literal) text expression, a `composite::text`
  record_out cast, and an anonymous `ROW(…)::type` structural cast — is a documented `0A000`
  narrowing this slice (relaxable), mirroring the deferred runtime text→scalar casts.

## 9. Cost

No new cost units this slice. `ROW(…)` construction and `(expr).field` access are interior
expression nodes — each rides one `operator_eval` (like every constructor-shaped node). A
composite `=` in `WHERE`/`ON` is one compare node → one `operator_eval`, the per-field walk
internal. Sort/dedup comparisons follow the existing unmetered-sort boundary ([cost.md §3](cost.md)).
Composite fields are schema-bounded (not unbounded like a big decimal), so flat-per-node is a
sound bound; [../cost/schedule.toml](../cost/schedule.toml) is unchanged.

## 10. Ratified decisions and deliberate PostgreSQL divergences

Default is "match PostgreSQL" (CLAUDE.md §1); the divergences below each have an overriding reason
and are recorded in [../conformance/oracle_overrides.toml](../conformance/oracle_overrides.toml).

1. **Named composites only** — no anonymous `record`-typed columns (`0A000`). The closed→open
   transition is already XL; a `ROW(…)` result still types structurally.
2. **PG all-fields `IS NULL` / `IS NOT NULL` rule** — adopted as-is (§5).
3. **Composite-as-key deferred `0A000`** — encoding authored, not exercised (§6); the
   text/decimal/bytea-PK precedent.
4. **No implicit per-table row types** (divergence) — PG auto-creates a composite type per table
   (`tablename` usable as a type); jed does not. We own our surface (CLAUDE.md §1), and coupling
   the table and type catalogs would complicate `DROP` dependency tracking.
5. **`ROW(…)` only**, bare `(a,b)` deferred `0A000` (§1).
6. **Array vs. composite sequencing** — composite ships first; the open-`Type` plumbing here is
   the shared "containers" foundation the future `array` axis reuses ([TODO.md](../../TODO.md)).
7. **`record_out` matches PG byte-for-byte; `record_in` accepts ≥ what `record_out` emits** (§8).
8. **Nested composites supported** (by construction), persisted by reference (§3).

## 11. Errors

| Failure | Code |
|---|---|
| `CREATE TYPE` duplicate type name | `42710` duplicate_object |
| Unknown field type in `CREATE TYPE` | `42704` undefined_object |
| Field access on a non-composite base | `42809` wrong_object_type |
| Unknown field name (`(addr).nope`) | `42703` undefined_column |
| Wrong field count / type in `ROW(…)` vs a composite target | `42804` datatype_mismatch |
| Malformed composite text literal | `22P02` invalid_text_representation |
| `DROP TYPE … RESTRICT` with dependents | `2BP01` dependent_objects_still_exist |
| `DROP TYPE` of a missing type (no `IF EXISTS`) | `42704` undefined_object |
| Corrupt type catalog (dangling/cyclic field ref) | `XX001` data_corrupted |
| Composite `PRIMARY KEY` / index / `UNIQUE`; bare `(a,b)`; anonymous `record`; `ALTER TYPE`; `DROP TYPE … CASCADE` | `0A000` feature_not_supported |

## 12. Delivery (sub-slices)

Composite types are **not a single vertical slice**. They land as ordered, independently-shippable
sub-slices, each passing `rake ci`: **S0** ✅ spec + the CLAUDE.md §4/§5 open-type-system revision +
decisions + error codes (this doc); **S1** ✅ the open-`Type` refactor as a behavior-preserving no-op;
**S2** ✅ `CREATE`/`DROP TYPE` + the catalog type-definition section + `format_version` 9 + goldens —
the composite **type** is created, dropped, and persisted; **S3** ✅ a storable composite **column**
+ the `ROW(…)` constructor + the recursive value codec (null bitmap + present-field bodies, §4) +
the INSERT/SELECT round-trip + `record_out` rendering (§8) — goldens pin the value bytes, all three
cores + the Ruby reference byte-identical; **S4** ✅ field access `(expr).field` / `(expr).*` — the
parens-required `.field`/`.*` postfix operator and the resolver field lookup (§1; the oracle
corrected the original bare-`col.field`-fallback assumption — PG requires parens); no on-disk format
change; **S5** ✅ comparison / ordering / `IS NULL` — the resolver gate lifted (`classify_comparable`
now allows same-arity, field-comparable composites; `42804` otherwise), the non-recursive all-fields
`IS NULL` rule (`Value::is_null_test`, §5 — the differential oracle corrected the recursive
assumption), the `ORDER BY` lexicographic total-order arm, and DISTINCT/GROUP BY composite keys (the
value Hash/Eq from S3); the S5 corpus rows are PG-verified; no format change; **S6** ✅ the PG-exact
`record_out` (`"`→`""`, `\`→`\\` doubling) + `record_in` (`'(…)'::type` / `type '(…)'`,
string-literal→composite) + the oracle check — `rake corpus:check` regenerates `composite.test`
byte-identically from live PG (two documented comparison-error-code overrides); no format change.
**The composite-types feature is complete (S0–S6).**

**A composite type as an array element** (`addr[]`) landed in [array.md §12](array.md) AC1 — a
composite is a first-class array element type (the recursive codec/comparator/text-I/O composed for
free; the per-element array comparison routes through the composite *total order*, not 3VL —
array.md §5). The **mirror** nesting — a composite type with an **array-typed field**
(`CREATE TYPE poly AS (name text, pts int32[])`) — **landed** ([array.md §12](array.md), capability
`types.composite_array_field`): the composite-type catalog entry gains a `field_type_code = 15`
array field carrying the inline element descriptor (§3, no `format_version` bump — still 10), and
the value codec / comparison / `record_out` / `record_in` recurse through the array field for free
(an array field's `record_in` token is an array text literal coerced through `array_in`, one level
down). The element may itself be a composite (the doubly-nested `addr[]` field, `element_type_code
14` + name). `DROP TYPE` dependency tracking and the two-pass-load existence/acyclicity validation
look through one array level, so an `addr[]` field (or column) is a `2BP01` dependent of `addr`.

**Still narrowed (relaxed in a later slice):** `INSERT … SELECT` into a composite column and
`UPDATE` of a composite column remain `0A000`; a composite `PRIMARY KEY` / index / `UNIQUE` stays
`0A000` (§6); `DEFAULT` on a composite column is `0A000`; the runtime (non-literal) text→composite
cast, the `composite::text` cast, the anonymous `ROW(…)::type` structural cast, and the nested
`ROW(ROW(…),…)`-into-column constructor (a jed extension PG rejects — covered by unit tests, not the
PG-oracle corpus) are each `0A000` / jed-only. Composite **value** comparison (`WHERE c = ROW(…)`),
ORDER BY, DISTINCT, and GROUP BY all landed in S5.
