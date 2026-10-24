# Secondary indexes — design

> `CREATE [UNIQUE] INDEX` / `DROP INDEX`: secondary indexes as on-disk B-trees beside
> each table's primary tree, maintained on every write and used by the planner to bound a
> scan; a **unique** index additionally enforces uniqueness (§8) and is what backs the
> `UNIQUE` constraint ([constraints.md §5](constraints.md)). This doc is the contract all
> three cores implement in lockstep (CLAUDE.md §2); the grammar is in
> [../grammar/grammar.ebnf](../grammar/grammar.ebnf) +
> [grammar.md §30/§31](grammar.md), the byte layout in
> [../fileformat/format.md](../fileformat/format.md) (`format_version` 6; **expression keys
> `format_version` 26**; **partial-index predicates `format_version` 27**, §6/§9), the
> entry-key encoding in [encoding.md](encoding.md), and the
> cost contract in [cost.md §3](cost.md).
> PostgreSQL semantics were pinned against the live `postgres:18` oracle (CLAUDE.md §1).

## 1. Surface

```sql
CREATE [UNIQUE] INDEX [IF NOT EXISTS name | name] ON table (key [ASC | DESC] [NULLS FIRST | LAST] [, ...]) [WHERE predicate]
DROP INDEX name
REINDEX [ ( option [, ...] ) ] { INDEX | TABLE } [ CONCURRENTLY ] [db.]name     -- §10
REINDEX [ ( option [, ...] ) ] { DATABASE | SYSTEM } [ CONCURRENTLY ] [ name ]
```

**`IF NOT EXISTS`** (a name is required, as in PostgreSQL) turns a relation-namespace collision
into a silent **no-op success** (PG's notice has no jed channel). PostgreSQL decides it in
`index_create`, after the whole definition is validated, so jed decides it at the same point the
bare form raises `42P07`: every definition error (§2, [gin.md §3](gin.md), [gist.md §5](gist.md),
§9) still fires, and only the build is skipped — zero cost, no `23505` build check, the existing
relation untouched. It applies identically to every index kind ([grammar.md §30](grammar.md)).

The optional **`WHERE predicate`** makes the index **partial** (§9): only rows for which the
predicate evaluates to **TRUE** are indexed (and, for a `UNIQUE` partial index, only those rows
are constrained). The predicate is a filter context ([cost.md](cost.md) §3): its top-level `AND`
chain stops at the first conjunct that is not TRUE. A partial index is **B-tree only** this slice; a `WHERE` clause on a
`USING gin` / `USING gist` index is `0A000`.

Each **key element** is one of three forms, matching PostgreSQL's `index_elem` (oracle-probed):

- a **bare column** — `email`;
- a **bare function call** — `lower(email)` (no extra parentheses needed);
- a **parenthesized expression** — `(a + b)`, `(email || '@x')`.

A general operator expression must be parenthesized (`CREATE INDEX ON t (a + b)` is a
**syntax error**, as in PG; `(a + b)` is accepted). A parenthesized bare column `(a)`
normalizes to a **column** key, not an expression (PG-matched). An index may **mix** the
three forms in one key list (`CREATE INDEX ON t (lower(email), a, (b + 1))`).

- **Non-unique by default.** Duplicate indexed values are expected and handled by the
  entry-key suffix (§3). The `UNIQUE` flag adds the §8 enforcement — the tree, the entry
  keys, the maintenance, and the planner treatment are otherwise identical.
- **Column keys may be listed more than once** (`CREATE INDEX i ON t (a, a)`) — PostgreSQL
  allows it (oracle-probed), and rejecting it would be a gratuitous divergence. (Contrast
  the composite `PRIMARY KEY`, where PG itself rejects a duplicate member, 42701 —
  [constraints.md §3](constraints.md).) Expression keys are likewise unconstrained by
  duplication.
- **Indexable types = key-encodable types.** A column key's type — or an **expression
  key's *result* type** — must be a key-encodable scalar or keyable array: every scalar is
  keyable today (the integer widths, `boolean`, `uuid`, `timestamp`, `timestamptz`, `text`,
  `decimal`, `bytea`, `interval`, `float`, `json`-family excluded — [encoding.md §2](encoding.md)),
  so the lone rejection is a **composite** result type — **`0A000`** (the same narrowing a
  composite `PRIMARY KEY` / column key carries). This is why an expression producing `text`
  (`lower(email)`) is a valid key: `text` keys landed ([encoding.md §2.4](encoding.md)).
  **An indexed column or expression may be nullable** — the slot carries the encoding.md §2.2
  presence tag (§3); NULL sorts after every present value (ascending).
- **Indexing the PK column (or a constant-over-it expression) is legal** (pointless but
  harmless, as in PG).
- **Expression keys must be immutable and self-contained.** An index expression may
  reference **only the table's own columns**, and must be a pure, deterministic function of
  them — the same purity the built-in surface guarantees (CLAUDE.md §13). At `CREATE INDEX`
  the expression is resolved and rejected if it (§2): calls a non-immutable function (the
  entropy/clock seam `uuidv4`/`uuidv7`/`now`/`current_timestamp`/`clock_timestamp`/`current_date` —
  **`42P17`**),
  resolves a **STABLE node** (the runtime `text → date` cast, whose input grammar admits the
  clock-relative specials, or a clock-relative date literal `'today'`/`'now'`/… — **`42P17`**,
  agreeing with PG's stable `date_in`; [date.md §6](date.md)),
  contains an **aggregate** (`42803`), a **window function** (`42P20`), a **subquery**
  (`0A000`), or a **bind parameter** `$N` (`42P02`). An immutable expression is a
  deterministic function of the row, so the index stays consistent with the table under the
  §8/§10 contract.
- **Per-key ordering** — `a DESC`, `b NULLS FIRST` — has landed (§11, `format_version` 46).
- **Still deferred** (each a relaxable narrowing, PostgreSQL features): `CONCURRENTLY`, and an
  explicit operator class /
  `COLLATE` on a key element. An expression key's text result uses the expression's own
  effective collation (jed's ordinary text-collation resolution), not a per-key `COLLATE`.
  (Partial (`WHERE`) indexes have **landed** for B-tree — §9.)

**Expression dependencies.** [index-dependencies.md](index-dependencies.md) is the admission and
lifetime contract for both keys and predicates. A type does not make an expression volatile:
`timestamptz IS NULL`, comparisons of instants, and explicit-zone conversions are allowed. Session
casts, clocks, and other mutable state are rejected even when nested in a null test, CASE, or array.
Named-zone data is pinned per index; a mismatch excludes the index from read planning and refuses
writes (`XX002`) until a rebuild — `REINDEX` (§10), or the equivalent transactional DROP/CREATE. Epoch extraction and explicit-zone
`make_timestamptz` receive stronger argument-sensitive admission than PostgreSQL's blanket STABLE
classification. UTC interval arithmetic retains jed's existing [interval.md](interval.md) contract.

## 2. DDL semantics (PG-matched, oracle-probed)

**Namespace.** Index names live in the **relation namespace, shared with tables**
(PostgreSQL's model): a `CREATE INDEX` whose name (explicit) collides with an existing
table *or* index — byte-exact on the name, jed's identifier rule ([grammar.md §3](grammar.md)) — is **42P07**
`relation already exists: {name}`; symmetrically `CREATE TABLE` of an existing *index*
name is the same 42P07. The 42P07 registry template is now the PG word "relation"
(covering both kinds); previously "table", a message-text-only change (matching is on
code — [../errors/registry.toml](../errors/registry.toml)).

**Validation order at CREATE INDEX** (deterministic, probed):

1. The table must exist — **42P01** (`CREATE INDEX i ON nosuch (nope)` reports the
   table, not the column).
2. Each key element, in **list order**:
   - a **column key** must exist in the table — **42703** — and be of an indexable type —
     **0A000**. (Column validation precedes the name-collision check: PG reports 42703 for
     `CREATE INDEX dup_name ON t (nope)`.)
   - an **expression key** is resolved against the table's columns (an unknown column →
     **42703**), then checked for validity in this order: an **aggregate** → **42803**, a
     **window function** → **42P20**, a **subquery** → **0A000**, a **bind parameter** `$N`
     → **42P02**, a **non-immutable** function call (the entropy/clock seam, incl.
     `current_date`) or a resolved **STABLE node** (the runtime `text → date` cast or a
     clock-relative date literal, flagged at their birth via the resolver's
     `nonimmutable` channel) → **42P17**
     (`invalid_object_definition`, *functions in index expression must be marked IMMUTABLE*).
     Finally its **result type** must be indexable — **0A000** for a composite result.
     (Aggregate/window/subquery/param rejections ride the ordinary resolver, exactly as a
     `CHECK` expression does; the immutability walk is index-specific.)
3. The **explicit name**, if any, is checked against the relation namespace — **42P07**.
4. An **omitted name** is derived (below) — derivation always finds a free name, so it
   cannot collide.

**Auto-naming** (PostgreSQL's `ChooseIndexName` / `ChooseIndexColumnNames`, probed): the
base is `<table>_<part>_<part>..._idx`, built from the **exact** names (no case change — a quoted
`"Foo"` table gives `Foo_…_idx`, as PostgreSQL) — one **name part** per key
element in list order, joined with `_`, where the part is:

- a **column key** → the **column name** (`lower(email), a` → parts `lower`, `a`);
- a **bare-function-call** expression → the **function name** (`lower(email)` → `lower`,
  `abs(a)` → `abs`);
- any **other expression** → the literal `expr` (`(a + b)` → `expr`, `(email || 'x')` →
  `expr`).

A part repeating an earlier one becomes `part1`, `part2`, … (PostgreSQL's
`ChooseIndexColumnNames`): `(a, a)` → `t_a_a1_idx`, `(lower(x), lower(x))` →
`t_lower_lower1_idx`, `((a + 1), (b + 1))` → `t_expr_expr1_idx` (probed). A base longer than the
63-byte identifier limit is cut to fit, the longer of the table name and the joined parts
shortened first, never splitting a character ([grammar.md §3.1](grammar.md)).

If the base is taken (byte-exact), try `<base>1`, `<base>2`, … and take the smallest
free suffix — so `CREATE INDEX ON t (lower(email))` then `CREATE UNIQUE INDEX ON t (lower(email))`
yield `t_lower_idx`, `t_lower_idx1` (probed). The suffix belongs to the label (`idx1`), so a cut
name re-shortens its parts to make room. An explicit name is stored as the **name** (bare spellings fold to lowercase, quoted
spellings are exact; comparisons are byte-exact — [grammar.md §3](grammar.md)) — same rule as
table and CHECK-constraint names.

**`CREATE UNIQUE INDEX`** validates identically; its auto-name keeps the `_idx` suffix
(the `_key` spelling belongs to the `UNIQUE` *constraint* — constraints.md §5.3). Before
registration the build additionally **verifies the existing rows are unique** under §8's
rule (any duplicate non-NULL tuple → **23505**, and the index is not created).

**DROP INDEX**: the name must exist — **42704** `index does not exist: {name}` — and must
be an index, not a table — **42809** (`wrong_object_type`, new) `{name} is not an index`.
Symmetrically, `DROP TABLE` of an index name is **42809** `{name} is not a table`.
**`DROP TABLE` drops the table's indexes with it** (they live in its catalog entry and
have no independent life). A **unique** index drops like any other, but one that backs a
`UNIQUE` constraint (or an `EXCLUDE` constraint) belongs to the constraint: its `DROP INDEX` is
**2BP01**, and `ALTER TABLE … DROP CONSTRAINT` drops both, as in PostgreSQL
(constraints.md §5.5). Cost of both DDL statements: **zero** for `DROP INDEX` (a pure
catalog edit, like DROP TABLE); `CREATE INDEX` charges its build scan (§5).

## 3. The index entry: key encoding, no payload

An index is a B-tree of **entries** whose byte-ordered keys realize the index order; an
entry carries **no payload** (its record is the key alone — format.md). The entry key is:

```
entry_key = nullable-slot(key_1) ‖ nullable-slot(key_2) ‖ … ‖ row_storage_key
```

- Each key element's **value** for a row is its indexed column's value (a column key) or
  the **result of evaluating the key expression against the row** (an expression key —
  §4). That value is encoded as the **encoding.md §2.2 nullable slot**: a 1-byte presence
  tag (`0x00` present ‖ the type's order-preserving key bytes, `0x01` NULL) — **always**,
  even for a NOT NULL column or a NOT-NULL-typed expression (one uniform rule; nullability
  never changes the byte layout). This is the first place §2.2 is exercised in stored
  bytes: NULL sorts **after** every present value (ascending), the PostgreSQL model. (A key
  element declared `DESC` / `NULLS FIRST` uses the corresponding §2.16 ordered slot instead — §11.) An
  expression key encodes under the **expression's resolved result type and collation**
  (e.g. `lower(email)` → the `text` order-preserving key under the expression's effective
  collation).
- The **row's storage key** (the encoded PK, or the synthetic-rowid key of a no-PK
  table) is appended as the **suffix**. It makes every entry key unique (a non-unique
  index needs no duplicate handling in the tree), defines a deterministic order among
  equal indexed values (storage-key order), and *is* the row pointer: every prefix
  component is self-delimiting (a fixed width or its own terminator behind the tag — the
  §5.1 slot rule), so the suffix is recovered by walking the prefix — no payload needed.

Composition and `memcmp` order follow [encoding.md §2.3](encoding.md) unchanged.

## 4. Maintenance (every write path, phase 2)

Indexes are maintained **inside the same statement** that mutates the table, in the
write phase of the existing two-phase / all-or-nothing model. The **entry keys are
computed in phase 1** (validation, alongside coercion / NOT NULL / CHECK / duplicate-key)
and the tree edits applied in phase 2. For a **plain column index** this is a bookkeeping
detail; for an **expression index** it is load-bearing: evaluating a key expression *can
fail* (overflow `22003`, division by zero `22012`, a domain error), and computing every
row's entry keys in phase 1 means such a failure aborts the statement **before any write**,
so all-or-nothing atomicity is preserved exactly as for CHECK. Because an index expression
is immutable (a deterministic function of the row, §1), a key that computed successfully in
phase 1 recomputes identically, so phase 2's tree edits still cannot fail:

- **INSERT** (both forms): after a row is stored, insert its entry into every index of
  the table.
- **DELETE**: after a row is removed, remove its entries.
- **UPDATE**: for each rewritten row, compute the old and new entry keys; if they
  **differ**, remove the old and insert the new — if they are **equal** (the update did
  not change either the indexed value or the row's storage key), leave the tree node
  untouched. The skip is part of the contract, not an optimization detail: it keeps the
  copy-on-write dirty set — and therefore the incremental commit's written pages —
  byte-identical across cores (CLAUDE.md §8). Assigning a primary-key member **re-keys**
  the row, so every index entry for that row moves even when its indexed value is unchanged:
  the storage-key suffix changed (CLAUDE.md §11 step 6).
- **CREATE INDEX on a non-empty table** builds the index by scanning the table once in
  key order (cost: §5), **evaluating each expression key per row** (a failure aborts the
  build before the index is registered — nothing is created, matching PG), then inserting
  the computed entries **sorted by entry key** —
  ascending inserts take the B+tree's right-edge append split every time
  (spec/fileformat/format.md "Split point"), so the built tree packs leaves ~full instead
  of the few-percent fill that storage-key-order (random in entry-key space) insertion
  produced. The sort order is part of the byte contract: it fixes the built tree's shape,
  and therefore the committed pages, across cores (CLAUDE.md §8).

Maintenance work is **unmetered** (like sort and the commit itself — cost.md §3 "What is
NOT metered"); the *scan* side of CREATE INDEX is metered. **Expression-key evaluation is
part of that unmetered maintenance work** — an INSERT/UPDATE/DELETE into an
expression-indexed table accrues exactly the cost of the same statement into a
plain-indexed one (the eval runs against an unmetered meter). The one place expression keys
touch metered cost is the **CREATE INDEX build scan**: its touched-column set is the
columns the expressions *reference* (not the fixed-width indexed columns of a plain index),
so if a referenced value is a spilled/compressed large value the build charges its
`value_decompress` slabs (large-values.md §14) — deterministic and cross-core identical.

A **plain** column index may key a variable-width column (`text`/`bytea`/`decimal`, or an
array/range/composite) whose value spilled to an overflow chain or compressed when its
*table* record exceeded `RECORD_MAX` (large-values.md §12): the index entry key holds the
full encoded value, which is admitted up to a page (a larger entry is `0A000`, from INSERT
and UPDATE alike). Once the row's leaf faults in from disk that value is lazily
**deferred**, and an UPDATE's new row carries its unassigned columns over still deferred,
so every entry builder — B-tree, GIN, GiST, and the uniqueness/FK/EXCLUDE probes — resolves
a deferred column value before encoding it, computing the same entry bytes the INSERT
wrote (a skipped deferred value would leave stale or missing entries). An **expression**
index may likewise reference a spilled column (`lower(bigtext)`); evaluating the expression
faults it in through the ordinary evaluator backstop (the lazy-record `Unfetched`
resolution). Both fetches are unmetered maintenance work, like the eval itself. Pinned by
`conformance/suites/dml/index_spilled_key.test` (its disk pass reopens before every record).
PostgreSQL rejects a btree entry over ~2.7 KB (`54000`); jed's larger key ceiling is a
deliberate divergence of its page-sized B-tree records.

## 5. The planner: index-bounded scans (SELECT)

(This is one arm of the planner's access-path precedence — the pass structure and full
rule inventory are in [planner.md](planner.md) §4/§5.)

The existing per-relation pushdown seam ([cost.md §3](cost.md) "bounded scan") gains a
second bound kind. Candidate detection remains the following complete structural inventory:

1. The **PK tuple bound**, if the WHERE AND-chain supplies a maximal leading equality prefix and
   optional next-member range (the PK is the row's own key; it needs no second tree and is strictly
   cheaper).
2. An **index access-predicate bound** for every B-tree index that yields a non-empty **access
   predicate** — a maximal
   **equality prefix** on the index's leading key columns plus an **optional range** on
   the next key column (§5.1). Each prefix/range term is a `col <cmp> const-source`
   conjunct, `const-source` being a literal, `$N` param, or correlated outer / sibling
   column (the same rule as the PK bound, type-matched so a promoted comparison stays
   residual).
3. The full scan.

For a one-base-relation SELECT, the planner chooses the minimum complete-pipeline estimated cost
among the PK candidate, every eligible B-tree/GiST/GIN/interval candidate, and full scan; exact ties
use kind then lowest index name by byte order ([estimator.md §9.1](estimator.md)). Eligible
multi-relation SELECTs feed the same access inventories into the bounded join search. UPDATE/DELETE
choose their target path from the same inventory by the same estimate and tie order
([estimator.md §9.3](estimator.md)).

#### 5.1 The access predicate — equality prefix + optional trailing range

A B-tree keyed on `(k₁, k₂, …, k_k)` — each `kᵢ` a **column or an expression** — can seek
any predicate of the form "an equality on a *prefix* of the key elements, then at most one
*range* on the next element" — the classic B-tree **access predicate**. The plan builds it
by walking the index's key elements **in key order** against the WHERE AND-chain
(indexes.md §5's `const-source` rule per element, an `=`/`<`/`<=`/`>`/`>=` conjunct with the
key operand on either side, `BETWEEN` desugared):

- **Matching a key element to a conjunct operand.** For a **column** key, the operand must
  be a reference to that column (as today). For an **expression** key, the operand must be
  **structurally equal** to the index's resolved key expression — the same operator/function
  tree over the same table columns and constants (so `WHERE lower(email) = $1` matches an
  index on `lower(email)`, but `WHERE upper(email) = …` or a re-associated `b + a` against
  `a + b` does not — this is PostgreSQL's syntactic index-expression matching, not a
  semantic prover). The comparison is on the **resolved** expression tree, so bind-param /
  correlated `const-source` operands on the *other* side are gated by the ordinary rule.
- While element `kᵢ` has an **equality** conjunct `kᵢ = const-source`, consume it into the
  **equality prefix** and advance. (Several equalities on one element must agree at exec
  time; a co-present range on a prefix element stays purely residual.)
- At the first element `k_{p}` with **no** equality but **one or more range** conjuncts
  (`<`/`<=`/`>`/`>=`), take those as the **range** and **stop** (no key element past a
  range can be seeked).
- Stop at the first element with no usable term.

The bound is used iff it is **non-empty** — at least one leading equality, or a leading
range. The chosen index is the lowest-name (byte order) index that produces one.

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
prefix by its known byte length** `len(P)` and then every remaining key component — the range
element (if any) and all trailing elements — by its **self-delimiting slot**
([encoding.md §2.2](encoding.md)): a `0x01` NULL tag alone, or `0x00` + the component's key body,
whose length its own encoding fixes:

| key form | body length |
|---|---|
| integers, `bool`, `uuid`, `date`, `timestamp[tz]`, `f32`/`f64` | the type's fixed width |
| `interval` (`interval-span-i128`, §2.10) | 16 |
| `C` `text` / `bytea` (`*-terminated-escape`, §2.4/§2.6) | through the first `0x00 0x01` (a `0x00 0xFF` pair is an escaped NUL) |
| `decimal` (`decimal-order-preserving`, §2.5) | `1` for zero (`0x04`); else the class byte, the 4-byte exponent, and the pair bytes through the terminator — `0x00` for a positive (`0x05`), `0xFF` for a negative (`0x03`) |
| collated `text` (`text-collated-sortkey`, §2.12) | 2-byte weight units through the third `0x0000` separator, then a terminated `C` text body |

A container (range/array/composite) element after the equality prefix has no slot rule here, so
such an index is not used for the bound (the full scan + residual filter; rows identical). The
suffix after the skipped slots names the row's storage key, which is fetched from the table tree by
**point lookup**, in index-entry order (= key order, then storage-key order, so downstream
order-determinism — ORDER BY tie-breaking, DISTINCT first-occurrence — is unchanged). The WHERE
stays the **residual filter**, re-applied to every fetched row: the bound only narrows which rows
are scanned, so the result is always correct even where the bound is a superset.

An `UPDATE` / `DELETE` target scan uses the same eligible access predicate as SELECT. It gathers
every admitted `(storage key, old row)` from the pre-mutation index state, rechecks the complete
WHERE, finishes validation for the whole batch, and only then changes table/index storage. Thus an
indexed-column update, a PK-rekeying update, and a partial/expression-index bound cannot perturb the
candidate walk in progress. With a non-blocking `LIMIT`, the same bound is pulled in index-entry
order and table point-lookups stop at `OFFSET+LIMIT` residual survivors. If `ORDER BY` is exactly the
same index order, its sort is also elided; an incompatible order retains eager gather + sort.

Any scalar key element may be the **range element** or a **trailing element**: a range on a
variable-width element encodes its endpoints exactly as an equality does (a collated text element's
endpoints as the column collation's sort key, so it range-scans in collation order —
[collation.md §8](collation.md)), and every element after the equality prefix is skipped by the slot
rule above. So `WHERE name > 'm'` over an index on `name text` and `WHERE a = 1` over `(a i32, b
text)` both seek. (Before the slot rule, only fixed-width elements could follow the prefix.)

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

## 6. Persistence (`format_version` 6, expression keys `format_version` 26, partial predicates `format_version` 27)

The catalog reshape (v5) + the unique flag (v6)
([../fileformat/format.md](../fileformat/format.md)):

- The table entry gains an explicit **primary-key ordinal list** (`pk_count` + column
  ordinals in **key order**). This retires column-flag bit0 (now reserved, written 0)
  and **lifts the composite-PK order narrowing**: `PRIMARY KEY (b, a)` is now legal —
  list order is key order, persisted independently of declaration order
  ([constraints.md §3](constraints.md)).
- The table entry gains its **index list**: per index, the name (exact bytes) + a
  **key-element list** (key order, duplicates allowed) + a **flags byte** (`bit0 unique` —
  added in v6; **`bit1 has_predicate`** — added in v27, §9; the remaining bits are reserved,
  written 0 and read-validated) + the index tree's **root page**, then — **only when `bit1`
  is set** (`format_version` 27) — a `u16 pred_len` + `pred_len` UTF-8 bytes of the partial
  index's **predicate canonical text** (the *Check-expression text* form). On load a partial
  predicate re-parses (text this binary cannot parse is kept, unparsed: reads skip the index and
  writes to its table are `XX003`, [unparsable-definitions.md](unparsable-definitions.md) §3.4) and re-resolves against the
  loaded table's columns per statement (never persisted-resolved — a deterministic function of
  the column types). A non-partial index writes no `bit1` and no predicate bytes and is
  byte-identical to v26. Indexes are stored and held in **ascending name byte order**
  (the catalog's deterministic order, like checks; also the §5 tie-break order and the §8
  violation-report order).
- **Each key element** is a `u16`: a **column ordinal** (`< col_count`) for a column key,
  or the sentinel **`0xFFFF`** — which cannot be a valid ordinal (`col_count ≤ 65535` ⇒
  max ordinal `65534`) — for an **expression key** (**new in `format_version` 26**),
  followed by a `u16 expr_len` and `expr_len` UTF-8 bytes of the expression's **canonical
  text** (the *Check-expression text* form format.md defines, exactly as a `CHECK` / column
  `DEFAULT` stores). On load an expression element re-parses that text with the ordinary
  expression parser (unparsable text is kept, as for a partial predicate); its result type and
  collation are re-derived by resolving it against the loaded table's columns per statement
  (never persisted — a deterministic function of column types that are themselves
  cross-core-identical). A plain column index's on-disk bytes are unchanged from v6.
- Each index is an ordinary on-disk **B-tree of empty-payload records** — the same
  leaf/interior pages, split/merge rules, copy-on-write incremental commit, free-list
  reclamation, and demand-paged open as a table tree. A record is `key_len ‖ key` with
  zero value columns.

## 7. Divergences from PostgreSQL (documented per CLAUDE.md §1)

- **No system catalog surface**: PG exposes indexes via `pg_indexes`; jed has no catalog
  tables (auto-chosen names are observable via `DROP INDEX` / collision errors, via the
  `jed_indexes` introspection relation — an expression key shows as its canonical text in
  the `columns` array — and via the host API's catalog in per-core tests).
- **Expression-index matching is syntactic** (as in PG): the planner uses an expression
  index only when a WHERE operand is structurally equal to the key expression, not
  whenever they are semantically equivalent (`b + a` does not match an index on `a + b`).
- **Partial-index implication is a bounded subset of PostgreSQL's** (§9.1): jed proves
  implication with a deterministic, step-bounded subset of `predtest.c` (no refutation, no
  cross-family constants, no `NOT` reasoning, a `$N` never a known value). A jed miss is a correct
  scan without the index.
- PG's index machinery (btree opclasses, `USING`, collations, opfamilies) is owned
  surface jed does not implement — we own our surface (CLAUDE.md §1).
- Error **messages** differ in jed's house style (no identifier quoting); codes match.
- PG reserves `ON` (and `UNIQUE`), so `CREATE INDEX on ON t (a)` is unparseable there;
  jed keeps every word non-reserved via the grammar.md §30 lookahead (the standing
  no-reserved-words stance, as for `check` / `constraint`).
- **UPDATE uniqueness is validated against the statement's end state**, not per-row in
  heap order — PG fails `UPDATE t SET v = v + 1` on a unique `v` where jed succeeds (§8;
  the overriding reason is the two-phase / all-or-nothing model, CLAUDE.md §11 step 6).
- When one row violates **several** unique indexes, jed reports the lowest name (byte
  order); PG reports creation order, which jed does not persist (constraints.md §5.4).

## 8. UNIQUE indexes (the enforcement)

A **unique** index (`unique = true` in the catalog — set by `CREATE UNIQUE INDEX` or by a
`UNIQUE` constraint, constraints.md §5) forbids two rows from sharing its **key value
tuple** — a tuple of column values and/or **evaluated expression values** (`CREATE UNIQUE
INDEX ON t (lower(email))` forbids two rows with the same `lower(email)`, the canonical
case-insensitive-unique idiom). Everything else about it — entry keys (§3), maintenance
(§4), planner treatment (§5), persistence (§6) — is exactly a plain index's; the
uniqueness *probe key* is the entry-key prefix built the same way, so an expression key's
value enters the probe exactly like a column value. Enforcement:

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
  of). A genuine conflict with an untouched row traps 23505 as usual. A primary-key
  assignment may also change the rewritten row's storage-key suffix; uniqueness remains
  a check over the value prefix, while §4 maintenance replaces the old suffixed entry
  with the new one.
- **DELETE** cannot violate uniqueness; no probe.
- **`CREATE UNIQUE INDEX` on a non-empty table** verifies the §1 build's computed
  entries pairwise (the same exempt-NULL prefix rule) before the index is registered; a
  duplicate traps `23505` and creates nothing.

**Cost.** Uniqueness probes are **unmetered** validation work, like the primary-key
duplicate check and the index maintenance itself ([cost.md §3](cost.md) "What is NOT
metered") — an INSERT into a uniquely-indexed table accrues the same cost as into a
plainly-indexed one. The `CREATE UNIQUE INDEX` build charges exactly the plain build's
scan (§5); its verification adds nothing.

## 9. Partial indexes (`CREATE INDEX … WHERE predicate`)

A **partial index** indexes only the rows for which its `WHERE predicate` is **TRUE**
(the ordinary 3VL WHERE rule — a row whose predicate is FALSE *or* NULL is left out).
`CREATE INDEX pt_amt_active ON pt (amt) WHERE status = 'active'` builds a B-tree over
`amt` holding an entry only for the `status = 'active'` rows — the canonical shape (a
narrow index over a hot subset). `CREATE INDEX … WHERE` is B-tree only (a `WHERE` on a GIN /
GiST index is `0A000`); the one partial GiST index is the backing index of a partial exclusion
constraint, `EXCLUDE … WHERE (predicate)` ([gist.md §7.1](gist.md)), whose predicate obeys every
rule below and which is constraint-only (never a read access path).

**The predicate.** A boolean expression over the table's own columns, immutable and
self-contained — the same purity an expression *key* guarantees (§1). At `CREATE INDEX`
the predicate is validated in this order (PG-agreeing, oracle-probed):

1. a **subquery** → **`0A000`** (`cannot use subquery in index predicate`) — the resolver
   admits an uncorrelated subquery, so this is a pre-resolution structural reject;
2. a **bind parameter** `$N` → **`42P02`** (`there is no parameter $N`) — likewise pre-resolution;
3. resolve against the table's columns (an unknown column → **`42703`**), requiring the result
   be **boolean** — a non-boolean predicate is **`42804`** (`argument of WHERE must be type
   boolean, not type <t>`); an **aggregate** in the predicate is **`42803`**, a **window
   function** **`42P20`** (both from the ordinary `Forbidden`-context resolver, as for a WHERE);
4. a **non-immutable** call (the entropy/clock/sequence seam — `now`/`clock_timestamp`/
   `current_date`/`uuidv4`/`uuidv7`/`nextval`/…), a resolved **STABLE node** (the runtime
   `text → date` cast or a clock-relative date literal,
   §1), or a session-dependent operation → **`42P17`** (`functions in index
   predicate must be marked IMMUTABLE`). Admission composes the dependencies of every
   resolved child, as specified in [index-dependencies.md](index-dependencies.md).
   `timestamptz` columns and results are allowed: `deleted_at IS NULL`, instant comparisons,
   and explicit-zone operations are safe. Named-zone operations persist their data dependencies.

The predicate is stored as its **canonical text** (the *Check-expression text* form, exactly as
a `CHECK` / expression key — `format_version` 27, §6) and re-parsed + re-resolved against the
loaded table's columns per statement (never persisted-resolved — a deterministic function of the
column types, which are themselves cross-core-identical). The auto-name (§2) is **unaffected** by
the `WHERE` clause: `CREATE INDEX ON pt (amt) WHERE …` derives `pt_amt_idx` exactly as a
non-partial index would (PG-matched).

**Maintenance (§4) is uniform.** A partial index is still "a row maps to a *set* of entries" —
the set is **empty** when the predicate is not TRUE. So INSERT inserts an entry only for a
qualifying row; DELETE removes one only for a formerly-qualifying row; UPDATE's old-set/new-set
diff handles every case for free (row enters the index, leaves it, moves within it, or is
untouched — no special code). The predicate evaluation is **unmetered** maintenance work (like an
expression key's — §4), so a partial-indexed write accrues the same cost as a plain-indexed one.
The **build** scans the table once and indexes only qualifying rows; a `UNIQUE` partial build
verifies uniqueness only among them (a duplicate among the qualifying rows traps `23505` and
creates nothing).

**Uniqueness (§8) is restricted to qualifying rows.** A `UNIQUE` partial index forbids two
*qualifying* rows from sharing a fully-non-NULL key tuple; a row whose predicate is not TRUE is
**exempt** (its uniqueness probe prefix is treated exactly like a NULL-bearing prefix — never
conflicts). So `CREATE UNIQUE INDEX ON pt (amt) WHERE status = 'active'` allows an `inactive`
row to duplicate an `active` row's `amt`, but forbids two `active` rows from sharing it.

**Planner (§5) — sound-if-conservative implication.** The partial index holds *only* the
qualifying rows, so a scan of it plus the residual WHERE returns the query's rows **iff every row
the query wants is a qualifying row** — i.e. the query's WHERE implies the index predicate. jed
decides that with the prover of §9.1. The full WHERE — including any conjunct that implies the
predicate — stays the residual filter (harmless: it is TRUE for every indexed row). A partial
B-tree index then serves the ordinary access-predicate bound (§5.1, SELECT and UPDATE/DELETE), the
**full partial-index scan** (§9.2), and the remaining ordered-index paths (§9.3). Its scan rows
are estimated with the predicate's selectivity ([estimator.md §7](estimator.md)).

### 9.1 The implication prover

`implies(W, P)` decides whether a WHERE implies a partial index's resolved predicate `P`. `W` is the
list of the filter's top-level `AND` conjuncts (the conjuncts of every filter the caller supplies,
in order); `P`'s columns are table-local and match a filter column shifted by the relation's
offset. Three procedures, each consuming **one step** of a budget of **10,000** per call of the
prover (a procedure entered with the budget exhausted answers `false`). Every procedure
short-circuits left to right in the order written:

1. **`conj(W, P)`** — if `P` is `A AND B`: `conj(W, A) && conj(W, B)`. Otherwise `true` if some
   `w ∈ W` (in order) has `clause(w, P)`; else, if `P` is `A OR B`: `conj(W, A) || conj(W, B)`;
   else `false`.
2. **`clause(w, p)`** — `true` if `w` is structurally equal to `p` (the §5.1 expression match;
   comparisons agree on operator and, for an ordering operator, collation). Else: if `p` is
   `A AND B`, `clause(w, A) && clause(w, B)`; if `w` is `A OR B`, `clause(A, p) && clause(B, p)`;
   if `w` is `A AND B` (nested under an OR), `clause(A, p) || clause(B, p)`; if `w` is an
   **`x = ANY(array)` premise** — a non-row-wise, non-`ALL` `=` quantified comparison whose array is
   a constant array or a one-dimensional `ARRAY[…]`, at most **100** elements, every element NULL or
   a prover constant — then every non-NULL element `e` (in order) must satisfy `atom(x = e, p)`, and
   at least one element must be non-NULL (a NULL element never makes `ANY` TRUE); the premise's
   ordering-collation flag is the quantifier's. Else, if `p` is `A OR B` and
   `clause(w, A) || clause(w, B)`, `true`. Else, if `w` is a **comparison atom**, `atom(w, p)`;
   else `false`.
3. **`atom(x op₁ c₁, p)`** — if `p` is `A AND B`: `atom(…, A) && atom(…, B)`; if `A OR B`:
   `atom(…, A) || atom(…, B)`. If `p` is `y IS NOT NULL` with `y` structurally equal to `x`: `true`
   (a TRUE comparison with a constant has a non-NULL operand). If `p` is a comparison atom
   `y op₂ c₂` with `y` structurally equal to `x` and `c₁`, `c₂` of the same constant family: the
   implication table below on the order of `c₁` vs `c₂`, where an ordering operator (`< <= > >=`)
   on either side requires both comparisons to carry no collation (`C` text or a non-text family).
   Otherwise `false`.

A **comparison atom** is a comparison (`= <> < <= > >=`) with a **prover constant** on one side
(normalized to the right, the operator flipped when the constant is on the left; the right side is
tried first). The prover constant families, each ordered within itself only: integer literals (by
value), `text` (byte order — `C`), `decimal` (by value), `date`, `timestamp`, `timestamptz`,
`boolean` (`false < true`), `uuid` (byte order). A NULL literal, a parameter `$N`, a float, an
interval, a cast, or any other expression is not a prover constant. `IN (…)` is its desugared OR of
equalities, and `BETWEEN` its AND of two comparisons.

The comparison implication table (PostgreSQL's btree `BT_implic_table`): `x op₁ c₁` implies
`x op₂ c₂` when

| `op₁` | `op₂` and the condition on `c₁` vs `c₂` |
|---|---|
| `=` | `c₁ op₂ c₂` holds |
| `<` | `op₂ ∈ {<, <=, <>}` and `c₁ <= c₂` |
| `<=` | `op₂ ∈ {<, <>}` and `c₁ < c₂`; or `op₂` is `<=` and `c₁ <= c₂` |
| `>` | `op₂ ∈ {>, >=, <>}` and `c₁ >= c₂` |
| `>=` | `op₂ ∈ {>, <>}` and `c₁ > c₂`; or `op₂` is `>=` and `c₁ >= c₂` |
| `<>` | `op₂` is `<>` and `c₁ = c₂` |

So `amt > 50` implies `amt > 0`, `amt = 5` implies `amt <> 3`, `status IN ('a','b')` implies
`status IN ('a','b','c')`, `status > 'b'` implies `status IS NOT NULL`, and `amt = ANY(ARRAY[4,
33])` implies `amt > 0`; `amt >= 0` does not imply `amt > 0`. Every rule is sound for any row,
including one whose columns are all NULL (a NULL-extended outer-join row): a TRUE premise never
holds of a NULL operand.

**NULL-rejecting predicates.** A predicate is *NULL-rejecting* when it cannot be TRUE for a row whose
columns are all NULL. The syntactic test: a comparison with a bare column on either side; a `LIKE`
or regex match, or a non-`ALL` quantified comparison, with a bare column operand; `col IS NOT
NULL`; a bare column or its `NOT`; an `AND` with either side NULL-rejecting; an `OR` with both sides
NULL-rejecting. Anything else is not. Used by §9.3.

### 9.2 The full partial-index scan

When a **single-relation** SELECT, UPDATE, or DELETE's WHERE implies a partial B-tree index's
predicate but supplies **no** equality/range access predicate on its keys, the whole index is still a
complete source of every row the statement can return. The planner then inventories one more
candidate for that index: a **full partial-index scan** — the §5.1 bound with an empty equality
prefix and no range (the range `[ε, ∞)` over the index tree), recovering each entry's storage key by
the slot rule and point-looking-up its row. It is a B-tree candidate under the index's name, so it
takes part in cost selection like any bound ([estimator.md §9](estimator.md)), composes with an
exact same-index `ORDER BY` (the sort is elided) and with LIMIT streaming exactly as a bound over
that index does, and appears in EXPLAIN as `Index scan: using <index>`. It is offered only when every
key element is a scalar with a slot rule (§5.1) and no key column is a version-skewed collated
column ([collation.md §12](collation.md)), and never in a multi-relation scope: there a relation may
be NULL-extended, and an implied predicate such as `deleted_at IS NULL` is TRUE for a NULL-extended
row the index does not hold. The structural selector used by fixed-policy (multi-relation)
boundaries therefore never sees it. The canonical use is a soft-delete index over the few live
rows: `CREATE INDEX t_live ON t (k) WHERE deleted_at IS NULL` serves `WHERE deleted_at IS NULL` and
`WHERE deleted_at IS NULL ORDER BY k LIMIT n`.

### 9.3 The other ordered-index paths

- **OR / IN interval sets** ([cost.md §3](cost.md)) use a partial index when the WHERE implies its
  predicate; the set's disjunction is a WHERE conjunct of strict comparisons on the relation's own
  key, so it rejects a NULL-extended row.
- The **exact-order index walk** of `ORDER BY … LIMIT` ([planner.md §5](planner.md), single
  relations only) walks a partial index when the WHERE implies its predicate.
- **Index-nested-loop inner.** A partial index serves as the inner of an index-nested-loop join
  when the join's own `ON` implies its predicate — a row the index lacks fails the `ON` and never
  joins, outer join or not — or when the `ON` together with the WHERE implies a **NULL-rejecting**
  predicate. A WHERE conjunct holds only of the final rows, and an outer join may NULL-extend this
  relation in a row that the index's missing rows produced; a NULL-rejecting predicate makes every
  such row fail the implied WHERE, so the result is unchanged. (`a LEFT JOIN b ON b.k = a.x WHERE
  b.tag IS NULL` must not probe an index `WHERE tag IS NULL`: an `a` row whose only matches are
  tagged would become a NULL-extended row that passes the WHERE.)
- The **referencing-side lookup** of a foreign-key action ([constraints.md §6](constraints.md))
  still uses only non-partial indexes (it has no query predicate to imply the index predicate).

**Divergences from PostgreSQL** (§7): the prover is a bounded subset of PostgreSQL's
(`predtest.c`): no refutation, no cross-family constants, no `NOT` reasoning, and a parameter
`$N` is never a known constant (PostgreSQL's custom plans see the value), so a jed miss is a
correct scan without the index. A partial unique index arbitrates `ON CONFLICT` only through an
implying `WHERE index_predicate` ([upsert.md §2](upsert.md)), as in PostgreSQL.

**Introspection.** `jed_indexes` gains a `predicate text` column carrying the canonical predicate
text (NULL for a non-partial index) — the analog of PostgreSQL's `pg_index.indpred`
([introspection.md §5.1](introspection.md)).

## 10. `REINDEX` — rebuild in place

```sql
REINDEX [ ( option [, ...] ) ] { INDEX | TABLE } [ CONCURRENTLY ] [db.]name
REINDEX [ ( option [, ...] ) ] { DATABASE | SYSTEM } [ CONCURRENTLY ] [ name ]

option ::= VERBOSE [ boolean ] | CONCURRENTLY [ boolean ] | TABLESPACE name
boolean ::= TRUE | FALSE | ON | OFF | 1 | 0          -- bare or quoted; omitted means TRUE
```

PostgreSQL's `REINDEX` (oracle-probed), for the forms that have a meaning in jed. Its job in jed is
mostly not corruption repair: it is the **one-statement rebuild of an index whose persisted
dependencies no longer match the environment** — a host function whose component or version changed
([extensibility.md](extensibility.md) §8.1/§11), or named-zone data the index pinned
([index-dependencies.md](index-dependencies.md)). Such an index is skipped by reads and makes its
table refuse writes (`XX002`) until it is rebuilt with the current environment.

### 10.1 What a rebuild is

Rebuilding an index is **exactly** dropping it and creating it again with the same definition — the
same name, key elements and their ordering options (§11), uniqueness, method, and predicate — in one all-or-nothing statement, with
three properties a DROP/CREATE pair does not give for free:

- **Position and links are kept.** The index keeps its place in the table's catalog order (ascending
  name, §6, so a re-created index lands there too) and every link that names it: the `UNIQUE`
  constraint it *is* (constraints.md §5), the `EXCLUDE` constraint it backs (gist.md §7), and any
  foreign key whose referenced columns it covers. `DROP INDEX` of an `EXCLUDE`-backing index is
  `2BP01`; `REINDEX` of it is legal.
- **Dependencies are re-admitted against the current environment.** Every key expression and the
  predicate are resolved again and admitted exactly as `CREATE INDEX` admits them (§2), and the
  host-function and timezone dependency lists are **collected afresh** rather than verified: the
  rebuilt index records the current component identity and semantic version of each host function
  and pins the currently loaded zone data. So the codes are `CREATE INDEX`'s: a host function no
  longer registered is `42883`, one no longer `IMMUTABLE` or without a component identity `42P17`, an
  explicitly named zone that is not loaded `22023`. An index whose stored key or predicate text this
  binary cannot parse is **`XX003`** ([unparsable-definitions.md](unparsable-definitions.md) §3.4):
  rebuilding needs the parsed definition, so such an index can still only be dropped.
- **The new tree is built from the current rows** by the `CREATE INDEX` build (§2): a `UNIQUE` index
  re-verifies uniqueness (`23505`, with the duplicate's detail) — a changed host function can map two
  rows that were distinct to one key. A plain-column `EXCLUDE`-backing index is not re-checked: its
  keys are columns compared by built-in operators, which the rebuild cannot change. A version-skewed
  collated table is `XX002`, as for `CREATE INDEX` ([collation.md](collation.md) §12): the collation
  migration stays the host operation `upgrade_collations`, never SQL.

Every target index is re-admitted and built **before any is installed**, so a failure in any of
them — including the cost ceiling — leaves every index, and its stale pins, as they were. Planner
statistics are unchanged (the rows are unchanged). The catalog generation advances, so cached plans
are re-planned and see the usable index.

### 10.2 Targets

- **`INDEX name`** rebuilds one secondary index. An unqualified name resolves like `DROP INDEX`'s —
  the session's temporary domain, then `main`; a qualifier (`main.`, `temp.`, an attachment) names the
  database, as for a table (attached-databases.md §3). No relation of that name is **`42P01`**
  (`relation "x" does not exist`, as PostgreSQL); a table, view, sequence, or built-in catalog
  relation is **`42809`** (`"x" is not an index`).
- **`TABLE name`** rebuilds every secondary index of the table in catalog order; a table with none is
  a zero-cost success (PostgreSQL's notice has no jed channel). Missing is `42P01`; an index, view, or
  sequence is `42809` (`"x" is not a table or materialized view`); a built-in catalog relation is
  `42809` (`cannot modify system relation`, the CREATE INDEX / ANALYZE precedent).
- **`DATABASE [name]`** rebuilds every index of every table of the database, tables in name byte
  order. Unnamed or `main`, the database is `main` **followed by the session's temporary tables**
  (PostgreSQL's database-wide form covers the session's own temp tables); an attachment's name
  rebuilds that attachment's tables. Any other name, `temp` included, is **`0A000`** *can only reindex
  the currently open database* (PostgreSQL's check of a name other than the current database's).
- **`SYSTEM [name]`** rebuilds system-catalog indexes. jed's catalog relations are computed, with no
  indexes, so it is a zero-cost success after the same name check. `SYSTEM CONCURRENTLY` is `0A000`
  *cannot reindex system catalogs concurrently*, checked first, as PostgreSQL.
- **`SCHEMA name`** is **`0A000`**: jed has no schemas (grammar.md §3); the qualifier names a database,
  which `DATABASE` covers.

The primary key owns no index object ([constraints.md](constraints.md) §5.4; it is the table's own tree), so
`REINDEX INDEX t_pkey` is `42P01` and `REINDEX TABLE` does not rewrite the table. PostgreSQL rebuilds
the primary-key index there; a jed table tree has nothing equivalent to rebuild.

### 10.3 Options and `CONCURRENTLY`

- **`VERBOSE`** is accepted and emits nothing: PostgreSQL's per-index `INFO` lines carry CPU timings,
  and jed has no notice channel.
- **`CONCURRENTLY`**, as the positional keyword or the option, performs the ordinary rebuild. Its
  PostgreSQL purpose — not blocking reads and writes during the build — is already jed's model:
  readers never wait on the single writer (CLAUDE.md §3). In the positional spelling the keyword is
  recognized only before a name for `INDEX`/`TABLE`, so a relation named `concurrently` stays
  nameable without quotes.
- **`TABLESPACE`** is `0A000`: jed has no tablespaces.
- Option names and values follow PostgreSQL: an unknown option is `42601` (*unrecognized REINDEX
  option*), a value that is not a boolean is `42601` (*… requires a Boolean value*), and a repeated
  option is accepted, the last value winning.

### 10.4 Transactions, privileges, cost

- **Transactional.** `REINDEX INDEX` and `REINDEX TABLE` join an open transaction block and roll back
  with it (a rolled-back rebuild restores the stale index and its pins). As in PostgreSQL,
  `CONCURRENTLY`, `DATABASE`, and `SYSTEM` cannot run inside a transaction block: **`25001`** *REINDEX
  CONCURRENTLY | DATABASE | SYSTEM cannot run inside a transaction block* (CONCURRENTLY checked first),
  which aborts the block like any error. Outside a block every form is one atomic implicit
  transaction; PostgreSQL commits the database-wide form per table, a difference visible only in which
  indexes a failure leaves rebuilt — none, in jed.
- **A write.** It is `25006` in a read-only transaction or on a read-only handle, and against a
  read-only attachment. The rebuilt trees replace the old ones; the old pages become orphans that the
  commit's free-list reclamation returns, exactly as for `DROP INDEX` + `CREATE INDEX`. No new page
  type, commit path, or format change.
- **Privileges** ([session.md](session.md) §5.3). Like `ANALYZE`, `REINDEX` is maintenance that reads
  every row: `INDEX` and `TABLE` need **`allow_ddl`** and **`SELECT`** on the table (for `INDEX`, the
  owning table); `DATABASE` and `SYSTEM` need only `allow_ddl` (PostgreSQL's database-owner check). The
  `allow_temp_ddl` split does not apply — it governs creating and removing temporary schema, not
  maintenance — so a temporary target still needs `allow_ddl`.
- **Cost** ([cost.md](cost.md) §3). Each rebuilt index charges **exactly its `CREATE INDEX` build
  scan** (page_read per table-tree node + storage_row_read per row, over the columns its keys and
  predicate read, plus their `value_decompress` slabs and any host-function calls), summed over the
  targets in order and guarded per row against the ceiling. Nothing else is charged; a target with no
  index costs zero.

### 10.5 Divergences from PostgreSQL

The primary key is not an index object (`REINDEX INDEX t_pkey` is `42P01`); `SCHEMA` and
`TABLESPACE` are `0A000`; `VERBOSE` prints nothing; a database-wide rebuild is atomic rather than
committed per table; `DATABASE <attachment>` is a jed extension (PostgreSQL has no attached
databases); a built-in catalog relation is `42809` where PostgreSQL rebuilds a system table's
indexes. None is PG-comparable through the oracle (which probes inside a transaction block), so
`suites/ddl/reindex_jed.test` pins them cross-core, beside the oracle-clean `suites/ddl/reindex.test`.


## 11. Ordered keys (`ASC` / `DESC`, `NULLS FIRST` / `NULLS LAST`)

```sql
CREATE INDEX t_ab ON t (a DESC, b NULLS FIRST, (c + 1) DESC NULLS LAST)
```

Each key element may carry PostgreSQL's ordering options ([grammar.md §30](grammar.md)). Semantics
are PostgreSQL's (oracle-probed):

- **Defaults.** The direction defaults to `ASC`; the NULL placement defaults **by direction** —
  `ASC` ⇒ `NULLS LAST`, `DESC` ⇒ `NULLS FIRST` (NULL is the largest value, [encoding.md §4](encoding.md)).
  `ASC NULLS LAST` is the plain index of §1–§9, byte-identical to an index without options.
- **B-tree only.** A GIN or GiST index rejects any *written* option — an explicit `ASC` included —
  with `0A000` (`access method "gin" does not support ASC/DESC options`; then, for a NULL placement,
  `… does not support NULLS FIRST/LAST options`), checked per element right after the element's own
  column checks (so a missing column is still `42703`). An `EXCLUDE` element takes none (its
  grammar has no option).
- **Constraints take none.** `UNIQUE (a DESC)` and `PRIMARY KEY (a DESC)` are syntax errors (`42601`),
  as in PostgreSQL: only `CREATE UNIQUE INDEX` takes an ordering. (PostgreSQL's `ADD CONSTRAINT …
  USING INDEX`, which refuses an index without default ordering, has no jed analogue.)
- **The auto-name ignores the order** (`CREATE INDEX ON t (b DESC, a)` is `t_b_a_idx`).

**Entries.** Element `i`'s slot is the [encoding.md §2.16](encoding.md) **ordered slot** under its
order: the presence tag chosen by the NULL placement, the type's order-preserving body bitwise
inverted for `DESC`. Every keyable type qualifies — the inversion reverses the order of the
prefix-free bodies, variable-width and container bodies included — so an index iterates in its
declared order. The order changes only the entry bytes, never *which* rows an index holds or
constrains: maintenance (§4), uniqueness (§8, NULLS DISTINCT, the probe prefix built from the same
ordered slots), partial predicates (§9), the `ON CONFLICT` arbiter (inference ignores the order, as
in PostgreSQL — [upsert.md](upsert.md)), a foreign key referencing a unique index's columns, and a
child index serving the parent-side reverse match ([constraints.md §6.5](constraints.md)) all encode
their probes through the index's ordered slots.

**Planner.** Every §5 path treats an ordered index as an ASC one, through its slots:

- the **access predicate** (§5.1) encodes equality-prefix slots as ordered present slots; a trailing
  range maps its logical interval into the element's present region `[tag, tag + 1)` — under `DESC`
  the byte endpoints swap (`a > v` becomes `< tag ‖ ~v`) — so the NULL slot stays outside the range
  whether it sorts first or last;
- the unified **key-slot skipper** (§5.1) steps over each skipped element's ordered slot: its NULL tag, or its present tag and the body — under `DESC` the inverted body, whose end the skipper finds by inverting the terminator / length bytes (the no-prefix property holds under inversion, [encoding.md §2.16](encoding.md));
- **key-set probes** (OR / `IN` / `= ANY`, [cost.md §3](cost.md) "Multi-column secondary indexes")
  encode each member's points as ordered slots and emit the probes in physical byte order, so a set
  over an ordered index still streams that index's order;
- **index-nested-loop** seeks, `UPDATE`/`DELETE` targets, and the FK reverse match seek ordered slots;
- an **`ORDER BY` served by an index walk** ([cost.md §3](cost.md) "secondary-index order") requires
  every `ORDER BY` key to match its index element's **direction and effective NULL placement**
  (`ORDER BY a DESC` matches `a DESC` = `DESC NULLS FIRST`, not `a DESC NULLS LAST`). The walk is
  forward, so ties are broken by the ascending storage-key suffix — exactly the eager stable sort.

Estimates and costs are those of the equivalent ASC index (the trees have identical shapes); an
ordered index adds candidates, never a new cost unit.

**Not this slice.** A **backward** walk — serving `ORDER BY a DESC` from an `ASC` index, as
PostgreSQL's *Index Scan Backward* does — stays a follow-on: reversing the walk also reverses the
storage-key tie-break, which then disagrees with jed's PK-ascending tie order unless the index key is
unique and NULL-free (the reverse-PK trap, cost.md §3). Declaring the index in the wanted order is the
supported path.

**Persistence** (`format_version` 46, [format.md](../fileformat/format.md)): `index_flags` bit4
`has_key_options`; when set, one option byte per key element (bit0 `desc`, bit1 `nulls_first`)
follows the index's other optional sections. The writer omits the list when every element is
`ASC NULLS LAST` (so such an index is byte-identical to v45); a reader rejects a reserved option bit,
the bit on a non-B-tree index, or an all-default list (`XX001`).

**Introspection.** `jed_indexes` gains `descending boolean[]` and `nulls_first boolean[]`, parallel
to `columns` — PostgreSQL's `pg_index.indoption` bits ([introspection.md §5.1](introspection.md)).
The CLI dump re-emits the options as `pg_get_indexdef` prints them (`a DESC`, `b NULLS FIRST`,
`c DESC NULLS LAST`).
