# GIN inverted indexes — design

> `CREATE INDEX … USING gin (col)`: a **generalized inverted index** — a second index
> *kind* beside the ordered B-tree ([indexes.md](indexes.md)) — that maps the **terms**
> extracted from a value to the rows containing them, so a predicate over a multi-valued
> column narrows its scan to matching rows instead of reading the whole table. This slice
> ships the **`array_ops`** operator class only, accelerating the array **containment
> `@>`** and **overlap `&&`** operators ([array-functions.md §10](array-functions.md)) over
> an **integer-element** array column. This doc is the contract all three cores implement
> in lockstep (CLAUDE.md §2); the grammar is in
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
- **One column, a fixed-width-key-encodable-element array.** A single column whose element
  type is one of the engine's keyable scalars — the integers (`i16[]` / `i32[]` / `i64[]`),
  `boolean[]`, `uuid[]`, `date[]`, `timestamp[]`, `timestamptz[]` — i.e. exactly the element
  types whose order-preserving key encoding has landed ([encoding.md §2.1](encoding.md)), the
  *same set an ordered-index / `PRIMARY KEY` key column accepts*. A multi-column GIN, and an
  array of a variable-width / not-yet-key-encoded element type (`text[]`, `decimal[]`,
  `bytea[]`, `interval[]`, `f64[]`), are `0A000` this slice (§3, §10) — the same "lift it when
  its key encoding lands" narrowing the ordered index makes for text/decimal keys.
- **Accelerates `@>`, `&&`, `const = ANY(col)` membership, and array `=`.** A `WHERE`
  conjunct `col @> const` (contains), `col && const` (overlaps), `const = ANY(col)` (the
  array spelling of membership — semantically `col @> ARRAY[const]`), or `col = const`
  (exact array equality — a `col @> distinct(const)` bound + the residual `=` filter, §6)
  against a GIN-indexed column bounds the scan to candidate rows (§6). `<@` (contained-by)
  and `IN` membership over a scalar list are **not** GIN-accelerated this slice (they still
  run, by full scan — §10).
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
battle-tested decomposition — and it is the seam a future `jsonb_ops` (jsonb keys + scalar
values as terms) or an object-type opclass slots into with **no change to the inverted
core**. This slice defines exactly one opclass:

**`array_ops`** (the only opclass this slice):

- `extract_index_terms(arr)` = the **distinct, non-NULL elements** of `arr`, flattened
  across any dimensionality, each encoded by the element type's key encoding
  ([encoding.md §2.1](encoding.md)). A NULL element and a whole-NULL/empty array produce
  **no** terms (§4).
- `extract_query_terms(Q, strategy)`:
  - `@>` (`contains`): the distinct elements of `Q`, mode **ALL**.
  - `&&` (`overlaps`): the distinct **non-NULL** elements of `Q`, mode **ANY**.
  - `= ANY` (`member`): `Q` is a **scalar** `const`, not an array — its single term
    `{ encode(const) }`, mode **ALL** (one term). A degenerate single-term `@>`: the
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
- `consistent` is **the residual operator itself** — see §6: this slice always re-applies
  the original `@>`/`&&` predicate to each candidate row as the residual `WHERE` filter
  (the standard bounded-scan contract — [indexes.md §5](indexes.md)), rather than trusting
  the gather to be exact. For `array_ops` the gather *is* exact, so the residual filter
  only ever confirms; keeping it (a) costs one `operator_eval` per candidate, already in
  the cost model, and (b) seats the **lossy** opclasses a future slice needs (a `jsonb_ops`
  `@>` gather over key/value tokens is a superset and *must* recheck). "Recheck always" is
  the v1 posture (§9).

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
   - a **non-array** column has no GIN operator class — **42704** (PG's "data type … has no
     default operator class for access method gin"; jed matches the code, house-style
     message);
   - an array whose element type is **not a fixed-width key-encodable scalar** is **0A000**
     this slice. The admitted element types are exactly the engine's keyable scalars — the
     integers (`i16`/`i32`/`i64`), `boolean`, `uuid`, `date`, `timestamp`, `timestamptz` —
     the *same set a `PRIMARY KEY` / ordered-index key column accepts*
     ([indexes.md §1](indexes.md)), because a GIN term is exactly that key encoding (§4).
     A **variable-width or not-yet-key-encoded** element type — `text[]`, `decimal[]`,
     `bytea[]`, `interval[]`, `float`/`f64[]` (floats are kept out of the key/order path —
     CLAUDE.md §8) — stays **0A000** (the lift-when-encoded narrowing — §1, §10; PG *would*
     build it, a documented divergence recorded in the oracle override ledger).
4. **`UNIQUE` with `USING gin`** is **0A000** (uniqueness is undefined for an inverted
   index — §1; PG raises `0A000` too: "access method gin does not support unique indexes").
5. **More than one key column** with `USING gin` is **0A000** this slice (PG *would* build a
   multicolumn GIN — divergence, override).
6. The explicit name, if any, is checked against the relation namespace — **42P07** (§
   [indexes.md §2](indexes.md), unchanged).

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

- `encode_element(term)` is the element type's order-preserving key encoding
  ([encoding.md §2.1](encoding.md)) — fixed-width for every element type this slice admits
  (the integers, `boolean`, `uuid`, `date`, `timestamp`, `timestamptz` — §3), so the term
  needs **no presence tag and no terminator** (a GIN term is never NULL — §2 — and a
  fixed-width term self-delimits). It is the *same* per-type key encoder a `PRIMARY KEY` /
  ordered-index key column uses, so a `uuid[]` term is the 16 raw bytes, a `date[]` term the
  i32 sign-flipped day count, a `timestamp[]` term the i64 sign-flipped microseconds — each
  already byte-pinned by that type's key-encoding vectors and PK goldens. The future
  variable-width element types reuse the terminated/escaped encodings
  ([encoding.md §2.4–§2.6](encoding.md)) when they lift.
- The **row storage key** (the encoded PK, or the synthetic-rowid key) is the **suffix**,
  exactly as in an ordered index: it makes every entry unique, names the row, and — since
  the fixed-width term self-delimits — is recovered by skipping the term. So all rows
  carrying a term `t` are the **contiguous entry range** `[encode_element(t),
  byte-successor(encode_element(t)))` — the term's *posting list*, read by one prefix range
  scan. No separate posting-tree structure exists; the inverted index *is* the ordered
  entry B-tree.

**Term extraction is a SET (dedup is mandatory, not an optimization).** A row
`{1, 1, 2}` yields the terms `{1, 2}` — one entry each. Two `1` entries for one row would
be the identical key `encode(1) ‖ storage_key` and collide on insert (the index's
uniqueness invariant — [indexes.md §3](indexes.md)); deduping the elements is what keeps
each `(term, row)` pair unique. A whole-NULL array, an empty array `{}`, and a NULL array
**column value** all yield **zero** entries — so they are absent from every posting list,
which is exactly correct for `@>`/`&&` (a row with no indexed term can neither contain a
non-empty query nor overlap anything).

## 5. Maintenance (every write path, phase 2)

GIN maintenance is the ordinary index two-phase write ([indexes.md §4](indexes.md))
generalized from **one** entry per row to **a set**:

- **INSERT**: after a row is stored, insert the opclass's extracted entries (zero or more)
  into the index.
- **DELETE**: after a row is removed, remove its entries.
- **UPDATE**: compute the old and new **term sets**; if they differ, remove the old
  entries and insert the new — if equal (the update did not change the indexed array's
  element set), leave the tree untouched (the same byte-identical-dirty-set rule as an
  ordered index). The row's storage key cannot change (the PK-assignment narrowing,
  CLAUDE.md §11 step 6), so suffixes are stable.
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
(exact array equality) where `col` is a GIN-indexed column and **the query operand is a
constant** (`Q` a literal/`$N`-param array, `c` a literal/`$N`-param scalar), the plan bounds the scan through the GIN
index. **`UPDATE`/`DELETE` apply the identical bound to their target-row scan** (the gather +
residual filter that finds the rows to rewrite/remove), so the same conjunct that bounds a
`SELECT` bounds the mutation — only the GiST/GIN precedence differs: after PK and ordered B-tree,
a mutation tries **GIN before GiST**, then the point-set fallbacks. The bound is over the
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
lowercased name** wins (deterministic; cost-based selection is later). Otherwise the existing
PK/ordered/full choice stands.

**Execution** — the classic inverted gather + residual filter:

1. The opclass extracts the query terms and mode (§2). Several **provably-empty** shapes
   read nothing (like an ordered equality against NULL): `Q` is the NULL array (`@>`/`&&`/`=`
   all → NULL for every row); `Q` contains a NULL element under `@>` (a NULL is found in no
   row → never TRUE — but **not** under `=`, see below); `&&` whose `Q` has no non-NULL element;
   and **`= ANY` whose `c` is NULL** (`NULL = ANY(col)` is NULL/FALSE for every row → never TRUE
   — reachable as a typed `NULL::i32`, since a bare untyped `NULL` operand is `42P18`). (An
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
2. Each query term's posting list is read by a **prefix range scan** of the entry B-tree
   (§4) into a set of row storage keys.
3. The posting sets are combined by mode: **ALL → intersection** (`@>`, and `= ANY`'s single
   term — an intersection of one list is that list), **ANY → union** (`&&`). The result is the
   **candidate** storage-key set.
4. Each candidate row is fetched from the table tree by **point lookup**, in storage-key
   order (so downstream order-determinism is unchanged), and the original `@>`/`&&` predicate
   stays the **residual `WHERE` filter**, re-applied to every fetched row — the standard
   bounded-scan contract. The bound only narrows which rows are fetched; the result is always
   correct.

**Narrowings this slice** (documented, relaxable, each a follow-on with its own NoREC
obligation — [conformance.md §8](conformance.md)): ordinary single-relation/mutation bounds require
a **constant** query operand; join INL additionally admits the bare earlier-sibling operand in §6.1,
but not an expression, the indexed relation's own column, a later sibling, or a correlated outer
operand. The accelerated operators are **`@>`, `&&`, `= ANY`, and array `=` only** (no `<@` or
`IN` over a scalar list). With a non-blocking `LIMIT`, posting-list gather/combine remains complete
and fully charged, then storage-key-ordered table point-lookups and residual work stop at the window
(mutations have no `LIMIT`).

### 6.1 Join sibling query operands

For an INNER/CROSS/LEFT join's right base relation, the same four GIN strategies may take their query
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

- **`page_read` × the entry-tree nodes** overlapping each query term's prefix range — the
  same overlap-node rule as the ordered index, applied **once per query term** (a logical
  count, never faulted).
- **`gin_entry` × the posting entries visited** across all term scans — the per-entry
  gather/combine work (intersection/union), which `page_read` alone (a per-*node* count)
  under-bounds when a term's posting list is long. This is the unit that makes a
  many-term or high-posting `@>`/`&&` cost proportional to its real work, so a `max_cost`
  ceiling bounds it (§13).
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

**Golden fixture.** `fixtures/gin_array_table.jed` ([../fileformat/format.md](../fileformat/format.md)
*Fixtures*): a table with an `i32[]` column and a `USING gin` index over it, rows holding
multi-element / duplicate-element / empty / NULL arrays (exercising term dedup and the
zero-entry cases), beside one ordinary ordered index (`index_kind = 0`) in the same catalog
(the per-index kind byte). Byte-identical across Rust, Go, TS, and the Ruby reference (the §8
cross-core round-trip).

## 8. Determinism (the byte contract)

A GIN index is byte-identical across cores by construction: the **term set** is a pure
deterministic function of the row (the opclass extracts the same distinct elements
everywhere), the **term encoding** is the shared order-preserving element key encoding, and
the **entry B-tree** inherits the ordered index's content-deterministic split contract — so
the same mutation sequence produces the same entries and the same tree. The build's
sort-before-insert (§5) fixes the from-scratch shape. No new determinism-ledger entry is
needed ([determinism.md](determinism.md)): unlike the float exemptions, every input to a GIN
index (array elements, integer key bytes, storage keys) is already in-contract. The gather's
intersection/union is over storage-key **sets**, so its result is order-insensitive and the
final row order is governed by `ORDER BY` exactly as any scan (CLAUDE.md §8).

## 9. Divergences from PostgreSQL (documented per CLAUDE.md §1)

- **No system-catalog surface** — PG exposes GIN indexes via `pg_indexes` / `pg_am`; jed has
  none (auto-names are observable via `DROP INDEX`/collisions and the host catalog in
  per-core tests), as for the ordered index ([indexes.md §7](indexes.md)).
- **Element-type subset** — PG's `array_ops` GIN indexes an array of any element type; jed
  covers the **fixed-width key-encodable** elements (the integers, `boolean`, `uuid`, `date`,
  `timestamp`, `timestamptz` — §3), the variable-width / not-yet-key-encoded rest `0A000`
  until their key encoding lifts. A divergence on `text[]`/`decimal[]`/etc. (PG builds, jed
  `0A000`), oracle-override-ledgered.
- **Single-column only** — PG supports multicolumn GIN; jed is `0A000` this slice (§3).
- **`<@` / `IN` not accelerated** — PG's `array_ops` GIN also serves `<@` (via a broad scan +
  recheck); jed runs `<@` and `IN`-over-a-scalar-list by full scan this slice (a cost/plan
  difference, never a result difference — §10). **Array `=` IS accelerated** (§1/§6) — a
  `col @> distinct(Q)` bound + the residual `=` filter; PG's `array_ops` GIN opclass includes
  the `=` strategy (`GinEqualStrategy`, strategy 4), so this **matches** PG (both can use the
  index; same rows either way). PG's `ginarrayconsistent` *also* marks the `=` strategy
  **lossy → recheck** (its comment: `array_eq` and `array_contain_compare` handle NULLs
  differently), so jed's always-recheck posture (§2) aligns with PG here exactly — unlike
  `@>`/`&&`, which PG marks non-lossy. **`const = ANY(col)` membership IS accelerated** (§1/§6), a single-term
  `@>` gather; here PG does *not* rewrite `= ANY(array)` onto a GIN array index (it leaves it a
  full scan), so that one is a jed *acceleration* divergence — same rows, lower cost — not a
  result difference.
- **Recheck always** — jed re-applies the residual predicate to every candidate even though
  the `array_ops` gather is exact (§2); an overriding-reason simplification that also seats
  the future lossy opclasses. (PG marks `array_ops` `@>`/`&&` as non-lossy and can skip the
  recheck.) Unobservable in results; it only adds the (already-metered) residual
  `operator_eval`.
- **Error code on a non-indexable column** matches PG's code (42704 non-array, 0A000 for the
  deferred element types), house-style messages.

## 10. Narrowings this slice (deferred follow-ons)

Each is `0A000`-free (simply not built) or `0A000` (rejected at DDL), relaxable as its own
vertical slice with a NoREC obligation ([conformance.md §8](conformance.md)):

- **More opclasses** — `jsonb_ops` (jsonb keys + scalar values as terms; the lossy-recheck
  path §2 already seats) and a future object/document type, each a new opclass behind the §2
  seam; `jsonb_path_ops` (path hashes) later still.
- **More array element types** — the **fixed-width key-encodable** elements (`uuid`, `date`,
  `timestamp`, `timestamptz`, `boolean`, alongside the integers) have **landed** (§3): the GIN
  term is their existing key encoding, so the inverted core was unchanged — only the DDL gate
  and the per-element encoder generalized. The **variable-width / not-yet-key-encoded** elements
  — `text[]`, `decimal[]`, `bytea[]`, `interval[]` — remain `0A000` until their order-preserving
  element key encodings lift ([encoding.md §2.4–§2.6](encoding.md)); composite-element arrays too.
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
