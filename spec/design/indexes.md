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
CREATE [UNIQUE] INDEX [name] ON table (key [, key ...]) [WHERE predicate]
DROP INDEX name
```

The optional **`WHERE predicate`** makes the index **partial** (§9): only rows for which the
predicate evaluates to **TRUE** are indexed (and, for a `UNIQUE` partial index, only those rows
are constrained). A partial index is **B-tree only** this slice; a `WHERE` clause on a
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
- **Still deferred** (each a relaxable narrowing, PostgreSQL features): per-key
  `ASC`/`DESC`/`NULLS`, `IF NOT EXISTS`, `CONCURRENTLY`, and an explicit operator class /
  `COLLATE` on a key element. An expression key's text result uses the expression's own
  effective collation (jed's ordinary text-collation resolution), not a per-key `COLLATE`.
  (Partial (`WHERE`) indexes have **landed** for B-tree — §9.)

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
base is the **lowercased** `<table>_<part>_<part>..._idx` — one **name part** per key
element in list order, joined with `_`, where the part is:

- a **column key** → the **column name** (`lower(email), a` → parts `lower`, `a`);
- a **bare-function-call** expression → the **function name** (`lower(email)` → `lower`,
  `abs(a)` → `abs`);
- any **other expression** → the literal `expr` (`(a + b)` → `expr`, `(email || 'x')` →
  `expr`).

If the base is taken (case-insensitive), try `<base>1`, `<base>2`, … and take the smallest
free suffix — so `CREATE INDEX ON t (lower(email))` then `CREATE UNIQUE INDEX ON t (lower(email))`
yield `t_lower_idx`, `t_lower_idx1` (probed). An explicit name is stored **as written**
(original case round-trips; comparisons are case-insensitive) — same rule as table and
CHECK-constraint names.

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
entry_key = nullable-slot(key_1) ‖ nullable-slot(key_2) ‖ … ‖ row_storage_key
```

- Each key element's **value** for a row is its indexed column's value (a column key) or
  the **result of evaluating the key expression against the row** (an expression key —
  §4). That value is encoded as the **encoding.md §2.2 nullable slot**: a 1-byte presence
  tag (`0x00` present ‖ the type's order-preserving key bytes, `0x01` NULL) — **always**,
  even for a NOT NULL column or a NOT-NULL-typed expression (one uniform rule; nullability
  never changes the byte layout). This is the first place §2.2 is exercised in stored
  bytes: NULL sorts **after** every present value (ascending), the PostgreSQL model. An
  expression key encodes under the **expression's resolved result type and collation**
  (e.g. `lower(email)` → the `text` order-preserving key under the expression's effective
  collation).
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
  not touch this index's columns), leave the tree node untouched. The skip is part of
  the contract, not an optimization detail: it keeps the copy-on-write dirty set — and
  therefore the incremental commit's written pages — byte-identical across cores
  (CLAUDE.md §8). The row's storage key cannot change (the PK-assignment narrowing,
  CLAUDE.md §11 step 6), so the suffix is stable.
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

A **plain** column index reads only fixed-width, never-spilling indexable columns, so its
maintenance cannot fault. An **expression** index may reference a variable-width column
(`lower(bigtext)`) whose value spilled to an overflow chain; evaluating the expression
faults it in on demand through the ordinary evaluator backstop (the lazy-record
`Unfetched` resolution), so maintenance transparently materializes what a key expression
reads.

## 5. The planner: index-bounded scans (SELECT)

(This is one arm of the planner's access-path precedence — the pass structure and full
rule inventory are in [planner.md](planner.md) §4/§5.)

The existing per-relation pushdown seam ([cost.md §3](cost.md) "bounded scan") gains a
second bound kind. For each **base relation of a SELECT scan** (single-table, a JOIN
base table, or a correlated subquery's inner table), the plan picks, in order:

1. The **PK tuple bound**, if the WHERE AND-chain supplies a maximal leading equality prefix and
   optional next-member range (the PK is the row's own key; it needs no second tree and is strictly
   cheaper).
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

An `UPDATE` / `DELETE` target scan uses the same eligible access predicate as SELECT. It gathers
every admitted `(storage key, old row)` from the pre-mutation index state, rechecks the complete
WHERE, finishes validation for the whole batch, and only then changes table/index storage. Thus an
indexed-column update, a PK-rekeying update, and a partial/expression-index bound cannot perturb the
candidate walk in progress. The **LIMIT streaming short-circuit does not combine** with an index
bound (an index-bounded scan with LIMIT takes the eager path — its cost reads the full admitted set).

An index is **eligible for the bound only when every key element from the range element
onward** — i.e. all elements **after the equality prefix** — has a **fixed-width scalar**
type (a column key's column type, or an expression key's *result* type). The suffix-skip
above recovers the row's storage key by advancing over each such component by its type's
*fixed* width, which a **non-scalar** (range/array/composite) or a **variable-width scalar**
(`text` / `decimal` / `bytea` / `interval`) does not have. The **equality-prefix elements
may be any width** (including collated `text`, and an expression producing `text` such as
`lower(email)`) — their slots are matched as the known encoded prefix `P`, skipped by
`len(P)`, never by width; so an index whose leading elements are variable-width is usable
**when the WHERE pins them by equality** (`lower(email) = 'x' AND b > 3` over
`(lower(email), b i32)` seeks; a bare `b > 3` there does not, because the leading slot is
then unknown and variable-width). An index whose range element or a trailing unbounded
element is variable-width is **not used for the bound**: the query takes the full scan +
residual filter (rows identical, only the cost differs). Lifting the fixed-width tail
requirement is a follow-on: skip a variable-width component by its self-delimiting length,
not a fixed width.

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
- The table entry gains its **index list**: per index, the name (original case) + a
  **key-element list** (key order, duplicates allowed) + a **flags byte** (`bit0 unique` —
  added in v6; **`bit1 has_predicate`** — added in v27, §9; the remaining bits are reserved,
  written 0 and read-validated) + the index tree's **root page**, then — **only when `bit1`
  is set** (`format_version` 27) — a `u16 pred_len` + `pred_len` UTF-8 bytes of the partial
  index's **predicate canonical text** (the *Check-expression text* form). On load a partial
  predicate re-parses (`XX001` on failure, like a stored CHECK) and re-resolves against the
  loaded table's columns per statement (never persisted-resolved — a deterministic function of
  the column types). A non-partial index writes no `bit1` and no predicate bytes and is
  byte-identical to v26. Indexes are stored and held in **ascending lowercased-name order**
  (the catalog's deterministic order, like checks; also the §5 tie-break order and the §8
  violation-report order).
- **Each key element** is a `u16`: a **column ordinal** (`< col_count`) for a column key,
  or the sentinel **`0xFFFF`** — which cannot be a valid ordinal (`col_count ≤ 65535` ⇒
  max ordinal `65534`) — for an **expression key** (**new in `format_version` 26**),
  followed by a `u16 expr_len` and `expr_len` UTF-8 bytes of the expression's **canonical
  text** (the *Check-expression text* form format.md defines, exactly as a `CHECK` / column
  `DEFAULT` stores). On load an expression element re-parses that text with the ordinary
  expression parser (`XX001` if it fails, like a stored CHECK); its result type and
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
- **Partial-index implication is syntactic** (§9): jed uses a partial index only when the
  WHERE AND-chain **contains a conjunct structurally equal to the index predicate**, where
  PG's planner also matches a query predicate that *implies* the index predicate
  (`amt > 50 ⟹ amt > 0`). A jed miss is a correct full scan. A partial-index predicate
  that references a `timestamptz` column/value is conservatively `42P17` (the expression-key
  hazard); and a partial index is used only via the access-predicate bound (no full
  partial-index scan, and no partial OR/IN / ORDER-BY-skip / INL path this slice).
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

## 9. Partial indexes (`CREATE INDEX … WHERE predicate`)

A **partial index** indexes only the rows for which its `WHERE predicate` is **TRUE**
(the ordinary 3VL WHERE rule — a row whose predicate is FALSE *or* NULL is left out).
`CREATE INDEX pt_amt_active ON pt (amt) WHERE status = 'active'` builds a B-tree over
`amt` holding an entry only for the `status = 'active'` rows — the canonical shape (a
narrow index over a hot subset). B-tree only this slice (a `WHERE` on a GIN / GiST index
is `0A000`); the `EXCLUDE … WHERE (predicate)` partial form is a separate GiST follow-on
([gist.md §7](gist.md)).

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
   §1), **or** a **`timestamptz`-dependent** subexpression (one that
   references a `timestamptz` column or produces a `timestamptz` value — the same conservative
   session-timezone hazard an expression key carries, §1) → **`42P17`** (`functions in index
   predicate must be marked IMMUTABLE`).

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
qualifying rows, so a bounded scan of it plus the residual WHERE returns the query's rows **iff
every row the query wants is a qualifying row** — i.e. the query's WHERE implies the index
predicate. jed uses a **syntactic** test (PG's, not a semantic prover): a partial B-tree index is
eligible for an access-predicate bound (§5.1) **only when the WHERE AND-chain contains a conjunct
structurally equal to the index's predicate** (the §5.1 `rexpr_eq_shifted` structural match). So
`SELECT … WHERE status = 'active' AND amt = 100` seeks `pt_amt_active` (the `amt = 100`
access predicate, gated by the present `status = 'active'` conjunct); `SELECT … WHERE amt = 100`
(no predicate conjunct) takes the full scan. The full WHERE — including the predicate conjunct —
stays the residual filter (harmless: it is TRUE for every indexed row). Partial indexes are used
**only** through the ordinary access-predicate bound, including UPDATE/DELETE when the mutation
WHERE contains the predicate conjunct. The OR/IN merged-point-lookup, ORDER-BY-skip-sort walk, and
index-nested-loop paths keep non-partial indexes only this slice (each a documented follow-on).

**Divergences from PostgreSQL** (§7): (a) the implication test is **syntactic** — jed uses a
partial index only when the WHERE literally contains the predicate conjunct, where PG's prover
also matches an implying predicate (`amt > 50` query ⟹ `amt > 0` index); a jed miss is a correct
full scan. (b) A predicate that references a `timestamptz` column or value is conservatively
**`42P17`** (the expression-key hazard, extended to predicates) — so `WHERE deleted_at IS NULL`
over a `timestamptz deleted_at` is rejected this slice (relaxable; a `timestamp`/`boolean`
soft-delete marker works). (c) There is no full partial-index scan without a leading equality/range
access predicate (a follow-on). Each is a relaxable narrowing, recorded here.

**Introspection.** `jed_indexes` gains a `predicate text` column carrying the canonical predicate
text (NULL for a non-partial index) — the analog of PostgreSQL's `pg_index.indpred`
([introspection.md §5.1](introspection.md)).
