# GIN inverted indexes — design

> `CREATE INDEX … USING gin (col)`: a **generalized inverted index** — a second index
> *kind* beside the ordered B-tree ([indexes.md](indexes.md)) — that maps the **terms**
> extracted from a value to the rows containing them, so a predicate over a multi-valued
> column narrows its scan to matching rows instead of reading the whole table. Two operator
> classes ship: **`array_ops`**, accelerating the array **containment `@>`**, **overlap `&&`**,
> membership, and equality operators ([array-functions.md §10](array-functions.md)) over an array
> column of an admitted key-encodable element type (§1), and **`jsonb_ops`** (§2.1),
> accelerating the `jsonb` containment `@>` and key-existence `?` / `?|` / `?&` operators
> ([json-sql-functions.md §1](json-sql-functions.md)) over a `jsonb` column. This doc is the
> contract all three cores implement in lockstep (CLAUDE.md §2); the grammar is in
> [../grammar/grammar.ebnf](../grammar/grammar.ebnf) + [grammar.md §30](grammar.md), the
> byte layout in [../fileformat/format.md](../fileformat/format.md) (`format_version` 13),
> the term encoding in [encoding.md](encoding.md), and the cost contract in
> [cost.md §3](cost.md). PostgreSQL semantics are pinned against the live `postgres:18`
> oracle (CLAUDE.md §1).

## 1. Surface

```sql
CREATE [UNIQUE] INDEX [name] ON table USING gin (col)
```

- **A GIN index is a new index *kind*.** Everything an ordinary index has — the relation
  namespace, auto-naming (`<table>_<col>_idx` + suffixes), `DROP INDEX`, `DROP TABLE`
  cascade, the catalog slot, the page-backed copy-on-write B-tree — it has unchanged
  ([indexes.md §2/§6](indexes.md)); only **how entries are derived from a row** and **how a
  query uses it** differ. The default kind (no `USING`, or `USING btree`) is the ordered
  B-tree; `USING gin` selects this one.
- **One column: a `jsonb` column (`jsonb_ops`, §2.1) or an array (`array_ops`).** The operator
  class is implied by the column type — there is no explicit opclass syntax and none is persisted.
  For an array, a single column whose element type is one of the integers (`i16[]` / `i32[]` /
  `i64[]`), `boolean[]`, `uuid[]`, `date[]`, `timestamp[]`, `timestamptz[]`, the binary floats
  `f32[]` / `f64[]`, `interval[]`, `text[]`, `bytea[]`, or `decimal[]` — every scalar element type
  whose order-preserving key encoding has landed ([encoding.md §2.1](encoding.md), [§2.4](encoding.md)
  for text, [§2.5](encoding.md) for decimal, [§2.6](encoding.md) for bytea, [§2.8](encoding.md) for
  floats, [§2.10](encoding.md) for interval), the keys an ordered-index / `PRIMARY KEY` column
  accepts. A multi-column GIN, and arrays of composite, `jsonb`, or other non-scalar elements, are
  `0A000` this slice (§3, §10). A `json` column has no GIN operator class (`42704`, as in
  PostgreSQL).
- **Accelerates `@>`, `&&`, `const = ANY(col)` membership, and array `=`.** A `WHERE`
  conjunct `col @> const` (contains), `col && const` (overlaps), `const = ANY(col)` (the
  array spelling of membership — semantically `col @> ARRAY[const]`), or `col = const`
  (exact array equality — a `col @> distinct(const)` bound + the residual `=` filter, §6)
  against a GIN-indexed column bounds the scan to candidate rows (§6). `<@` (contained-by)
  and `IN` membership over a scalar list are **not** GIN-accelerated this slice (they still
  run, by full scan — §10). On a `jsonb` column the accelerated conjuncts are `col @> const`
  (equivalently `const <@ col`), `col ? const`, `col ?| const`, and `col ?& const` (§2.1).
- **`UNIQUE` is meaningless for GIN and rejected.** An inverted index has many entries per
  row, so uniqueness is undefined; `CREATE UNIQUE INDEX … USING gin` is `0A000` (§3). A GIN
  index never backs a `UNIQUE` constraint.
- **Indexes maintained on every write, like any index** (§5); the index changes *which
  rows are scanned*, never *which rows or values a query returns* — results are identical
  with or without it (§6).

## 2. The operator class (the type-generic seam)

A GIN index is parameterized by an **operator class** (opclass) — three pure,
deterministic, hand-written-per-core functions that are the *only* type-specific part; the
inverted machinery (the entry B-tree, maintenance, the gather, the cost) is shared:

```
opclass(T):
  extract_index_terms(value: T)            -> Set<Term>           # what to index for a stored value
  extract_query_terms(query, strategy)     -> (Set<Term>, mode)   # what to look up for a predicate
  consistent(found, strategy, query)       -> bool                # does a candidate actually match
```

A `Term` is a canonical, order-preserving byte string (it lives in the entry B-tree, §4);
`mode` is `ALL` (every query term must be present) or `ANY` (≥ 1). This mirrors
PostgreSQL's `extractValue` / `extractQuery` / `consistent` GIN support functions — a
battle-tested decomposition — and it is the seam `jsonb_ops` (§2.1) slots into with **no change
to the inverted core**: the entry tree, the posting-list gather, the ALL/ANY combine, the
storage-key-order candidate fetch, the residual recheck, maintenance, persistence, and the cost
contract are shared. Only term extraction and the accelerated operator shapes differ.

**`array_ops`** (an array column):

- `extract_index_terms(arr)` = the **distinct, non-NULL elements** of `arr`, flattened
  across any dimensionality, each encoded by the element type's key encoding
  ([encoding.md §2.1](encoding.md)). A NULL element and a whole-NULL/empty array produce
  **no** terms (§4).
- `extract_query_terms(Q, strategy)`:
  - `@>` (`contains`): the distinct elements of `Q`, mode **ALL**.
  - `&&` (`overlaps`): the distinct **non-NULL** elements of `Q`, mode **ANY**.
  - `= ANY` (`member`): `Q` is a **scalar** `const`, not an array — its single term
    `{ encode(const) }`, mode **ALL** (one term). A float `const` of the *other* float width is
    converted to the element width first (§2 *Float terms*). A degenerate single-term `@>`: the
    posting list of `encode(const)` is exactly the rows containing `const`, which is exactly
    where `const = ANY(col)` is TRUE (§6). A NULL `const` yields **no** term (a provably-empty
    bound — §6). `const` is always in the element type's range here: jed resolves
    `const = ANY(col)` by coercing `const` to the element type, so an out-of-range constant is
    rejected at resolve (`22003`) and never reaches the gather (§6); the gather range-checks
    anyway, as a defensive guard against silently truncating an out-of-range value into a wrong
    term.
  - `=` (`equal`): exact array equality `col = const`. The **distinct non-NULL elements** of
    `const`, mode **ALL** — the *same* term set and gather as `@>` of `const`, because
    `col = const` ⟹ `col @> const` (equal arrays have identical element multisets, hence
    identical distinct-non-NULL term sets), so the `@>` posting-intersection is a sound
    **superset** of the equal rows and the residual `=` filter (§6) makes it exact. Two ways
    `=` parts from `@>`, both in the degenerate handling (§6), not the gather: **(a)** a NULL
    *element* of `const` does **not** make the bound empty (unlike `@>`, where `@> {…,NULL}` is
    never TRUE) — `col = ARRAY[1,NULL]` legitimately matches a row `{1,NULL}`, which carries the
    term `1`, so the `@> {1}` bound finds it and the residual `=` confirms; **(b)** when `const`
    has **no** non-NULL element (the empty array `'{}'` or an all-NULL `const`), the bound has no
    term, and the rows it would match (`{}`, `{NULL}`, …) carry **no terms at all**, so the index
    cannot enumerate them — this **falls back to the full scan** (like `@> '{}'`), not a
    provably-empty bound. A whole-NULL `const` (`col = NULL`) is 3VL-NULL for every row → provably
    empty (§6).
- **Float terms.** A float element's term is its `float-order-preserving` key
  ([encoding.md §2.8](encoding.md)), which canonicalizes `-0 → +0` and every NaN payload to one
  NaN — so term equality is exactly SQL float `=` under PostgreSQL's total float order (`-0 = +0`,
  `NaN = NaN`; [float.md §3](float.md)), and PG's `array_ops` (whose `float4`/`float8` element
  comparison is that order) agrees. A stored `{-0}` and a query `0` share a term; `{0,-0}` and
  `{NaN,NaN}` each extract ONE term (the dedup of §4 is over canonical bytes). jed resolves a
  float comparison across widths by promoting the `f32` side ([float.md §2](float.md)), so a query
  element of the other width (`f64 = ANY(f32[])`, `f32 = ANY(f64[])`, or the always-FALSE
  cross-width array `=`) is converted to the element width before encoding: an `f32` widens
  exactly; an `f64` narrows only when it **is** an `f32` (`(f32)x == x`, or NaN) — otherwise no
  `f32` element can equal it, so it has no term: `= ANY`, `@>`, and `=` are then provably empty
  (every query element must be present), and `&&` simply drops it. Cross-width `@>`/`&&` do not
  resolve (`42883`), so only `= ANY` and `=` reach this.
- **Interval terms.** An interval element's term is its 16-byte `interval-span-i128` key
  ([encoding.md §2.10](encoding.md)) — the signed 128-bit span `(months·30 + days)·86_400_000_000 +
  micros`, PG's `interval_cmp_value` — so term equality is exactly SQL interval `=`, and PG's
  `array_ops` (whose `interval` element comparison is that span) agrees: `1 mon`, `30 days`, and
  `720:00:00` are ONE term. A row storing `{30 days}` is in the posting list a query `'1 mon' =
  ANY(col)` gathers, and `{1 mon,30 days}` extracts ONE term (the dedup of §4 is over canonical
  bytes). Only the term canonicalizes: the stored value keeps each interval's own three fields, and
  the residual recheck (§6) compares by span too, so the rows are PG's. Interval compares only with
  interval, so there is no cross-type query value to convert. The interval key width (16) is not
  its value-codec width — the gather recovers each storage key by skipping the *term's* own length
  (§4), never a per-type value width.
- **Numeric terms.** A decimal element's term is its value-based `decimal-order-preserving` key
  ([encoding.md §2.5](encoding.md)), so `1.5`, `1.50`, and `1.500` are one term (§4.3), as
  PostgreSQL's `numeric` equality makes them one value. An integer and a decimal compare by value
  (`2 = 2.0`), and jed resolves `c = ANY(col)` with `c` an integer against a `decimal[]` column, or a
  decimal against an integer array, without converting the operand. So the query value is converted
  to the element type before its term is taken: an integer becomes the decimal of the same value
  exactly; a decimal becomes an integer only when it is integral and within the element's range
  (`2.0` → `2`), and otherwise equals no element, so `= ANY` is provably empty. An integer outside a
  narrower integer element's range (`5000000000` against `i32[]` from a sibling column) likewise
  equals no element. Only `= ANY` reaches this: array `@>`, `&&`, and `=` across integer and decimal
  element types do not resolve (`42883` / `42804`), and floats do not compare with integers or
  decimals at all.
- `consistent` is **the residual operator itself** — see §6: this slice always re-applies
  the original `@>`/`&&` predicate to each candidate row as the residual `WHERE` filter
  (the standard bounded-scan contract — [indexes.md §5](indexes.md)), rather than trusting
  the gather to be exact. For `array_ops` the gather *is* exact, so the residual filter
  only ever confirms; keeping it (a) costs one `operator_eval` per candidate, already in
  the cost model, and (b) seats the **lossy** opclasses — the `jsonb_ops` gather (§2.1) is a
  superset and *must* recheck. "Recheck always" is the v1 posture (§9).

### 2.1 `jsonb_ops` (a `jsonb` column)

PostgreSQL's default `jsonb` GIN operator class, term-for-term (`gin_extract_jsonb` /
`gin_extract_jsonb_query` in `jsonb_gin.c`), with jed's own byte form for each term (§4.1).

- `extract_index_terms(doc)` walks the **whole document** (every nesting level) and emits:
  - a **key** term for every object member's **key**;
  - a **key** term for every **string** that is an **array element** — including a top-level
    scalar string, which PostgreSQL stores as a one-element raw-scalar array (this is why `?`
    matches a string array element and an equal top-level string, and why the index finds them);
  - a **value** term for every other **scalar** — an object member's scalar value (a `null`,
    boolean, number, or **string** value term), and a non-string array element or top-level
    scalar (a `null`, boolean, or number value term).

  Containers (objects, arrays) produce no term of their own; only their keys and scalars do. The
  term set is deduplicated by its bytes (§4). A SQL NULL column value, `{}`, `[]`, and a document of
  only empty containers yield **zero** terms.
- `extract_query_terms(Q, strategy)`:
  - `@>` (`json_contains`, `col @> Q` or `Q <@ col`): `extract_index_terms(Q)`, mode **ALL**. A NULL
    `Q` is provably empty; a `Q` with **no** terms (`{}`, `[]`, `[[]]`, …) **falls back to the full
    scan** (PostgreSQL's `GIN_SEARCH_MODE_ALL`): `{}` is contained by every object, and the index
    cannot enumerate term-less rows.
  - `?` (`json_has_key`, `col ? k`): the single key term of `k`, mode **ALL**. A NULL `k` is
    provably empty.
  - `?|` (`json_has_any_key`, `col ?| K`): the key terms of `K`'s **non-NULL** elements, mode
    **ANY**. A NULL `K`, or a `K` with no non-NULL element, is provably empty.
  - `?&` (`json_has_all_keys`, `col ?& K`): the key terms of `K`'s **non-NULL** elements, mode
    **ALL**. A NULL `K` is provably empty; a `K` with no non-NULL element (`'{}'`, `'{NULL}'`)
    falls back to the full scan (`?& '{}'` is TRUE for every non-NULL document).
- **Soundness.** For every row where the operator is TRUE, every query term is among the row's
  terms, so the ALL/ANY gather is a **superset** of the matching rows and the residual recheck makes
  it exact. For `@>` this rests on PostgreSQL's containment rule ([json-sql-functions.md
  §1.2](json-sql-functions.md)): each contained key must appear in the containing object, each
  contained scalar must *equal* a scalar at the corresponding position — a member value of the same
  kind, or a direct array element — and only the **top level** admits an array containing a bare
  scalar. Every one of those matches lands on an identical term. For `?`/`?|`/`?&`, a top-level key,
  string array element, or equal top-level string is a key term by construction.
- **Lossy, always rechecked.** The gather ignores structure (a key found at another nesting level, a
  value under a different key) and string terms may be truncated (§4.1), so candidates include false
  positives; the residual operator removes them. PostgreSQL marks every `jsonb_ops` strategy
  `recheck = true` as well.
- **Not accelerated:** the jsonpath operators `@?` / `@@` (PostgreSQL's `jsonb_ops` serves them
  through a path-to-terms extraction — a follow-on, §10), `=`, and every other `jsonb` operator.
  `jsonb_path_ops` (hashed paths, `@>` only) is not offered.

## 3. DDL semantics (PG-matched where the surface overlaps; oracle-probed)

`USING` is **not reserved** (CLAUDE.md §3 / [grammar.md §30](grammar.md)): it is recognized
positionally after the table name (before the column list, PostgreSQL order), so a table or
column may still be named `using`. The method name (`gin` / `btree`) is a bare identifier resolved at execution,
like a type name.

**Validation order at `CREATE INDEX … USING gin`** (deterministic, extends
[indexes.md §2](indexes.md)):

1. The table must exist — **42P01**.
2. The access method must be known — `gin` or `btree`; anything else is **42704**
   (`undefined_object`, "access method does not exist: {name}"). PG's code for an unknown
   access method.
3. The key column must exist — **42703** — and satisfy the opclass:
   - a **`jsonb`** column takes `jsonb_ops` (§2.1);
   - any other **non-array** column (including `json`) has no GIN operator class — **42704**
     (PG's "data type … has no default operator class for access method gin"; jed matches the
     code, house-style message);
   - an array whose element type is **not admitted** is **0A000** this slice. The admitted
     element types are the integers (`i16`/`i32`/`i64`), `boolean`, `uuid`, `date`, `timestamp`,
     `timestamptz`, `f32`, `f64`, `interval`, `text`, `bytea`, and `decimal` — the scalar keys a
     `PRIMARY KEY` / ordered-index key column accepts ([indexes.md §1](indexes.md)), because a GIN
     term is that key encoding (§4, §4.2, §4.3). Arrays of composite, `jsonb`, and other non-scalar
     elements stay **0A000** (§10; PG *would* build them, a documented divergence recorded in the
     oracle override ledger). A `text[]` column with any collation is admitted; its terms do not
     depend on the collation (§4.2).
4. **`UNIQUE` with `USING gin`** is **0A000** (uniqueness is undefined for an inverted
   index — §1; PG raises `0A000` too: "access method gin does not support unique indexes").
5. **More than one key column** with `USING gin` is **0A000** this slice (PG *would* build a
   multicolumn GIN — divergence, override).
6. The explicit name, if any, is checked against the relation namespace — **42P07** (§
   [indexes.md §2](indexes.md), unchanged).

**`ALTER COLUMN … TYPE`** re-applies step 3 to every surviving index that keys the column
([alter.md §3.3](alter.md)): retyping a GIN-indexed column to a type with no GIN operator class is
**42704**, to a deferred array element type **0A000** — PostgreSQL likewise rebuilds the index under
the new type and fails the same way. (The same gate applies per index kind: an ordered index's
column must stay key-encodable, `0A000`; a GiST column must keep a GiST operator class, `42704`/
`0A000`.)

**Auto-naming** is unchanged from the ordered index — `<table>_<col>_idx` with the smallest
free suffix (the access method does not change the name; PG agrees). **`DROP INDEX`** /
**`DROP TABLE`** treat a GIN index exactly as any index ([indexes.md §2](indexes.md)). DDL
cost: `CREATE INDEX … USING gin` charges its build **scan** (§6, identical to the ordered
build — the table scan, not the unmetered entry writes); `DROP INDEX` charges 0.

## 4. The index entry: terms, no payload

A GIN index is a B-tree of **entries** ([indexes.md §3](indexes.md) — same empty-payload
records, same pages, same splits), but with **many entries per row**: one per term the
opclass extracts. An entry key is:

```
entry_key = encode_element(term) ‖ row_storage_key
```

- For `array_ops`, `encode_element(term)` is the element type's order-preserving key encoding
  ([encoding.md §2.1](encoding.md)) — fixed-width for every admitted element type but `text`,
  `bytea`, and `decimal`, whose keys self-delimit too, also after the 128-byte cut (§4.2, §4.3), so
  the term needs **no presence tag and no extra framing** (a GIN term is never NULL — §2). It is the *same* per-type key encoder a `PRIMARY KEY` /
  ordered-index key column uses, so a `uuid[]` term is the 16 raw bytes, a `date[]` term the
  i32 sign-flipped day count, a `timestamp[]` term the i64 sign-flipped microseconds, an
  `f64[]`/`f32[]` term the canonicalized `float-order-preserving` bits (§2 *Float terms*), an
  `interval[]` term the 16-byte `interval-span-i128` span (§2 *Interval terms*) — each
  already byte-pinned by that type's key-encoding vectors and PK goldens; a `text[]` term is the
  `C` text key of the element's first 128 bytes (§4.2); a `bytea[]` term the bytea key of its first
  128 bytes and a `decimal[]` term the first 128 bytes of its value-based key (§4.3).
- The **row storage key** (the encoded PK, or the synthetic-rowid key) is the **suffix**,
  exactly as in an ordered index: it makes every entry unique, names the row, and — since
  every term self-delimits — is recovered by skipping the term. So all rows
  carrying a term `t` are the **contiguous entry range** `[encode_element(t),
  byte-successor(encode_element(t)))` — the term's *posting list*, read by one prefix range
  scan. No separate posting-tree structure exists; the inverted index *is* the ordered
  entry B-tree.

### 4.1 `jsonb_ops` term bytes

A `jsonb_ops` term is a flag byte then a body:

```
term  = flag (u8) ‖ body
flag  = 0x01 key │ 0x02 null │ 0x03 boolean │ 0x04 number │ 0x05 string value
        (│ 0x10 on a key / number / string whose body was truncated)
body  = null:    (empty)
        boolean: 0x00 false │ 0x01 true
        key, string value: text-terminated-escape (encoding.md §2.4) of the first
                           min(n, 128) bytes of the string's UTF-8 encoding
        number:  text-terminated-escape of the first min(n, 128) bytes of the value's
                 decimal-order-preserving key (encoding.md §2.5)
```

The flag values are PostgreSQL's `JGINFLAG_KEY`/`NULL`/`BOOL`/`NUM`/`STR` and its `JGINFLAG_HASHED`
bit. A **number** term encodes the *value*, not the display scale, so `1.5` and `1.50` share a term
(numeric `=`), as PostgreSQL's normalized numeric text does. A string body is the exact UTF-8 (the
`C` collation, json.md's string equality).

**Truncation (128 bytes).** A body source longer than 128 bytes keeps only its first 128 bytes and
sets `0x10` in the flag. Equal values truncate identically, so the term stays sound (a superset;
the recheck removes the collisions), and every term is at most `1 + 2·128 + 2` bytes, so a GIN entry
always fits a node: a huge string or number in a document never makes its row unindexable.
PostgreSQL hashes over-long strings (`JGIN_MAXLENGTH`); jed truncates because a prefix needs no
cross-core hash function and is byte-identical by construction. The cut is at a byte, not a
character boundary — a term is bytes, never decoded.

**Self-delimiting.** A `null` term is one byte, a `boolean` term two, and every other body ends in
the `0x00 0x01` terminator that never occurs inside an escaped body, so no term is a proper prefix of
another. The posting list of a term `t` is therefore the entry range `[t, byte-successor(t))`, and
the storage key is the entry's suffix after `t` — exactly the `array_ops` gather (§6), now with
variable-width terms.

### 4.2 `text[]` element terms

A `text` element's term is its `C` text key — `text-terminated-escape` over the UTF-8 bytes
([encoding.md §2.4](encoding.md)) — taken over at most its **first 128 bytes**:

```
term = text-terminated-escape(utf8(element)[0 .. min(n, 128)])
```

- **Not the collation's sort key.** A collated `text[]` column (`COLLATE "unicode"`) still keys its
  terms by raw bytes. Every collation jed ships is deterministic, so text `=` — and therefore `@>`,
  `&&`, `= ANY`, and array `=` element matching — is byte identity under any collation
  ([collation.md §6/§7](collation.md)), and byte terms are exact for it. A GIN index therefore never
  depends on a collation table: a collation upgrade leaves its entries unchanged, and the skew
  guard has nothing to check. (A future non-deterministic collation would need a different term,
  [collation.md §14](collation.md).)
- **The 128-byte cut.** For an element of at most 128 bytes the term is exactly its text key. A
  longer element keeps its first 128 bytes, cut at a byte, never a character boundary. Every element
  sharing those bytes shares the term, including the 128-byte string that equals them. The gather
  is then a superset that the residual operator rechecks, and an entry always fits a node, so a
  long string never makes its row unindexable. Unlike `jsonb_ops` (§4.1) there is no truncation
  flag: a cut term and the term of the identical 128-byte prefix are the same, and the recheck
  separates them. PostgreSQL's `array_ops` stores whole elements and has no cut.
- **Self-delimiting.** The `0x00 0x01` terminator never occurs inside an escaped body, so no term is
  a proper prefix of another; the posting list and storage-key suffix rules are the ones above.

### 4.3 `bytea[]` and `decimal[]` element terms

Both are variable-width keys cut, like text, so an entry always fits a node:

```
bytea term   = bytea-terminated-escape(element[0 .. min(n, 128)])
decimal term = decimal-order-preserving(element)[0 .. min(k, 128)]
```

- **`bytea`** ([encoding.md §2.6](encoding.md)) is text's rule over raw bytes: the first 128 bytes of
  the value, each `0x00` escaped to `0x00 0xFF`, then the `0x00 0x01` terminator. bytea `=` is byte
  identity, so the term is exact up to the cut. An embedded or trailing `0x00` byte is part of the
  term, never its end, and the empty value `\x` is a term (`00 01`). A term is at most `2·128 + 2`
  bytes.
- **`decimal`** ([encoding.md §2.5](encoding.md)) keeps the first 128 bytes of the element's whole
  key, with no re-escaping. The key encodes the value, not the display scale, so `1.5`, `1.50`, and
  `1.500` share a term and `0` at any scale is the one byte `04`. A key is 5 header bytes, one byte per
  base-100 digit pair, and a terminator, so a value with up to 244 significant digits keeps its whole
  key; a longer one keeps its first 123 pairs (246 digits) and loses its terminator. Values that agree
  on sign, exponent, and those digits share the term, and the recheck separates them.
- **Self-delimiting after the cut.** A bytea term ends in its terminator, as text's does. A decimal
  term is either a whole key, which ends in its terminator (`00`, or `FF` for a negative), or exactly
  128 bytes. A whole key cannot be a prefix of a cut one, because the cut key's byte at that position
  is a digit pair, never the terminator (pairs are `01`–`64`, `9B`–`FE` complemented). A cut key
  cannot be a prefix of a whole key, which is at most 128 bytes. So no term is a proper prefix of
  another, and the posting-list and storage-key rules of §4 hold.
- **No flag**, as for text: a cut term and a whole key with the same 128 bytes cannot both occur for
  decimal (a whole key of exactly 128 bytes ends in its terminator), and for bytea the recheck
  separates the 128-byte value from its longer extensions. PostgreSQL's `array_ops` stores whole
  elements and has no cut.

**Term extraction is a SET (dedup is mandatory, not an optimization).** A row
`{1, 1, 2}` yields the terms `{1, 2}` — one entry each. Two `1` entries for one row would
be the identical key `encode(1) ‖ storage_key` and collide on insert (the index's
uniqueness invariant — [indexes.md §3](indexes.md)); deduping the elements is what keeps
each `(term, row)` pair unique. A whole-NULL array, an empty array `{}`, and a NULL array
**column value** all yield **zero** entries — so they are absent from every posting list,
which is exactly correct for `@>`/`&&` (a row with no indexed term can neither contain a
non-empty query nor overlap anything). The same holds for `jsonb_ops`: a document's terms are
deduplicated by bytes, and a NULL, `{}`, or `[]` document has no entry.

## 5. Maintenance (every write path, phase 2)

GIN maintenance is the ordinary index two-phase write ([indexes.md §4](indexes.md))
generalized from **one** entry per row to **a set**:

- **INSERT**: after a row is stored, insert the opclass's extracted entries (zero or more)
  into the index.
- **DELETE**: after a row is removed, remove its entries.
- **UPDATE**: compute the old and new **entry sets** (`term ‖ storage_key`) and apply
  the set difference — shared entries stay untouched (the same byte-identical-dirty-set
  rule as an ordered index). Changing only the indexed array moves entries for terms that
  enter or leave its element set; assigning a primary-key member re-keys the row and moves
  every entry because every storage-key suffix changes (CLAUDE.md §11 step 6).
- **CREATE INDEX on a non-empty table** scans the table once (cost: §6), extracts every
  row's term entries, and inserts them **sorted by entry key** — ascending inserts pack the
  built tree's leaves the same way the ordered build does. The sort over the full
  `(term ‖ storage_key)` entry set is part of the byte contract: it fixes the built GIN
  tree's shape, and therefore the committed pages, across cores (CLAUDE.md §8).

Maintenance work is **unmetered**, like ordered-index maintenance ([cost.md §3](cost.md)
"What is NOT metered"); the *build scan* of `CREATE INDEX` is metered. Indexed array values
may be large/compressed/external (unlike the fixed-width ordered keys), so maintenance can
fault a value's overflow chain to read its elements — the read is part of the (unmetered)
write phase, deterministic, and cannot fail.

## 6. The planner: GIN-bounded scans (SELECT, UPDATE, DELETE)

The per-relation pushdown seam ([indexes.md §5](indexes.md), [cost.md §3](cost.md)) gains a
**third bound kind**, after the PK bound and the ordered-index equality bound. For a base
relation scanned by a **`SELECT`, `UPDATE`, or `DELETE`**, if the `WHERE` AND-chain has a conjunct
`col @> Q` (contains), `col && Q` (overlaps), `c = ANY(col)` (membership), or `col = Q`
(exact array equality) where `col` is a GIN-indexed array column — or `col @> Q` / `Q <@ col` /
`col ? k` / `col ?| K` / `col ?& K` where `col` is a GIN-indexed `jsonb` column (§2.1) — and **the
query operand is a constant** — row-independent, built only from literals and `$N` parameters
(`Q` an array literal, an `ARRAY[…]` constructor such as `ARRAY['a', $1]`, or a literal/`$N`-param
`jsonb`; `c`/`k` a literal/`$N`-param scalar or key; `K` a literal or `ARRAY[…]`-constructed
`text[]`) — the plan bounds the scan through the GIN index. A bare `$N` cannot be a whole array
operand (`col @> $1`, `col ?| $1`): a bind parameter of array type is the container-parameter
narrowing ([array.md](array.md) §4, [api.md](api.md) §5), so such a query fails before planning. **`UPDATE`/`DELETE` apply the identical bound to their target-row scan** (the gather +
residual filter that finds the rows to rewrite/remove), so the same conjunct that bounds a
`SELECT` bounds the mutation, chosen against the other candidates by the same estimate and
canonical tie order ([estimator.md §9.3](estimator.md)). The bound is over the
**pre-mutation** index
state (the `WHERE` evaluates against the old row), so the candidate set is exactly the rows the
full scan would have matched; phase 2 then rewrites/removes them and maintains every index
(the GIN entries among them) as before — the result and end state are identical to the full
scan ([indexes.md §4](indexes.md)). The array column is in the `WHERE`, hence in the touched
set, hence resolved, so GIN-entry maintenance over it stays correct. (`= ANY` is the only quantified form accelerated: the membership `c = ANY(col)` —
*not* `c = ALL(col)` or any `<>`/`<`/… quantifier, which are not a single-term posting gather;
and the indexed column must be `ANY`'s **array** operand, with `c` the scalar. Array `=` is
**commutative** — both `col = Q` and `Q = col` are recognized; the terms come from the constant
`Q` either way. `<>` is *not* accelerated.) Among several eligible GIN indexes the **lowest
name** (byte order) wins (deterministic; cost-based selection is later). Otherwise the existing
PK/ordered/full choice stands.

**Execution** — the classic inverted gather + residual filter:

1. The opclass extracts the query terms and mode (§2; `jsonb_ops`' own provably-empty and
   full-scan-fallback shapes are listed in §2.1). Several `array_ops` **provably-empty** shapes
   read nothing (like an ordered equality against NULL): `Q` is the NULL array (`@>`/`&&`/`=`
   all → NULL for every row); `Q` contains a NULL element under `@>` (a NULL is found in no
   row → never TRUE — but **not** under `=`, see below); `&&` whose `Q` has no non-NULL element;
   and **`= ANY` whose `c` is NULL** (`NULL = ANY(col)` is NULL/FALSE for every row → never TRUE
   — reachable as a typed `NULL::i32`, since a bare untyped `NULL` operand is `42P18`); and a
   cross-width float query element **no element can equal** — an `f64` that is not exactly an
   `f32`, against an `f32[]` index (§2 *Float terms*) — under `= ANY`, `@>`, or `=` (under `&&`
   that element is merely dropped from the term set). (An
   out-of-element-range `c` cannot reach the gather: jed coerces `c` to the element type at
   resolve, rejecting it `22003` — a documented divergence from PG, which promotes the elements to
   `c`'s wider type and returns no rows; the gather range-checks `c` defensively regardless, §2.)
   Two shapes **fall back to the full scan** (rows the index cannot enumerate, because they carry
   no terms): `@> '{}'` (the empty array is contained by every non-NULL array); and **array `=`
   whose `Q` has no non-NULL element** — `col = '{}'` (matches the empty-array rows) and
   `col = ARRAY[NULL,…]` (matches whole-NULL-element rows), both of which have zero index entries.
   Array `=` with a NULL *element* but ≥ 1 non-NULL element does **not** fall back and is **not**
   empty — it gathers the non-NULL terms and lets the residual `=` confirm (`col = ARRAY[1,NULL]`
   gathers `@> {1}`, finds `{1,NULL}`, confirms). These are degenerate constant queries; the common
   case proceeds.
2. **Mode ANY** (`&&`, `?|`): each query term's posting list is read whole by a **prefix range
   scan** of the entry B-tree (§4), and the lists are **unioned**. **Mode ALL** with one term (`=
   ANY`, `?`, a one-term `@>`) reads that term's list whole. **Mode ALL with two or more terms** is
   the **skip-scan intersection** (§6.2), which reads only the shortest list whole and probes the
   others at the surviving candidates.
3. The result is the **candidate** storage-key set, in ascending storage-key order.
4. Each candidate row is fetched from the table tree by **point lookup**, in storage-key
   order (so downstream order-determinism is unchanged), and the original `@>`/`&&` predicate
   stays the **residual `WHERE` filter**, re-applied to every fetched row — the standard
   bounded-scan contract. The bound only narrows which rows are fetched; the result is always
   correct.

### 6.2 The ALL-mode skip-scan intersection

An intersection needs only the rows every term carries, so it never needs the whole of a long
posting list — the common jsonb key under `doc @> '{"n": 7}'`, or a frequent array element. The
gather is, for distinct query terms `t₁ … t_k` (`k ≥ 2`):

1. **Order the terms.** Each term's **span** is the number of entry-tree nodes its posting range
   `[t, byte-successor(t))` overlaps — the overlap-node count a full read of that list would charge as
   `page_read`, computed from the resident tree skeleton without reading a leaf (the estimator's
   count, [estimator.md §8.1](estimator.md)). Terms are ordered by ascending span, ties by ascending
   term bytes. Computing spans is **unmetered** planning work bounded by `k ×` the tree height.
2. **Read the driver.** The first term's posting list is read whole, as a prefix range scan, charging
   its overlap nodes as `page_read` and each entry as `gin_entry`. Its storage keys, ascending, are the
   candidates `C`.
3. **Probe each following term in order**, while `C` is non-empty. Walk `C` ascending, cutting it
   into **leaf groups**: a group starts at the next unprocessed candidate `c_i` and extends to
   `c_{i+1}, c_{i+2}, …` while the inclusive range `[t ‖ c_i, t ‖ c_j]` still overlaps a single leaf —
   its overlap-node count equals the entry tree's height (again skeleton-only and unmetered). Each
   group reads its one leaf: it charges the root-to-leaf path (the tree height) as `page_read` once.
   It then matches its candidates by the cheaper of two leaf-local walks: probing each candidate for
   its exact entry `t ‖ c`, or merging with the entries of `t` in `[t ‖ c_first, t ‖ c_last]` (both
   ends inclusive). It charges one `gin_entry` per step: `min(candidates in the group, entries of t
   in that range)`. A candidate survives when its entry exists. The survivors become `C`.
4. **Stop early.** Once `C` is empty the remaining terms are not read and charge nothing.

The result is exactly the intersection of the posting lists, in storage-key order, so rows and
emission order are unchanged; only `page_read` and `gin_entry` change. Every input — the skeleton,
the height, the term bytes, the storage keys — is cross-core identical, so the order, the groups, and
the charges are a byte contract like the rest of the gather (§8). A probed term costs at most one
`gin_entry` per candidate rather than one per posting entry, and never more than the entries of its
own list between the candidates, plus one root-to-leaf path per leaf its candidates land in. The
candidates are a subset of the driver's list, so a common term costs no more than the driver does,
and over a one-leaf entry tree no intersection costs more than reading every list whole. This is PostgreSQL's GIN "fast scan" idea,
advancing long lists by the short one's keys, expressed as leaf-local probes of the entry tree.

**Narrowings this slice** (documented, relaxable, each a follow-on with its own NoREC
obligation — [conformance.md §8](conformance.md)): ordinary single-relation/mutation bounds require
a **constant** query operand; join INL additionally admits the bare earlier-sibling operand in §6.1,
but not an expression, the indexed relation's own column, a later sibling, or a correlated outer
operand. The accelerated operators are **`@>`, `&&`, `= ANY`, and array `=` only** (no `<@` or
`IN` over a scalar list). With a non-blocking `LIMIT`, posting-list gather/combine remains complete
and fully charged, then storage-key-ordered table point-lookups and residual work stop at the window
(mutations have no `LIMIT`).

### 6.1 Join sibling query operands

For an INNER/CROSS/LEFT join's right base relation, the same GIN strategies (the four `array_ops` and
four `jsonb_ops` shapes) may take their query
operand from a **bare column of an earlier sibling relation**. This is the opclass form of the
index-nested-loop rule: evaluate the sibling value once for the current combined left row, perform
the ordinary posting gather, fetch candidates in storage-key order, and reapply the complete ON and
WHERE predicates. NULL, empty, NULL-containing, and duplicate-term values take exactly the
provably-empty/full-scan-fallback paths above **per outer row**. A fallback scans the inner for that
outer row; it never turns an unsupported runtime value into an unsound prune. An empty/miss candidate
set on a LEFT join produces the normal NULL extension.

Detection reads ON before WHERE and requires the query node itself to be that bare earlier column.
An inner array column and a later sibling are unavailable and remain full-scan shapes. PK and ordered
B-tree INL have precedence; then GiST precedes GIN, matching SELECT bound precedence. A usable GIN
sibling bound overrides an ordinary once-materialized constant bound because it must be rebuilt for
each outer row. Capability `query.gin_index_nested_loop` and
`suites/joins/gin_index_nested_loop.test` pin rows, cost, rejection gates, precedence, EXPLAIN, and
the join top-N composition.

### Cost (the cross-core contract — [cost.md §3](cost.md))

A GIN-bounded scan accrues, in place of the full-scan block:

- **`page_read` × the entry-tree nodes** each posting read overlaps — the same overlap-node rule
  as the ordered index, applied to every whole-list read (mode ANY, a single-term gather, the
  ALL-mode driver), plus the root-to-leaf path once per leaf group of the ALL-mode skip-scan (§6.2)
  (a logical count, never faulted).
- **`gin_entry` × the posting entries visited** across those reads, plus one per candidate the
  skip-scan's leaf-local match steps — the per-entry gather/combine work (intersection/union), which `page_read`
  alone (a per-*node* count)
  under-bounds when a term's posting list is long. This is the unit that makes a
  many-term or high-posting `@>`/`&&` cost proportional to its real work, so a `max_cost`
  ceiling bounds it (§13). The skip-scan's ordering spans and leaf grouping are skeleton-only and
  unmetered.
- **Per candidate row** (post-combine): `page_read` × the table-tree nodes on its point
  lookup + that row's touched-column `value_decompress` slabs + **`storage_row_read`** —
  i.e. each candidate fetch costs exactly a PK point lookup of that row.
- **The residual filter** (`operator_eval` for the `@>`/`&&` recheck per candidate) and
  **`row_produced`**, unchanged.

A provably-empty bound charges nothing; the `@> '{}'` fallback charges the full scan it
falls back to. A **GIN-bounded `UPDATE`/`DELETE`** accrues this **same scan block** (the gather
+ per-candidate point lookup + residual `operator_eval`), in place of its full-scan block — so
a `DELETE … WHERE col @> Q` costs the same scan as the matching `SELECT … WHERE col @> Q`,
minus the `row_produced` a bare mutation does not emit (a `RETURNING` clause restores it, plus
its projection's units); the phase-2 rewrite/remove and index maintenance are **unmetered**
writes (as for any mutation — [cost.md §3](cost.md)). `CREATE INDEX … USING gin` charges its
build scan — `page_read` × the
table's full node count + `storage_row_read` per row (the build's touched set is the array
column, which **can** be large, so its overflow-chain `page_read` and `value_decompress`
terms are *not* structurally zero, unlike the fixed-width ordered build); an empty table
charges 0. `DROP INDEX` charges 0.

## 7. Persistence (`format_version` 13)

The catalog index entry ([../fileformat/format.md](../fileformat/format.md)) gains a one-byte
**`index_kind`** discriminator between the `index_flags` byte and the `index_root_page`:

```
… key_ordinal × key_col_count ‖ index_flags (u8) ‖ index_kind (u8) ‖ index_root_page (u32)
```

`index_kind`: `0` = ordered B-tree (the v6 behavior), `1` = GIN; `2…` reserved (a set value
> 1 is `XX001`). An ordered index writes `index_kind = 0`, so every table entry grows by one
byte. A GIN index's `index_flags` always has bit0 (`unique`) clear (§1). Everything else —
the index is an on-disk B-tree of empty-payload records, allocated/split/committed/reclaimed
exactly as a table tree — is unchanged ([indexes.md §6](indexes.md)); a GIN entry's *key
bytes* differ (term ‖ storage-key, §4), but the page/record framing does not. Each version
is a clean break (CLAUDE.md §1); a reader accepts only version 13.

`jsonb_ops` needs no format change: the operator class follows from the indexed column's catalog
type (`jsonb` → `jsonb_ops`, an array → `array_ops`), so a reader recovers it without a new byte.
`fixtures/gin_jsonb_table.jed` pins its entry bytes: a GIN index over a `jsonb` column whose rows
hold nested keys, string array elements (key terms), a boolean, a JSON null, `1.5`/`1.50` (one
number term), a top-level string, a negative number, a term shared by two rows, `{}`, and a SQL NULL.
A cut (over-128-byte) term's entry exceeds the 256-byte fixture page's record limit, so the cut form
is pinned by [../encoding/gin_jsonb_terms.toml](../encoding/gin_jsonb_terms.toml) instead, which every
core checks.

**Golden fixture.** `fixtures/gin_array_table.jed` ([../fileformat/format.md](../fileformat/format.md)
*Fixtures*): a table with an `i32[]` column and a `USING gin` index over it, rows holding
multi-element / duplicate-element / empty / NULL arrays (exercising term dedup and the
zero-entry cases), beside one ordinary ordered index (`index_kind = 0`) in the same catalog
(the per-index kind byte). Byte-identical across Rust, Go, TS, and the Ruby reference (the §8
cross-core round-trip). `fixtures/gin_uuid_table.jed`, `fixtures/gin_float_table.jed`, and
`fixtures/gin_interval_table.jed` (and `fixtures/gin_text_table.jed` — empty, multibyte, duplicate, and NULL elements, §4.2; the cut form cannot fit the 256-byte page's
record limit and is pinned cross-core by the `query/gin_text_elements.test` costs; and
`fixtures/gin_decimal_bytea_table.jed` — `1.5`/`1.50` as one decimal term, a zero at scale 2, an
embedded `0x00`, the empty bytea, and a duplicate, §4.3, whose cut forms are pinned by the
`query/gin_decimal_elements.test` and `query/gin_bytea_elements.test` costs) pin the
non-integer element terms the same way — the float one
the canonicalizing float key (§2 *Float terms*): `{1.5,-0,0}` stores three values but two entries,
`{NaN,Infinity,NaN}` two; the interval one the span key (§2 *Interval terms*): `{1 mon,30
days,720:00:00}` stores three distinct values but ONE entry.

## 8. Determinism (the byte contract)

A GIN index is byte-identical across cores by construction: the **term set** is a pure
deterministic function of the row (the opclass extracts the same distinct elements
everywhere), the **term encoding** is the shared order-preserving element key encoding, and
the **entry B-tree** inherits the ordered index's content-deterministic split contract — so
the same mutation sequence produces the same entries and the same tree. The build's
sort-before-insert (§5) fixes the from-scratch shape. No new determinism-ledger entry is
needed ([determinism.md](determinism.md)): every input to a GIN index (array elements, their
key bytes, storage keys) is already in-contract — including every `jsonb_ops` term, which is a
pure function of the canonical document (exact decimal numbers, canonical key order). That includes a float element: a float **at
rest** — its stored bits, total order, and canonicalizing key bytes — is cross-core byte-identical
([encoding.md §2.8](encoding.md) "Why lifting the narrowing is sound"); only a *tainted* float (a
transcendental result) stored into the column carries the existing `float-transcendental` ledger
entry into the index, exactly as it does into a float `PRIMARY KEY`. The gather's
intersection/union is over storage-key **sets**, so its result is order-insensitive and the
final row order is governed by `ORDER BY` exactly as any scan (CLAUDE.md §8).

**Estimates.** Without statistics a GIN candidate takes the `matching` default selectivity
([estimator.md §7](estimator.md)). `ANALYZE` collects each term-eligible column's most common GIN
terms ([statistics.md §4.5](statistics.md)), after which a literal query's terms give the candidate's
rows, `gin_entry` reads, and index pages, and the predicate's logical selectivity
([statistics.md §6.1](statistics.md)).

## 9. Divergences from PostgreSQL (documented per CLAUDE.md §1)

- **No system-catalog surface** — PG exposes GIN indexes via `pg_indexes` / `pg_am`; jed has
  none (auto-names are observable via `DROP INDEX`/collisions and the host catalog in
  per-core tests), as for the ordered index ([indexes.md §7](indexes.md)).
- **Element-type subset** — PG's `array_ops` GIN indexes an array of any element type with a
  btree operator class; jed covers every scalar key type: the integers, `boolean`, `uuid`, `date`,
  `timestamp`, `timestamptz`, `f32`, `f64`, `interval`, `text`, `bytea`, and `decimal` (§3).
  Arrays of composite, `jsonb`, and other non-scalar elements are `0A000` (PG builds, jed `0A000`),
  oracle-override-ledgered.
- **`text[]`, `bytea[]`, and `decimal[]` terms are cut at 128 bytes** (§4.2, §4.3) — PG keeps the
  whole element. Lossy and rechecked; unobservable in results.
- **Single-column only** — PG supports multicolumn GIN; jed is `0A000` this slice (§3).
- **`<@` / `IN` not accelerated** — PG's `array_ops` GIN also serves `<@` (via a broad scan +
  recheck); jed runs `<@` and `IN`-over-a-scalar-list by full scan this slice (a cost/plan
  difference, never a result difference — §10). **Array `=` IS accelerated** (§1/§6) — a
  `col @> distinct(Q)` bound + the residual `=` filter; PG's `array_ops` GIN opclass includes
  the `=` strategy (`GinEqualStrategy`, strategy 4), so this **matches** PG (both can use the
  index; same rows either way). PG's `ginarrayconsistent` *also* marks the `=` strategy
  **lossy → recheck** (its comment: `array_eq` and `array_contain_compare` handle NULLs
  differently), so jed's always-recheck posture (§2) aligns with PG here exactly — unlike
  `@>`/`&&`, which PG marks non-lossy.
- **`const = ANY(col)` membership IS accelerated** (§1/§6) — a single-term `@>` gather; PostgreSQL
  full-scans it (oracle-verified: a sequential scan even with `enable_seqscan = off`). A jed
  *acceleration* divergence — same rows, lower cost — not a result difference. **Why PG cannot:** its
  planner matches a clause to an index only as `indexed_column OP value` with `OP` in the index's
  operator class. `c = ANY(col)` is a scalar `=` with the indexed column as `ANY`'s array argument,
  and `array_ops` holds only the whole-array `@>`/`<@`/`&&`/`=`. Nothing rewrites it to
  `col @> ARRAY[c]`, the spelling PostgreSQL users are told to write. **Why a rewrite would be
  wrong, and jed's bound is not:** the two differ under NULL (`42 = ANY('{1,NULL}')` is NULL,
  `'{1,NULL}' @> '{42}'` is false), so swapping one for the other changes `NOT (…)` or a
  select-list value. jed rewrites nothing. It applies the bound only to a `WHERE` conjunct, where
  NULL and FALSE both drop the row, and keeps `c = ANY(col)` as the residual recheck (§6). The gather
  is a superset because a TRUE row has an element equal to `c` and so is in `c`'s posting list.
  Exactness rests on two jed facts: `c` is converted to the element type before its term is taken
  (§2 *Float terms*), and every collation is deterministic, so text `=` is byte identity (§4.2).
  PostgreSQL's non-deterministic ICU collations would break a byte-term gather, and jed would have
  to revisit this when it adds one.
- **Recheck always** — jed re-applies the residual predicate to every candidate even though
  the `array_ops` gather is exact (§2); an overriding-reason simplification that also seats
  the future lossy opclasses. (PG marks `array_ops` `@>`/`&&` as non-lossy and can skip the
  recheck.) Unobservable in results; it only adds the (already-metered) residual
  `operator_eval`.
- **Error code on a non-indexable column** matches PG's code (42704 for a type with no GIN
  operator class, 0A000 for the deferred element types), house-style messages.
- **`jsonb_ops` over-long terms** — PostgreSQL hashes a key/string longer than 125 bytes into its
  term; jed keeps the first 128 bytes (§4.1). Both are lossy and rechecked; unobservable in results.
- **`jsonb_ops` does not accelerate `@?` / `@@`** (PostgreSQL does — §10). Same rows, by full scan.

## 10. Narrowings this slice (deferred follow-ons)

Each is `0A000`-free (simply not built) or `0A000` (rejected at DDL), relaxable as its own
vertical slice with a NoREC obligation ([conformance.md §8](conformance.md)):

- **More opclasses** — `jsonb_ops` has **landed** (§2.1); a future object/document type would be a
  new opclass behind the §2 seam; `jsonb_path_ops` (path hashes) later still.
- **`jsonb_ops` jsonpath strategies** — `col @? path` / `col @@ path`, extracting the key/value
  terms a path's equality chain requires (PostgreSQL's `jsonb_ops` strategies 15/16).
- **More array element types** — the **fixed-width key-encodable** elements (`uuid`, `date`,
  `timestamp`, `timestamptz`, `boolean`, alongside the integers) have **landed** (§3): the GIN
  term is their existing key encoding, so the inverted core was unchanged — only the DDL gate
  and the per-element encoder generalized. The binary floats `f32[]`/`f64[]` have landed the same
  way, plus the cross-width query-value conversion (§2 *Float terms*), and `interval[]` over its
  span key (§2 *Interval terms*), `text[]` over its cut `C` key (§4.2), and `bytea[]` and
  `decimal[]` over their cut keys (§4.3), with integer↔decimal query-value conversion (§2 *Numeric
  terms*). Every scalar key type is now admitted. Arrays of composite or `jsonb` elements remain
  `0A000`: each needs a term rule for a non-scalar value (a composite key exists,
  [encoding.md §2.15](encoding.md); `jsonb` has none).
- **More operators** — `<@` (contained-by, a broad scan + recheck), `IN` membership over a
  scalar list. (`const = ANY(col)` membership and array `=` have landed — §1/§6.)
- **Multi-column GIN** and general correlated / expression / same-or-later-relation query operands.
  (Bare earlier-sibling operands, GIN and ordered-index bounds for `UPDATE`/`DELETE` scans, and
  bounded LIMIT streaming have landed — §6/§6.1 and
  [indexes.md §5.1](indexes.md).)
- **Posting-list run compression** — a long contiguous run of one term's entries
  (a term present in very many rows) is stored as the raw entry sequence this slice; PG's
  posting-tree TID compression is a later storage optimization, byte-contract-pinned when it lands.
- **Recheck elision** — proving the `array_ops` gather exact and dropping the residual
  `operator_eval` for non-lossy opclasses.
