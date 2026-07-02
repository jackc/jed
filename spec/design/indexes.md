# Secondary indexes — design

> `CREATE [UNIQUE] INDEX` / `DROP INDEX`: secondary indexes as on-disk B-trees beside
> each table's primary tree, maintained on every write and used by the planner to bound a
> scan; a **unique** index additionally enforces uniqueness (§8) and is what backs the
> `UNIQUE` constraint ([constraints.md §5](constraints.md)). This doc is the contract all
> three cores implement in lockstep (CLAUDE.md §2); the grammar is in
> [../grammar/grammar.ebnf](../grammar/grammar.ebnf) +
> [grammar.md §30/§31](grammar.md), the byte layout in
> [../fileformat/format.md](../fileformat/format.md) (`format_version` 6), the entry-key
> encoding in [encoding.md](encoding.md), and the cost contract in [cost.md §3](cost.md).
> PostgreSQL semantics were pinned against the live `postgres:18` oracle (CLAUDE.md §1).

## 1. Surface

```sql
CREATE [UNIQUE] INDEX [name] ON table (col [, col ...])
DROP INDEX name
```

- **Non-unique by default.** Duplicate indexed values are expected and handled by the
  entry-key suffix (§3). The `UNIQUE` flag adds the §8 enforcement — the tree, the entry
  keys, the maintenance, and the planner treatment are otherwise identical.
- **Plain column keys only.** Each key is a bare column name. Expression keys
  (`(a + 1)`), per-key `ASC`/`DESC`/`NULLS`, partial (`WHERE`) indexes, `USING`,
  `IF NOT EXISTS`, and `CONCURRENTLY` are not in the grammar this slice (all are
  PostgreSQL features; each is a relaxable narrowing).
- **A column may be listed more than once** (`CREATE INDEX i ON t (a, a)`) — PostgreSQL
  allows it (oracle-probed), and rejecting it would be a gratuitous divergence. (Contrast
  the composite `PRIMARY KEY`, where PG itself rejects a duplicate member, 42701 —
  [constraints.md §3](constraints.md).)
- **Indexable types = key-encodable types**: the integer widths, `boolean`, `uuid`,
  `timestamp`, `timestamptz` — exactly the types a `PRIMARY KEY` accepts today. A `text` /
  `decimal` / `bytea` / `interval` / `float` key column is rejected `0A000` (the same documented
  narrowing as for PK, lifted per type when its order-preserving key encoding is exercised —
  [encoding.md §2.4–§2.6](encoding.md); `boolean`'s `bool-byte` key, §2.9, has since lifted).
  **Unlike a PK member, an indexed column may be nullable** — this is the first exercise of the
  encoding.md §2.2 presence tag (§3); for a nullable boolean index the slot is `00 00`/`00 01`
  present, `01` NULL.
- **Indexing the PK column is legal** (pointless but harmless, as in PG).

## 2. DDL semantics (PG-matched, oracle-probed)

**Namespace.** Index names live in the **relation namespace, shared with tables**
(PostgreSQL's model): a `CREATE INDEX` whose name (explicit) collides with an existing
table *or* index — case-insensitively, jed's identifier rule — is **42P07**
`relation already exists: {name}`; symmetrically `CREATE TABLE` of an existing *index*
name is the same 42P07. The 42P07 registry template is now the PG word "relation"
(covering both kinds); previously "table", a message-text-only change (matching is on
code — [../errors/registry.toml](../errors/registry.toml)).

**Validation order at CREATE INDEX** (deterministic, probed):

1. The table must exist — **42P01** (`CREATE INDEX i ON nosuch (nope)` reports the
   table, not the column).
2. Each key column, in **list order**: it must exist in the table — **42703** — and be of
   an indexable type — **0A000**. (Column validation precedes the name-collision check:
   PG reports 42703 for `CREATE INDEX dup_name ON t (nope)`.)
3. The **explicit name**, if any, is checked against the relation namespace — **42P07**.
4. An **omitted name** is derived (below) — derivation always finds a free name, so it
   cannot collide.

**Auto-naming** (PostgreSQL's `ChooseIndexName`, probed): the base is the **lowercased**
`<table>_<col>_<col>..._idx` — every listed column name in list order (duplicates
included), joined with `_`. If the base is taken (case-insensitive), try `<base>1`,
`<base>2`, … and take the smallest free suffix: `t_a_idx`, `t_a_idx1`, `t_a_idx2`.
An explicit name is stored **as written** (original case round-trips; comparisons are
case-insensitive) — same rule as table and CHECK-constraint names.

**`CREATE UNIQUE INDEX`** validates identically; its auto-name keeps the `_idx` suffix
(the `_key` spelling belongs to the `UNIQUE` *constraint* — constraints.md §5.3). Before
registration the build additionally **verifies the existing rows are unique** under §8's
rule (any duplicate non-NULL tuple → **23505**, and the index is not created).

**DROP INDEX**: the name must exist — **42704** `index does not exist: {name}` — and must
be an index, not a table — **42809** (`wrong_object_type`, new) `{name} is not an index`.
Symmetrically, `DROP TABLE` of an index name is **42809** `{name} is not a table`.
**`DROP TABLE` drops the table's indexes with it** (they live in its catalog entry and
have no independent life). A **unique** index drops like any other — including one that
backs a `UNIQUE` constraint, which drops the constraint with it (a documented PG
divergence, §7). Cost of both DDL statements: **zero** for `DROP INDEX` (a pure
catalog edit, like DROP TABLE); `CREATE INDEX` charges its build scan (§5).

## 3. The index entry: key encoding, no payload

An index is a B-tree of **entries** whose byte-ordered keys realize the index order; an
entry carries **no payload** (its record is the key alone — format.md). The entry key is:

```
entry_key = nullable-slot(col_1) ‖ nullable-slot(col_2) ‖ … ‖ row_storage_key
```

- Each indexed column value is encoded as the **encoding.md §2.2 nullable slot**: a
  1-byte presence tag (`0x00` present ‖ the type's order-preserving key bytes, `0x01`
  NULL) — **always**, even for a NOT NULL column (one uniform rule; a column's
  nullability never changes the byte layout). This is the first place §2.2 is exercised
  in stored bytes: NULL sorts **after** every present value (ascending), the PostgreSQL
  model.
- The **row's storage key** (the encoded PK, or the synthetic-rowid key of a no-PK
  table) is appended as the **suffix**. It makes every entry key unique (a non-unique
  index needs no duplicate handling in the tree), defines a deterministic order among
  equal indexed values (storage-key order), and *is* the row pointer: every prefix
  component is self-delimiting (fixed-width behind the tag today; terminated/escaped for
  the future variable-width types), so the suffix is recovered by walking the prefix —
  no payload needed.

Composition and `memcmp` order follow [encoding.md §2.3](encoding.md) unchanged.

## 4. Maintenance (every write path, phase 2)

Indexes are maintained **inside the same statement** that mutates the table, in the
write phase of the existing two-phase / all-or-nothing model — validation (coercion, NOT
NULL, CHECK, duplicate-key) only *reads* an index (the §8 uniqueness probes), never
writes one, and an index write cannot fail, so atomicity is unchanged:

- **INSERT** (both forms): after a row is stored, insert its entry into every index of
  the table.
- **DELETE**: after a row is removed, remove its entries.
- **UPDATE**: for each rewritten row, compute the old and new entry keys; if they
  **differ**, remove the old and insert the new — if they are **equal** (the update did
  not touch this index's columns), leave the tree node untouched. The skip is part of
  the contract, not an optimization detail: it keeps the copy-on-write dirty set — and
  therefore the incremental commit's written pages — byte-identical across cores
  (CLAUDE.md §8). The row's storage key cannot change (the PK-assignment narrowing,
  CLAUDE.md §11 step 6), so the suffix is stable.
- **CREATE INDEX on a non-empty table** builds the index by scanning the table once in
  key order (cost: §5), then inserting the computed entries **sorted by entry key** —
  ascending inserts take the B-tree's right-edge append split every time
  (spec/fileformat/format.md "Split point"), so the built tree packs leaves ~full instead
  of the few-percent fill that storage-key-order (random in entry-key space) insertion
  produced. The sort order is part of the byte contract: it fixes the built tree's shape,
  and therefore the committed pages, across cores (CLAUDE.md §8).

Maintenance work is **unmetered** (like sort and the commit itself — cost.md §3 "What is
NOT metered"); the *scan* side of CREATE INDEX is metered.

Indexed column values are always resident when maintenance reads them: the indexable
types are fixed-width and never spill or compress (large-values.md), so maintenance
cannot fault an `Unfetched` reference.

## 5. The planner: index-bounded scans (SELECT)

The existing per-relation pushdown seam ([cost.md §3](cost.md) "bounded scan") gains a
second bound kind. For each **base relation of a SELECT scan** (single-table, a JOIN
base table, or a correlated subquery's inner table), the plan picks, in order:

1. The **single-column PK bound**, if the WHERE AND-chain bounds the relation's PK
   (unchanged — the PK is the row's own key; it needs no second tree, supports ranges,
   and is strictly cheaper).
2. Else, an **index access-predicate bound**: among the relation's B-tree indexes, the
   one with the **lowest lowercased name** (a deterministic choice; cost-based selection
   is a later concern) that yields a non-empty **access predicate** — a maximal
   **equality prefix** on the index's leading key columns plus an **optional range** on
   the next key column (§5.1). Each prefix/range term is a `col <cmp> const-source`
   conjunct, `const-source` being a literal, `$N` param, or correlated outer / sibling
   column (the same rule as the PK bound, type-matched so a promoted comparison stays
   residual).
3. Else, the full scan.

#### 5.1 The access predicate — equality prefix + optional trailing range

A B-tree keyed on `(c₁, c₂, …, c_k)` can seek any predicate of the form "an equality on
a *prefix* of the key columns, then at most one *range* on the next column" — the
classic B-tree **access predicate**. The plan builds it by walking the index's key
columns **in key order** against the WHERE AND-chain (indexes.md §5's `const-source`
rule per column, an `=`/`<`/`<=`/`>`/`>=` conjunct with the column on either side,
`BETWEEN` desugared):

- While column `cᵢ` has an **equality** conjunct `cᵢ = const-source`, consume it into the
  **equality prefix** and advance. (Several equalities on one column must agree at exec
  time; a co-present range on a prefix column stays purely residual.)
- At the first column `c_{p}` with **no** equality but **one or more range** conjuncts
  (`<`/`<=`/`>`/`>=`), take those as the **range** and **stop** (no key column past a
  range can be seeked).
- Stop at the first column with no usable term.

The bound is used iff it is **non-empty** — at least one leading equality, or a leading
range. The chosen index is the lowest-lowercased-name index that produces one.

**Execution.** Let `P` be the encoded equality prefix — the `0x00 ‖ encode(vᵢ)` slot of
each prefix column's agreed value, concatenated ([encoding.md §2.2](encoding.md) present
slot). If any prefix equality is NULL (3VL — `col = NULL` is never true), the several
equalities on one column disagree (`a = 1 AND a = 2`), or an integer is out of the
column type's range, the scan is provably **empty** and reads nothing. Otherwise the
index tree is **range-scanned**:

- **No range column** (pure equality prefix): the range is `[P, byte-successor(P))` —
  every entry extending `P`, whatever the trailing columns hold (they are unbounded, so
  their NULLs are admitted and the residual filter decides them).
- **A range column** `c_p`: the range starts at `[P, P ‖ 0x01)` — the upper endpoint
  `P ‖ 0x01` stops **before** `c_p`'s NULL slot (tag `0x01` sorts after every present
  `0x00` slot), because a range comparison is never true for a NULL `c_p` (3VL). Each
  range term then tightens it against the term's slot `S = 0x00 ‖ encode(v)`:
  `c_p ≥ v` → lower `P ‖ S` inclusive; `c_p > v` → lower `byte-successor(P ‖ S)`
  inclusive (skip the whole `c_p = v` subtree); `c_p < v` → upper `P ‖ S` exclusive;
  `c_p ≤ v` → upper `byte-successor(P ‖ S)` exclusive. A NULL range endpoint makes the
  bound empty; an out-of-range integer endpoint drops only its own half-bound (a wider,
  still-sound scan); a contradictory pair (`c_p > 5 AND c_p < 5`) is empty.

In each admitted entry the row's storage key is recovered by skipping the **equality
prefix by its known byte length** `len(P)` and then every remaining key component —
the range column (if any) and all trailing columns — by width (each self-delimiting: a
`0x01` NULL tag alone, or `0x00` + the component type's fixed width); the suffix after
them names the row's storage key, which is fetched from the table tree by **point
lookup**, in index-entry order (= key order, then storage-key order, so downstream
order-determinism — ORDER BY tie-breaking, DISTINCT first-occurrence — is unchanged). The
WHERE stays the **residual filter**, re-applied to every fetched row: the bound only
narrows which rows are scanned, so the result is always correct even where the bound is a
superset.

**Narrowings this slice** (documented, relaxable, each a follow-on optimization slice
with its own NoREC obligation — conformance.md §8): `UPDATE` / `DELETE` scans keep their
PK pushdown but do **not** use indexes, and the **LIMIT streaming short-circuit does not
combine** with an index bound (an index-bounded scan with LIMIT takes the eager path —
its cost reads the full admitted set).

An index is **eligible for the bound only when every key column from the range column
onward** — i.e. all columns **after the equality prefix** — is a **fixed-width scalar**.
The suffix-skip above recovers the row's storage key by advancing over each such
component by its type's *fixed* width, which a **non-scalar** (range/array/composite) or
a **variable-width scalar** (`text` / `decimal` / `bytea` / `interval`) does not have.
The **equality-prefix columns may be any width** (including collated `text`) — their
slots are matched as the known encoded prefix `P`, skipped by `len(P)`, never by width;
so a multi-column index whose leading columns are variable-width is now usable **when the
WHERE pins them by equality** (`a = 'x' AND b > 3` over `(a text, b i32)` seeks; a bare
`b > 3` there does not, because `a`'s slot is then unknown and variable-width). An index
whose range column or a trailing unbounded column is variable-width is **not used for the
bound**: the query takes the full scan + residual filter (rows identical, only the cost
differs — `query/index_scan_vartail.test` pins this the cost way). Lifting the
fixed-width tail requirement is a follow-on: skip a variable-width component by its
self-delimiting length, not a fixed width.

### Cost (the cross-core contract — cost.md §3)

An index-bounded scan accrues, in place of the full-scan block:

- `page_read` × the index-tree nodes overlapping the access-predicate range (the same
  overlap-node rule as a PK bounded scan, applied to the index tree — an equality prefix
  narrows it, a range on the trailing column widens it, exactly like a PK point vs. range);
- per admitted entry: `page_read` × the table-tree nodes overlapping the **point**
  bound of that row's storage key (the root-to-row descent), plus that row's
  touched-column `value_decompress` slabs (large-values.md §14);
- `storage_row_read` per fetched row, and everything downstream (filter,
  projection, `row_produced`) unchanged.

An empty bound charges nothing. **CREATE INDEX** charges its build scan: `page_read` ×
the table's node count + `storage_row_read` per row (its touched set — the indexed
columns — is fixed-width, so the chain/decompress terms are structurally zero); an empty
table charges 0. `DROP INDEX` charges 0.

## 6. Persistence (`format_version` 6)

The catalog reshape (v5) + the unique flag (v6)
([../fileformat/format.md](../fileformat/format.md)):

- The table entry gains an explicit **primary-key ordinal list** (`pk_count` + column
  ordinals in **key order**). This retires column-flag bit0 (now reserved, written 0)
  and **lifts the composite-PK order narrowing**: `PRIMARY KEY (b, a)` is now legal —
  list order is key order, persisted independently of declaration order
  ([constraints.md §3](constraints.md)).
- The table entry gains its **index list**: per index, the name (original case) +
  column ordinals (key order, duplicates allowed) + a **flags byte** (`bit0 unique` —
  added in v6; the remaining bits are reserved, written 0 and read-validated) + the
  index tree's **root page**. Indexes are stored and held in **ascending
  lowercased-name order** (the catalog's deterministic order, like checks; also the §5
  tie-break order and the §8 violation-report order).
- Each index is an ordinary on-disk **B-tree of empty-payload records** — the same
  leaf/interior pages, split/merge rules, copy-on-write incremental commit, free-list
  reclamation, and demand-paged open as a table tree. A record is `key_len ‖ key` with
  zero value columns.

## 7. Divergences from PostgreSQL (documented per CLAUDE.md §1)

- **No system catalog surface**: PG exposes indexes via `pg_indexes`; jed has no catalog
  tables (auto-chosen names are observable via `DROP INDEX` / collision errors, and via
  the host API's catalog in per-core tests).
- PG's index machinery (btree opclasses, `USING`, collations, opfamilies) is owned
  surface jed does not implement — we own our surface (CLAUDE.md §1).
- Error **messages** differ in jed's house style (no identifier quoting); codes match.
- PG reserves `ON` (and `UNIQUE`), so `CREATE INDEX on ON t (a)` is unparseable there;
  jed keeps every word non-reserved via the grammar.md §30 lookahead (the standing
  no-reserved-words stance, as for `check` / `constraint`).
- **`DROP INDEX` of a constraint-backed unique index is allowed** and drops the `UNIQUE`
  constraint with it. PG refuses (`2BP01`, "drop constraint instead"); the overriding
  reason is structural — jed has no `ALTER TABLE … DROP CONSTRAINT`, so the index name
  is the constraint's *only* handle, and refusing would make a UNIQUE constraint
  permanent short of dropping the table.
- **UPDATE uniqueness is validated against the statement's end state**, not per-row in
  heap order — PG fails `UPDATE t SET v = v + 1` on a unique `v` where jed succeeds (§8;
  the overriding reason is the two-phase / all-or-nothing model, CLAUDE.md §11 step 6).
- When one row violates **several** unique indexes, jed reports the lowest lowercased
  name; PG reports creation order, which jed does not persist (constraints.md §5.4).

## 8. UNIQUE indexes (the enforcement)

A **unique** index (`unique = true` in the catalog — set by `CREATE UNIQUE INDEX` or by a
`UNIQUE` constraint, constraints.md §5) forbids two rows from sharing its **key-column
value tuple**. Everything else about it — entry keys (§3), maintenance (§4), planner
treatment (§5), persistence (§6) — is exactly a plain index's. Enforcement:

- **The rule (*NULLS DISTINCT* — PostgreSQL's default).** Two rows conflict iff their
  indexed tuples are equal **and every component is non-NULL**. A tuple with *any* NULL
  component never conflicts with anything (any number of `(NULL)`s, or `(1, NULL)`s,
  coexist — probed). Mechanically: a row's **uniqueness probe key** is its §3 entry key
  *prefix* (the nullable slots, without the storage-key suffix); a prefix containing a
  NULL tag is exempt, and a conflict is "another row's entry begins with the same
  prefix" (a range probe `[prefix, byte-successor(prefix))` over the index tree — the
  suffix makes tree keys unique, so equal prefixes sit adjacent).
- **INSERT** (both sources): in phase 1, per candidate row, **after** the primary-key
  duplicate check (PG reports the PK first when both are violated — probed), each
  *unique* index of the table is probed in catalog (name) order: a non-exempt prefix
  that matches an existing entry, **or one seen earlier in the same statement's batch**,
  traps `23505` naming that index (constraints.md §5.4). Nothing has been written
  (two-phase, all-or-nothing). NOT NULL (23502) and CHECK (23514) fire earlier, per the
  existing per-row order (constraints.md §4.4).
- **UPDATE**: phase 1 collects every matching row's rewritten values, then validates
  uniqueness **against the end state**, per unique index in catalog order: the new
  prefixes are checked against each other (an in-batch duplicate traps 23505) and
  against the index's existing entries **excluding the rewritten rows' own** (an entry
  whose storage-key suffix belongs to a row being rewritten is being replaced, so it
  does not conflict). So `UPDATE t SET v = v + 1` and a two-row value *swap* both
  succeed — the end state is unique — where PostgreSQL's per-row check fails on the
  transient collision (§7; PG's report depends on heap order, which jed has no analogue
  of). A genuine conflict with an untouched row traps 23505 as usual. The storage key
  cannot change (the PK-assignment narrowing), so suffixes are stable.
- **DELETE** cannot violate uniqueness; no probe.
- **`CREATE UNIQUE INDEX` on a non-empty table** verifies the §1 build's computed
  entries pairwise (the same exempt-NULL prefix rule) before the index is registered; a
  duplicate traps `23505` and creates nothing.

**Cost.** Uniqueness probes are **unmetered** validation work, like the primary-key
duplicate check and the index maintenance itself ([cost.md §3](cost.md) "What is NOT
metered") — an INSERT into a uniquely-indexed table accrues the same cost as into a
plainly-indexed one. The `CREATE UNIQUE INDEX` build charges exactly the plain build's
scan (§5); its verification adds nothing.
