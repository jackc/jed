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
CREATE TYPE addr AS (street text, zip i32)
DROP TYPE [IF EXISTS] addr [RESTRICT]

CREATE TABLE person (id i32 PRIMARY KEY, home addr)
INSERT INTO person VALUES (1, ROW('Main', 90210))
SELECT id, (home).zip, home FROM person ORDER BY home
SELECT * FROM person WHERE home = ROW('Main', 90210)
```

- **Named composites are the stored kind; anonymous records are query-only.** A composite type you
  can *store* has a name in the database's type namespace and a fixed, ordered list of named fields.
  An anonymous `ROW(…)` *result* — PostgreSQL's pseudo-type `record` — has a structural type (fields
  `f1…fN` over the field types, §5). It is a value and may be a column of a query's own output (a
  derived table, CTE, recursive CTE, or `VALUES` list), alone or as the element of a `record[]`
  (§13). It is **never stored**: no table, view, composite field, or file carries a `record` type,
  so a `CREATE TABLE … AS` or view column of that type is `42P16`. A `ROW(…)` constructor assigned
  to a stored composite column (or cast to a named composite) coerces field-by-field to the named
  type (§8, §12); any other anonymous record is not assignable.
- **Field types** are any *existing* type — a built-in scalar or a previously-defined composite.
  **Nested composites are supported** (`CREATE TYPE line AS (a addr, b addr)`): every derived
  method recurses. An unknown field type is `42704`. Nesting is bounded to `MAX_COMPOSITE_DEPTH`
  (32) deep — a deeper chain aborts `54001` ([cost.md §7b](cost.md)), the untrusted-query
  native-stack gate that keeps the recursive derived methods safe.
- **`ROW(…)` and the bare `(a, b, …)` constructor.** `ROW(x)` is a one-field row; `ROW()` is the
  zero-field row. The bare form — PostgreSQL's implicit `ROW`: two or more comma-separated expressions
  in parentheses — builds the identical anonymous composite (a single parenthesized expression stays a
  grouping, and a leading `SELECT` is still a subquery). It is the idiomatic spelling of row
  comparison (`WHERE (a, b) > (1, 2)`) and of the row-valued subqueries of
  [grammar.md §26](grammar.md) (`(a, b) IN (SELECT x, y …)`). A bare row is not a field-access base:
  `(1, 2).f1` is `42601`, `((1, 2)).f1` selects (PostgreSQL).
- **Field access** `(expr).field` and `(expr).*` (✅ S4). Field selection is **parens-required**,
  matching PostgreSQL: `.field` / `.*` is a postfix operator that applies only to a **parenthesized**
  base — `(home).zip`, `(t.home).zip`, `(ROW(1,2)).f1`, `('(…)'::addr).zip` — and chains on a prior
  field access (`(c).a.b`). The **unparenthesized** `home.zip` / `t.home.zip` is a (multi-part)
  column reference, **never** field access: its first identifier must name a relation, else `42P01`
  (`SELECT home.zip` where `home` is a composite column is `42P01`, exactly as PG — you must write
  `(home).zip`). This was an **oracle correction**: the original plan assumed a "bare `col.field`
  fallback" (table.column-then-column.field), but live PG rejects every unparenthesized field
  reference, so jed matches PG (no fallback). Field lookup is byte-exact on the folded name ([grammar.md §3](grammar.md)); an unknown field is
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

The anonymous record (§13) is a further arm, `Record([(name, Type)])`, that carries its field list
**inline** — structural like `Array`, so two records are the same type iff their field lists are
equal, and no catalog lookup is ever made for one. Only a derived relation's synthetic column has
this type; every storage path (the catalog encoders, the column-type resolver of `CREATE TABLE`) is
unreachable for it. (Rust `Type::Record`; Go a `Record *[]recordField` discriminant beside
`Comp`/`Array`/`Range`; TS a `{kind:"record", fields}` union arm.)

## 3. Catalog & on-disk format (`format_version` 9)

Composite types are **database-level objects**, not per-table, so the catalog — today a chain of
**table** entries — becomes a chain of **kind-tagged** entries. The on-disk shape (full byte
layout in [../fileformat/format.md](../fileformat/format.md)):

- **Every catalog entry gains a leading `entry_kind u8`**: `0` = a table entry (the v8 shape,
  unchanged after the tag byte), `1` = a composite-type entry. Composite-type entries are emitted
  **first** (ascending name byte order), then the table entries (ascending name byte
  order); `item_count` per catalog page counts all entries, packed greedily exactly as before.
  This keeps the catalog a uniform "sequence of entries" — no special head page, no separate page
  chain, no meta-page change.
- **Composite-type entry:** `entry_kind = 1`, `name_len u16` + name, `field_count u16`, then per
  field — `field_name_len u16` + name, `field_type_code u8`, then (only when the field's code is
  `14`) `field_type_name_len u16` + the referenced composite type's name **or** (only when the
  field's code is `15` — an **array-typed field**, [array.md §12](array.md)) the inline array
  element-type descriptor (the same descriptor an array column uses, array.md §3) **or** (only
  when the field's code is `17` — a **range-typed field**, `format_version` 41) the one-byte range
  element code (the same descriptor a range column uses, [ranges.md §3](ranges.md)), `field_flags
  u8` (bit 0 = NOT NULL), and (only when the field's code is `6`) the decimal typmod (`precision
  u16`, `scale u16`). Reuses the existing stable scalar type codes. The array and range field
  descriptors sit **before** the flags byte, exactly where a nested-composite name does. A range
  field takes no typmod — `numrange`'s element is the unconstrained `decimal`, so a type modifier on
  a range field is `0A000`, as on a range column (PostgreSQL raises `42601`; a per-core test pins
  it). A range element is always a scalar, so a range field references no other type and adds no
  edge to the two-pass load's reference graph.
- **Composite columns reference their type by NAME** — a composite column in a table entry stores
  `type_code = 14` followed by `type_name_len u16` + the composite type's name, occupying the slot
  a decimal column uses for its typmod. By-name (not a numeric id) is the boring, explicit choice
  (CLAUDE.md §10): self-describing, no id-stability/renumbering reasoning, matches how tables and
  the in-memory `CompositeRef` are referenced. The name is deterministic by construction.
- **Nested composites persist by reference** — a field whose type is itself a composite stores
  `field_type_code = 14` + the referenced type's name, never an inline nested definition (PG's
  `atttypid` model; keeps the byte stream non-recursive). The loader is therefore **two-pass**:
  collect every composite-type entry into a name→definition map, then validate that every
  referenced composite name exists, the reference graph is **acyclic**, and no type nests deeper
  than `MAX_COMPOSITE_DEPTH` (32 — the nesting-depth gate, [cost.md §7b](cost.md)); only then build
  the tables (resolving each composite column's name). A dangling reference, a definition **cycle**,
  or an **over-deep chain** is a malformed file — `XX001`. The depth bound runs before any store is
  built, so the `resolve_col_type` walk (and every later value-codec/comparator walk) recurses over
  a depth-bounded catalog and stays stack-safe — a conformant engine never *writes* an over-deep
  type (`CREATE TYPE` rejects it with `54001`), so an over-deep chain in a file is
  tampering/corruption, like the cyclic case.
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

Worked example, `addr AS (street text, zip i32)`:

| value | bytes (body, after the present tag) |
|---|---|
| `('Main', 90210)` | `00`(bitmap) `00 04 4D 61 69 6E`(text) `80 01 60 62`(i32 = 90210 + 2³¹, BE) |
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

## 6. Key encoding — the `composite-field-slots` container key (✅ landed)

A composite-typed column is a valid `PRIMARY KEY` / ordered secondary index / `UNIQUE` key: the
order-preserving key is the concatenation of each field's order-preserving encoding, each wrapped in
the [encoding.md §2.2](encoding.md) nullable slot (`0x00` present ‖ the field key, or `0x01` NULL),
recursing for nested composite / array / range fields — the **third container key**
([encoding.md §2.15](encoding.md), `composite-field-slots`, after `range` §2.11 and `array` §2.14).
Because a composite is **fixed-arity** (the field count is a property of the type) it needs no
terminator — unlike the variable-arity array §2.14 — and each field rides the ordinary §2.2 slot; the
whole key is self-delimiting, so it composes as a nested field, an index column (outer §2.2 slot +
storage-key suffix), and a multi-column-PK member. The bytes reproduce the §5 sort key (lexicographic,
NULLs-last per field) under `memcmp`. Cross-core byte-identity holds **by construction** (every field
key is already cross-core-identical, §5), pinned by the `composite_key_table.jed` golden and the
[../encoding/composite.toml](../encoding/composite.toml) vectors.

**Keyability is recursive** — a composite is keyable iff **every** field is keyable. Since every
scalar, every `range`, and every scalar-element `array` is keyable, the one non-keyable inner type is
an **array-of-composite** field (the §2.14 array key admits only scalar elements): a composite that
transitively contains one is `0A000` at CREATE TABLE / CREATE INDEX (a deferred follow-on, §12). The
DDL gate resolves the column's [`ColType`](../../impl/rust/src/catalog.rs) and walks it, bounded by
`MAX_COMPOSITE_DEPTH` (§3, and the type graph is proven acyclic at `CREATE TYPE`), so the check and
the encoder both terminate. Like the other container keys, **point-lookup pushdown stays deferred** (a
composite PK/index `WHERE k = ROW(…)` full-scans + residual-filters — correct, just unindexed), a
composite is **not** a GIN element, and a composite **FK** pairing + **array-of-composite** as a key
are deferred follow-ons. Composite **values** remain storable/orderable/groupable via the in-memory
structural comparator (§5) whether or not the column is a key.

## 7. `DROP TYPE` and dependency tracking

`DROP TYPE` is **`RESTRICT` by default**. It
fails with **`2BP01`** if any table column or any other composite type's field still references
the type, or a view depends on it ([views.md](views.md) §6.2). `CASCADE` drops the dependent views;
`CASCADE` of a type a column or field references is `0A000` (PostgreSQL would drop the column too); otherwise it removes the type from the catalog. `DROP TYPE IF EXISTS` on a missing type
is a no-op success; without `IF EXISTS`, a missing type is `42704`. The check runs against the
writer's pending catalog under the single-writer staging model (CLAUDE.md §3) — the type set a
`DROP` sees is the same one a concurrent reader does not, until commit.

## 8. Text I/O — `record_out` / `record_in`

- **`record_out`** (✅ S6) renders a composite as `(f1,f2,…)` with PG's field quoting: a field is
  double-quoted iff it is the empty string or contains a comma, parenthesis, double-quote,
  backslash, or whitespace. Inside the quotes PostgreSQL **doubles** an embedded `"` → `""` and
  `\` → `\\` (rowtypes.c `record_out` — *not* backslash-escaping; the oracle corrected the initial
  S3 `\"` rendering). A **NULL** field renders as the empty string between delimiters (unquoted).
  Each field renders by its type's output function — a **boolean** field is PG's `boolout` `t`/`f`
  (`ROW(true,1)` → `(t,1)`), as an array element is ([array.md §7](array.md)), not the result
  cell's canonical `true`/`false`. The renderer recurses for nested composites. This is the determinism/oracle surface — it equals
  PG byte-for-byte (CLAUDE.md §8), verified by `mise run corpus:check`.
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
  casts to the composite, and a same-named composite operand is the identity cast.
- **Runtime composite casts** (✅, capability `cast.composite`, corpus
  `suites/cast/composite_casts.test`):
  - **runtime text → composite** — a non-literal text expression parses through `record_in` per
    row against the target's field types (`22P02` for a malformed literal or a wrong field count,
    a field's own input error otherwise);
  - **composite → text** — `record_out` per row, the text a result cell shows (a `timestamptz`
    field in the session zone). **Explicit only**, like every cast to text (types.md §5); a
    `varchar(n)` target truncates the text. An anonymous `ROW(…)` renders the same way;
  - **`ROW(…)::type`** — an anonymous `ROW(…)` or bare `(a, b, …)` *constructor* cast to a named
    composite casts each field **explicitly** to its field type (its `numeric(p,s)` / `varchar(n)`
    modifier applied; a nested `ROW(…)` cast one level down) — PostgreSQL's
    `coerce_record_to_complex` under the explicit context. The field count must match.

  Both runtime casts are STABLE when the composite type holds a `timestamptz` (it reads the session
  zone) and immutable otherwise, the array cast's rule. A field-count mismatch, an uncastable field,
  an anonymous record that is not a constructor (a derived column, a subquery), a different named
  composite, and a composite → scalar cast are **`42846`** cannot_coerce, as in PostgreSQL (jed's
  `42804` convention for an unconvertible scalar pair, types.md §9, does not extend to composite
  casts). Each cast node is one `operator_eval`; `composite → text` is a growth kernel charged
  for its output and one `timezone` unit per rendered instant, like `array → text` (cost.md §8.1).

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

1. **Named composites are the only stored kind** — an anonymous `record` (and `record[]`) is a
   query-output type only (§13): a derived relation's column, never a table, view, or composite
   field. The same line PostgreSQL draws (its `record` is a pseudo-type, `42P16` in a table or
   view). Where PostgreSQL loses a derived record's field list, jed keeps it (§13, ledgered).
2. **PG all-fields `IS NULL` / `IS NOT NULL` rule** — adopted as-is (§5).
3. **Composite-as-key** — ✅ landed as the third container key (§6, [encoding.md §2.15](encoding.md));
   an **array-of-composite** key and a composite **FK** pairing remain deferred `0A000` follow-ons.
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
| Wrong field count / type in `ROW(…)` vs a composite target; a non-constructor anonymous record assigned or cast to a composite; a composite cast to a different composite or a scalar | `42846` cannot_coerce |
| Malformed composite text literal | `22P02` invalid_text_representation |
| `DROP TYPE … RESTRICT` with dependents | `2BP01` dependent_objects_still_exist |
| `DROP TYPE` of a missing type (no `IF EXISTS`) | `42704` undefined_object |
| `CREATE TYPE` nesting deeper than `MAX_COMPOSITE_DEPTH` (32) | `54001` statement_too_complex |
| Corrupt type catalog (dangling/cyclic/over-deep field ref) | `XX001` data_corrupted |
| A `CREATE TABLE … AS` or view column of type `record` / `record[]` (§13) | `42P16` invalid_table_definition |
| `unnest(record[])` in `FROM` without a column-definition list (§13) | `42601` syntax_error |
| A column-definition list whose shape does not match the record's fields (§13) | `42804` datatype_mismatch |
| A composite `PRIMARY KEY`/index/`UNIQUE` whose type transitively contains an **array-of-composite** field; a composite **FK** pairing; bare `(a,b)`; `ALTER TYPE`; `DROP TYPE … CASCADE` of a type a column or field uses | `0A000` feature_not_supported |

## 12. Delivery (sub-slices)

Composite types are **not a single vertical slice**. They land as ordered, independently-shippable
sub-slices, each passing `mise run ci`: **S0** ✅ spec + the CLAUDE.md §4/§5 open-type-system revision +
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
string-literal→composite) + the oracle check — `mise run corpus:check` regenerates `composite.test`
byte-identically from live PG (two documented comparison-error-code overrides); no format change.
**The composite-types feature is complete (S0–S6).**

**A composite type as an array element** (`addr[]`) landed in [array.md §12](array.md) AC1 — a
composite is a first-class array element type (the recursive codec/comparator/text-I/O composed for
free; the per-element array comparison routes through the composite *total order*, not 3VL —
array.md §5). The **mirror** nesting — a composite type with an **array-typed field**
(`CREATE TYPE poly AS (name text, pts i32[])`) — **landed** ([array.md §12](array.md), capability
`types.composite_array_field`): the composite-type catalog entry gains a `field_type_code = 15`
array field carrying the inline element descriptor (§3, no `format_version` bump — still 10), and
the value codec / comparison / `record_out` / `record_in` recurse through the array field for free
(an array field's `record_in` token is an array text literal coerced through `array_in`, one level
down). The element may itself be a composite (the doubly-nested `addr[]` field, `element_type_code
14` + name). `DROP TYPE` dependency tracking and the two-pass-load existence/acyclicity validation
look through one array level, so an `addr[]` field (or column) is a `2BP01` dependent of `addr`.

**A composite type with a range-typed field** (`CREATE TYPE booking AS (room text, span int4range)`)
**landed** (capability `types.composite_range_field`, `suites/types/composite_range_field.test`): the
composite-type catalog entry gains a `field_type_code = 17` range field carrying the one-byte range
element code (§3, `format_version` 41 — unlike the array field, which fit an existing v10 shape, a
range field had no descriptor slot, so its bytes needed the bump). The value codec, comparison,
`record_out` / `record_in`, and the composite key (§6, recursing into the range-bounds key) recurse
through the range field: its `record_in` token is a range text literal coerced through `range_in`
(a discrete subtype canonicalizes to `[)`), and `record_out` quotes `range_out`'s text, nesting a
`tsrange` bound's own quoting. Any of the six range types may be a field; a range element is
always a scalar, so `DROP TYPE` dependency tracking and the two-pass load gain nothing to look
through.

**The array function/operator surface over composite elements** (`array_append`/`array_cat`/`||`,
`@>`/`<@`/`&&`, `ANY`/`ALL`, the introspectors, the search/edit functions, `num_nulls` VARIADIC) and
**`unnest(composite[])`** landed (AF7, [array-functions.md §13](array-functions.md), capability
`func.array_composite`). The one composite-specific subtlety: `x op ANY/ALL(addr[])` compares a
composite operand pair through the composite **total order** (definite, NULL fields comparable —
PG `record_eq`/`record_cmp`), **not** the bare-`ROW` 3VL §5 rule — so `ROW('a',NULL)::addr =
ANY(ARRAY[ROW('a',NULL)::addr])` is **TRUE** (the array-`=`/AC1 dichotomy, extended to the
quantifiers), while a whole-element `NULL` still folds to UNKNOWN.

**Composite-as-key landed** (§6, [encoding.md §2.15](encoding.md)) — a composite-typed `PRIMARY KEY`
/ ordered secondary index / `UNIQUE` column of an all-keyable-field type, the third container key.
Still deferred `0A000` on the key axis: an **array-of-composite** field as a key, and a composite
**FK** pairing (which needs composite same-type FK resolution).

**`INSERT … SELECT` into a composite column** takes a source column of the same composite type or an
untyped `NULL`; any other type is `42804`, except an anonymous `ROW(…)` column, which is `42846`
as in PostgreSQL — a projected record is a source column, not a constructor whose fields could
coerce — [grammar.md](grammar.md) §24; `CREATE TABLE … AS` fills its composite columns through
it, [create-table-as.md](create-table-as.md) §3.

**Assigning a composite column.** A bare `NULL` adapts to a composite column as its whole-value
NULL, and a bare string literal adapts through `record_in` (§8) — the unknown-literal-adapts-to-context
rule scalar, array, and range columns follow — in every write position: a one-row or multi-row
`INSERT … VALUES` (beside `ROW(…)`, a cast literal, or an expression slot; inside a jed-extension
nested `ROW(…)` / `ARRAY[ROW(…), …]` constructor too), `INSERT … SELECT`, `UPDATE … SET`, and
`ON CONFLICT DO UPDATE SET` (oracle-checked, `suites/dml/composite_assign.test`). `UPDATE` and
`DO UPDATE` take the INSERT … VALUES expression rule otherwise: a value of the column's own composite
type (another composite column, a scalar subquery, `excluded.col`, `DEFAULT`) is assigned; any other
type is `42804`. A bare **`DEFAULT NULL`** on a composite (or array/range) column is accepted as the
same as no default — PostgreSQL stores no default for a NULL constant — so it records nothing (no
catalog bytes) and `jed_columns.column_default` is NULL.

**Assigning a `ROW(…)` expression** (✅, capability `types.composite_row_assign`, corpus
`suites/dml/composite_row_assign.test`). A `ROW(…)` or bare `(a, b, …)` constructor whose fields are
general expressions — an `INSERT … VALUES` slot, an `UPDATE SET` or `DO UPDATE SET` right-hand side,
a `DEFAULT` — coerces **field-by-field** to the column's named composite by the **assignment** rule
(PostgreSQL's record → composite coercion): each field resolves with its field type as the literal
hint and must be assignable to it, by the scalar column rule for a scalar field and by this
container rule for a container field — so a **nested `ROW(ROW(…), …)`** coerces one level down into
a nested composite field, beside a bare `NULL` or a `record_in` string for that field. The values are
coerced when stored (an integer range-checked `22003`, a decimal rounded to its field's scale,
`store_composite`). The field count must match the type's. A field-count mismatch, an unassignable
field, and an anonymous record that is **not** a constructor (a `CASE` over rows, a scalar subquery
returning a row) are `42846` cannot_coerce "cannot cast type record to addr", as in PostgreSQL.
The constructor is an ordinary `Row` node of the named type, so the cost is the
unchanged per-node `operator_eval`.

**A non-NULL `DEFAULT` on a composite column** (✅, capability `ddl.composite_default`, corpus
`suites/ddl/composite_default.test`, `format_version` 44). A composite column's default is always an
**expression default** ([constraints.md](constraints.md) §2), even a bare string literal: a
`ROW(…)` (the assignment coercion above), a `record_in` string, or any same-typed expression without
a column reference, validated at `CREATE TABLE` / `ALTER TABLE ADD COLUMN` / `ALTER COLUMN SET
DEFAULT` (a malformed literal `22P02`, a field-count mismatch `42846`, a non-composite expression `42804`,
a column reference `0A000`) and evaluated per applied row — an omitted column, a `DEFAULT` slot,
`UPDATE SET col = DEFAULT`, and the existing rows an `ADD COLUMN` fills. The catalog persists its
token text on the composite column entry (flags bit3 + the text after the type name,
[../fileformat/format.md](../fileformat/format.md)); `jed_columns.column_default` shows that text.

Composite **value** comparison (`WHERE c = ROW(…)`), ORDER BY, DISTINCT, and GROUP BY all landed in
S5.

## 13. Anonymous record columns (✅ landed)

An anonymous `ROW(…)` — PostgreSQL's pseudo-type `record` — and an array of them (`record[]`) are
first-class **query values**: every expression form already worked on them (comparison, `ORDER BY`,
`DISTINCT`, `GROUP BY`, field access, `ARRAY[ROW(…)]`, `||`, `= ANY`, subscripts, `record_out` /
`array_out`). This slice lets them be a **column of a derived relation** — a derived table, an
inline or `MATERIALIZED` CTE, a recursive CTE's working table, or a `VALUES` list — which used to be
`0A000`. It is the substrate of recursive-CTE `SEARCH` / `CYCLE`: a `record[]` path accumulated
per step (`path || ROW(id)`), ordered for depth-first order or tested with `ROW(id) = ANY(path)` for
cycle detection ([recursive-cte.md](recursive-cte.md) §9). Corpus: `suites/types/record_column.test`.

- **The type.** A derived relation's synthetic column takes the `Record` type arm (§2), carrying
  the row's resolved field list inline; a reference to the column resolves back to the same nameless
  composite the `ROW(…)` expression had. So the column behaves exactly like the expression: the row
  3VL for `=`/`<`, the composite total order for `ORDER BY` / `GROUP BY` / `DISTINCT`, the all-fields
  `IS NULL` rule, and field access `(r).f1` / `(r).*` / `(p[1]).f2`. A field whose expression was an
  untyped `NULL` is `text` (PostgreSQL's unknown → text rule for an output column), so comparing it
  with a non-text field is `42804`, as in PostgreSQL.
- **Unification widens integer fields.** A `VALUES` / set-operation / `CASE` column, and a recursive
  CTE's column, unifies two anonymous records of the same arity whose fields differ at most in
  integer width — the array rule ([grammar.md](grammar.md) §25), a value no-op since every integer
  is one `i64`. It matters because jed's unconstrained integer literal is `i64`: an anchor
  `ROW(1)` meets a recursive term's `ROW(n + 1)` over an `i32` column. Any other field difference is
  `42804`. PostgreSQL accepts any mix of anonymous records (its `record` is dynamic) but then cannot
  compare rows whose field types differ; jed's is the narrower, statically comparable rule. A
  recursive term still may not *widen* an anchor's record (anchor `ROW(1::i32)`, recursive
  `ROW(x::i64)` is `42804`, as for an array — [recursive-cte.md](recursive-cte.md) §2).
- **Never stored.** A `CREATE TABLE … AS` column ([create-table-as.md](create-table-as.md) §2) or a
  view column ([views.md](views.md) §1.1) of type `record` / `record[]` is **`42P16`** *column "r"
  has pseudo-type record* (`record[]`), as in PostgreSQL — a view column is a catalog column, even
  though jed persists only its name. The catalog encoders treat a `Record` type as unreachable.
- **`unnest(record[])`** (✅ landed, capability `func.unnest_record`, corpus
  `suites/types/record_unnest_coldef.test`). `unnest` over a `record[]` is a function returning
  `record`, which in `FROM` takes PostgreSQL's **column-definition list**: `unnest(arr) AS u(a i32,
  b text)` (also `u(a i32, …)` without `AS`, and the alias-less `AS (a i32, …)`, whose relation keeps
  the name `unnest`). The relation has the declared columns, typed as declared; each element's
  fields spread over them in order, a NULL element is a row of NULLs, and a NULL or empty array
  yields no rows; each row charges one `generated_row`, like `unnest`. The rules, PostgreSQL's:
  - Without the list it is **`42601`** *a column definition list is required for functions
    returning "record"*. A list on any other `unnest` is `42601` (*only allowed for functions
    returning "record"*; for a named-composite array, *redundant for a function returning a named
    composite type*), and on a multi-argument `unnest` it is `42601` too.
  - A duplicate column name is **`42701`** — in every column-definition list, the json record
    functions' too ([json-table.md](json-table.md) §1). An unknown type is `42704`.
  - The declared shape must match the record's: a different field count, or a field whose type
    is not its column's, is **`42804`** *function return row and query-specified return row do not
    match*. A declared column may be any scalar, range, named composite, or array of a scalar or
    composite; a type modifier (`decimal(6,2)`, `varchar(3)`) is accepted and **not** enforced —
    PostgreSQL's match is binary-coercibility, which neither checks nor applies it.
  - **Deliberate divergences** (ledgered). (1) jed checks the shape **at plan time**, against the
    record's static field list (CLAUDE.md §4); PostgreSQL checks the row it gets back, so an
    empty or all-NULL array never errors there. (2) **Integer widths are interchangeable** — also as
    integer-array elements — because jed's unconstrained integer literal is `i64` where PostgreSQL's
    is `int4`: without this, the PostgreSQL-idiomatic `ROW(1, …)` could not fill an `i32` column. A
    value outside a narrower declared integer type is `22003` at execution; PostgreSQL rejects the
    field type itself (`42804`). (3) A bare string literal inside `ROW(…)` is `text` in jed, so it
    fills a `text` column; PostgreSQL's is `unknown` and matches nothing.
- **Deliberate divergence — jed keeps the field list.** PostgreSQL's `record` is *dynamic*: it keeps
  a derived record's fields only where the planner still sees the `ROW(…)` expression (a plain
  derived table or CTE — jed agrees there). Through a `VALUES` list, a `record[]` element, or a
  recursive CTE's working table it loses them, and field access is `42809` *record type has not been
  registered* / `42703`. jed's record type is static (CLAUDE.md §4), so the field always resolves.
  A lenient divergence — jed accepts a superset — ledgered in
  [../conformance/oracle_overrides.toml](../conformance/oracle_overrides.toml).
- **No format change.** A derived relation is a query-plan construct; nothing new reaches the file.
  Spill and materialized-CTE buffers hold self-describing `Value` rows, so a record column needs no
  codec of its own there.

