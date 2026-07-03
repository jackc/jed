# GiST access method + `EXCLUDE` constraints — design

> A **third index *kind*** beside the ordered B-tree ([indexes.md](indexes.md)) and the GIN
> inverted index ([gin.md](gin.md)): a **G**eneralized **S**earch **T**ree — a height-balanced
> tree of **bounding predicates** (an interior entry's key *covers* every key beneath it), the
> access method PostgreSQL uses for overlap/containment queries and for every non-equality
> **exclusion constraint**. This doc is the contract all three cores implement in lockstep
> (CLAUDE.md §2). Its payoff is `EXCLUDE` constraints (§7); its first opclass is `range_ops`
> (§5, the [ranges.md §10](ranges.md) deferred follow-on, finally built). The slice ladder
> (GX0–GX3) is §12; the live backlog is [TODO.md](../../TODO.md) Phase 4.
>
> **GX0 (this slice) is spec-only.** It authors this doc, fixes the **determinism decision**
> (§3 — the load-bearing call, a §8 pre-coding hotspot), defines the **opclass seam** (§2),
> registers **`23P01 exclusion_violation`** ([../errors/registry.toml](../errors/registry.toml)),
> and reserves the `index_kind = 2` discriminator + the `format_version` bump that GX1 spends
> (§8). No executor code, no corpus entries, no format bump land here.

## 1. Surface (what the later slices expose)

```sql
-- GX1/GX2: the index as an acceleration structure
CREATE INDEX [name] ON table USING gist (col)

-- GX3: the exclusion constraint, backed by a GiST index
CREATE TABLE t (
  ...,
  EXCLUDE USING gist (col WITH operator [, col2 WITH operator2 ...])
)
```

- **A GiST index is a new index *kind*.** Everything an ordinary index has — the relation
  namespace, auto-naming (`<table>_<col>_idx` + collision suffixes), `DROP INDEX`, `DROP TABLE`
  cascade, the catalog slot, the page-backed copy-on-write B-tree of nodes, free-list
  reclamation — it has unchanged ([indexes.md §2/§6](indexes.md)); only **how an interior key
  summarizes its subtree** and **how a query descends** differ. `USING gist` selects this kind;
  the default (no `USING`, or `USING btree`) stays the ordered B-tree, `USING gin` the inverted
  index.
- **`UNIQUE` is meaningless for GiST and rejected.** A bounding tree has no notion of a unique
  key; `CREATE UNIQUE INDEX … USING gist` is `0A000` (the GIN-`UNIQUE` precedent, [gin.md §1](gin.md)).
  Uniqueness *over* a GiST index is expressed as an `EXCLUDE (col WITH =)` constraint, not a flag.
- **An exclusion constraint *is* its backing GiST index** (the UNIQUE-is-its-index model,
  [constraints.md §5](constraints.md), generalized from a single implicit `=` to an explicit
  list of `(column, operator)` pairs). §7.
- **Indexes are maintained on every write, like any index** ([indexes.md §6](indexes.md)); a GiST
  index changes *which rows are scanned / probed*, never *which rows a query returns* — a SELECT's
  result multiset is identical with or without it (§9).

## 2. The operator class — the type-generic seam

A GiST index is parameterized by an **operator class** (opclass): a small set of pure,
deterministic, hand-written-per-core functions that are the *only* type-specific part. The tree
machinery (node split/merge, the page-backed B-tree of nodes, maintenance, descent, cost) is
**shared** and opclass-agnostic — it manipulates an **abstract bounding key** (an opaque byte
string the opclass produces and compares), exactly as the GIN core manipulates abstract terms
([gin.md §2](gin.md)). The seam, the GiST analogue of GIN's three-function decomposition:

```
opclass(T):
  union(keys: [BoundingKey])         -> BoundingKey   # the minimal key covering all inputs
  consistent(node: BoundingKey,                       # could any entry under `node`
             query: Value, strategy)  -> bool          #   satisfy `query` under `strategy`? (descend?)
  penalty(node: BoundingKey,                          # cost of enlarging `node` to cover `entry`
          entry: BoundingKey)         -> Penalty        #   (drives choose-subtree on insert)
  picksplit(entries: [BoundingKey])  -> (left, right)  # partition an overflowed node into two
  same(a: BoundingKey, b: BoundingKey)-> bool          # equal bounding keys (maintenance)
  leaf_key(value: T)                 -> BoundingKey    # the bounding key of a single stored value
```

This is the battle-tested PostgreSQL GiST decomposition (`union`/`consistent`/`penalty`/
`picksplit`/`same`, plus a compress/decompress jed folds into `leaf_key` + the value codec). Two
constraints make it a *seam* and not just an interface:

1. **Typed over an abstract bounding key, never hard-wired to a range.** GX0 fixes the signatures
   so a future opclass slots in with **no change to the tree core** — the seam already anticipates
   `multirange_ops`, an `hstore`/dictionary opclass, a `pg_trgm`-style trigram `text` opclass, and
   an `intarray`-style array signature opclass ([TODO.md](../../TODO.md) GiST follow-on, §11).
2. **Every opclass function is pure + deterministic** (no wall-clock, no allocation- or
   iteration-order dependence, no binary float — §3). This is what the determinism decision rests on.

The opclasses this feature ships: **`range_ops`** (§5, GX1) and a **scalar `=`** family (§6, GX2).

## 3. The determinism decision (the load-bearing call)

This is the §8 divergence hotspot to settle *before* any code. **PostgreSQL's GiST is not
cross-core reproducible as written.** Its tree shape depends on insertion order *and* on heuristic
`penalty`/`picksplit` choices (Guttman quadratic/linear split, subtree-selection tie-breaks) whose
outcome is an implementation accident — two PostgreSQL backends building the "same" index can
produce different trees. jed cannot adopt that: there is no reference implementation (§2), so the
on-disk bytes are a cross-core contract (§8) and must be **byte-identical** across Rust, Go, and
TS.

**Decision: jed's GiST is an _operation-deterministic_ R-tree-style structure.**

- **`union`, `consistent`, `penalty`, `picksplit`, `same`, and node entry ordering are pure
  deterministic functions** of their inputs (§2). Concretely:
  - **`penalty`** (choose-subtree): descend into the child whose union, **merged** with the new
    entry (`range_merge` = `range_union(strict = false)`, the convex hull, [ranges.md](ranges.md)),
    has the lexicographically-smallest **value-codec bytes** (§4.1); ties broken by **lower child
    slot index** — a *total* order, never a coin-flip. (A child that already covers the entry merges
    to itself, so its bytes are unchanged — naturally the least "enlargement.")
  - **`picksplit`**: when a node exceeds the fan-out (§4.1), **sort its entries by `range_total_cmp`**
    (the canonical range total order, [ranges.md §6](ranges.md) — a pure deterministic function) and
    split at the **median** (first ⌈n/2⌉ ‖ the rest); recompute each half's `union`. A deterministic
    median split — *not* PG's quadratic heuristic. Fan-out balance is a cost concern, never a
    correctness one (§9).
  - **node entry order** on disk is `range_total_cmp` order (ties by storage-key for leaves / child
    page for interiors), so a node's bytes are a pure function of its entry *set*.
- **Cross-core byte-identity then holds by construction.** Every core replays the **identical
  mutation sequence** (the corpus / a program issues the same statements in the same order), and
  each tree operation is a pure function of `(current tree, the entry)`, so all three cores walk
  through the identical sequence of trees and serialize identical bytes — the same argument the
  ordered B-tree and GIN already make ([gin.md §8](gin.md)). The from-scratch build
  (`CREATE INDEX` on a populated table) inserts rows in **storage-key order**, fixing the build
  shape (GIN's sort-before-insert precedent).

**What jed deliberately does *not* guarantee: insertion-order independence (content-determinism).**
The ordered B-tree and GIN achieve the *stronger* property that the tree is a pure function of the
key *set* regardless of insert order — because their entries are sorted 1-D keys. A GiST entry is a
bounding box with **no total order that captures spatial proximity**, so an R-tree is inherently
order-sensitive; jed does not fight this. It **pins the order** (build = storage-key order;
incremental = the real mutation order) rather than chasing a canonical rebuild. The cost, stated
honestly: a golden fixture is specific to its construction sequence, and inserting the same rows in
a different order yields a *different* (still valid, still byte-identical-cross-core, still
correct) tree. This is acceptable and documented; the determinism that the agent loop and §8
depend on — a fixed `(statement sequence) → bytes` function — is fully preserved.

**GX1 in fact clears the stronger bar.** GX1 rebuilds the persisted tree at commit from the index
store's leaf *set* in **canonical** order (`range_total_cmp`, ties by storage key — §4.1), not in
mutation order, so its tree is a pure function of the row *set* — **content-deterministic**, like the
ordered B-tree and GIN (insertion order cannot change the bytes). Operation-determinism above is the
design *floor* (what a future incremental-COW GiST mutating the tree in place would still satisfy);
GX1 exceeds it. Either way the bytes are cross-core identical.

**This is a structural divergence from PostgreSQL, not a behavioral one.** *Which rows match* a
query, and *which writes a constraint rejects*, are identical to PG (behavior tracks PG, §1). Only
the tree *shape*, the plan/cost, and the on-disk bytes are jed's own — ledgered in §10, **not** a
[determinism.md](determinism.md) exception (the tree is fully deterministic given its inputs; no
entropy/float/clock relaxation is involved).

## 4. Tree structure & bounding keys

A GiST index is a page-backed copy-on-write tree of nodes, allocated/split/committed/reclaimed
exactly as a table or ordered-index tree ([indexes.md §6](indexes.md), [storage.md §4](storage.md))
— only the *entry* contents differ:

- a **leaf entry** = `leaf_key(value) ‖ storage-key` (the indexed row's primary/rowid key, so a
  match resolves to a row), empty payload;
- an **interior entry** = `union(child entries) ‖ child_page` (the bounding key covering the
  subtree).

A bound is stored as the range's **decodable value codec** — the same `encode_range_body` /
`read_range_body` a range *column* uses ([format.md](../fileformat/format.md)) — **not** the one-way
order-preserving key encoding ([encoding.md §2.11](encoding.md)): a persisted GiST node must
reconstruct each bound as a `RangeVal` to evaluate `consistent` / `union` on read, which the
order-preserving key encoding (lossy for some elements) cannot guarantee. `range_ops`' leaf bound is
`encode_range_body(elem, row_range)`; an interior bound is `encode_range_body(elem, union)` where the
union is the convex hull `range_merge` covering the subtree. The **canonical order** the §3 split and
node layout sort by is `range_total_cmp` (the range total order, [ranges.md §6](ranges.md)) — a pure
deterministic function — not the raw codec bytes. (The **scalar `=` opclass**, §6, instead stores
`[min, max]` over the **order-preserving key encoding** ([encoding.md §2](encoding.md)) — each
component the value's *key* bytes, length-prefixed — and orders by **raw byte comparison** of those
bytes, which reproduces the value order by construction; the tree core is byte-for-byte identical, only
the bound's encode/decode and the descend predicate differ. The scalar opclass never decodes a bound
back to a value — `=` needs only comparison, so the bound is compared, never reconstructed.)

### 4.1 The on-disk node format (GX1, `format_version` 20)

GiST nodes are page-backed like any tree node ([format.md](../fileformat/format.md) *Page header*):
the standard 16-byte header (per-page CRC included), payload from offset 16, with **two new
`page_type`s** — `5` = GiST leaf, `6` = GiST interior. `item_count` is the node's entry count *N*;
`next_page` is 0. The element (sub)type needed to decode a bound comes from the indexed range
column's catalog type.

- **Leaf node (`page_type 5`)** — *N* entries, each
  `bound_len u16 ‖ bound (bound_len B) ‖ skey_len u16 ‖ skey (skey_len B)`, where `bound =
  encode_range_body(elem, row_range)` and `skey` is the row's storage key (so a match resolves to a
  row). Ordered by `(range_total_cmp(bound), skey)`.
- **Interior node (`page_type 6`)** — *N* entries, each
  `bound_len u16 ‖ bound (bound_len B) ‖ child_page u32`, where `bound = encode_range_body(elem,
  subtree_union)`. Ordered by `(range_total_cmp(bound), child_page)`. Unlike a B-tree interior (N+1
  children separated by N keys), a GiST interior carries **N bounds for N children** — this count
  difference is exactly why GiST needs its own page types, not a reuse of `3`.

**Fan-out & split.** A node holds at most `GIST_FANOUT` entries (a pinned spec constant); inserting
an (N+1)-th triggers a `picksplit` (§3) and propagates upward, growing a new root when the old root
splits. Every GX1 element bound is fixed-width or small, so a node always fits its page and the byte
budget never binds before the fan-out; a bound that would exceed the page payload is `XX001`
(unreachable at GX1's element set).

**Serialization order (page allocation).** The tree serializes in a canonical **post-order** walk —
each node's children in entry order before the node itself, the root last — so page numbers are a
deterministic function of the tree, identical cross-core.

**GX1 simplifications (documented narrowings, none foreclosed — §11).** (a) The tree is **eagerly
loaded** on open, not demand-paged (the pager seam stays open, [pager.md](pager.md); fine for the
RAM-sized target, CLAUDE.md §9). (b) A commit **rewrites the whole GiST tree** (fresh pages for all
nodes, the old pages reclaimed by the free-list) rather than writing only dirty nodes — the pre-P6.1
whole-image flavor, scoped to GiST. The **in-memory** companion of (b): the planner descends a
**resident** R-tree, and that tree is **rebuilt canonically and whole** (`build_from_leaf_keys` over
the leaf store) after each mutating statement that touched the index — so a read always descends a
fresh, content-deterministic tree (the `gist_descent` cost is then a pure function of the row set,
sound for the untrusted SELECT-only surface, which never triggers a rebuild — CLAUDE.md §13). Both
rewrites are O(rows); incremental GiST COW (and demand-paging) are follow-ons (§11).

## 5. `range_ops` — the first opclass (GX1)

`CREATE INDEX … USING gist (range_col)` over a column of one of the six range types
([ranges.md](ranges.md)). `leaf_key` = the range's `range-bounds` encoding; `union` = the covering
range `[min(lowers), max(uppers)]` (empty ranges contribute nothing; an unbounded side makes the
union unbounded). `consistent` decides descent per operator:

| Strategy | Operator | Descend into `node` iff … |
|---|---|---|
| overlaps | `&&` | `node` overlaps the query range |
| contains | `@>` | `node` overlaps the query range / element |
| contained-by | `<@` | `node` overlaps the query range |
| left/right of | `<<` `>>` | `node` can hold a range strictly left/right of the query |
| adjacent | `-\|-` | `node` is adjacent-or-overlapping the query |
| not-extends | `&<` `&>` | `node` admits a range not extending past the query |
| equal | `=` | `node` contains the query bounds |

The planner pushdown seam ([indexes.md §5](indexes.md), the GIN precedent) gains a **GiST
consistent-descent gather**: descend from the root, visiting only children whose bounding key is
`consistent` with the query, collecting candidate storage keys at the leaves; then **always
recheck** the residual operator on each candidate row (the GIN always-recheck posture, [gin.md §2](gin.md)
— unobservable in results, only adds the already-metered `operator_eval`). Same rows as the
full-scan residual, lower cost.

**GX1 acceleration scope.** GX1 accelerates **`&&` (overlaps) and `@>` (contains)** — the two whose
conservative descend predicate is exactly `range_overlaps(node_union, query)` (a matching row must
overlap the query, and every row lies in its subtree's union, so a non-overlapping union holds no
match — pruning is sound). This mirrors GIN shipping `@>`/`&&` first. The remaining operators stay
**full-scan** this slice and are follow-ons (§11): `<<`/`>>`/`&<`/`&>`/`-|-` need bespoke
positional descend predicates; `<@` and `=` interact with **empty-range rows** (`empty <@ Q` and
`empty = empty` match, but an empty bound is absorbed by `range_merge` and so is invisible to the
union — a false-negative trap), as does an **empty query** (`col @> 'empty'` matches every row); all
are deferred rather than risk an unsound prune — the GIN-`<@` precedent. Empty-range *rows* are
indexed and correct under `&&`/`@>` (they simply never match a non-empty query). Applies to SELECT
and GiST-bounded UPDATE/DELETE.

## 6. Scalar `=` opclasses — the in-core `btree_gist` equivalent (GX2) ✅ LANDED

A GiST opclass over a **keyable scalar** whose bounding key is `[min, max]` over that type's
**order-preserving key encoding** ([encoding.md §2](encoding.md)) and whose only strategy is **`=`**
(`consistent`: descend iff the node's `[min,max]` brackets the query value's key bytes). The bound is
two length-prefixed key blobs, compared / unioned / descended as **raw bytes** — the key encoding makes
byte order reproduce value order, so the opclass needs **no value decode, no per-type comparator, and
no collation context** at compare time. The executor encodes a row value (and the equality constant)
to its key bytes via the shared `encode_key_value` ([encoding.md §2](encoding.md)); the tree only ever
compares. PostgreSQL needs the `btree_gist` *extension* for this; jed owns its surface (§1) so it ships
in-core (a documented divergence, §10). Its purpose is to let a **multi-column** GiST index carry a `=`
column beside a `&&` range column — the canonical exclusion shape (§7). Standalone it is a
(cost-inferior) alternative to the ordered B-tree and not something a user would reach for; it exists
for the constraint case.

**GX2 ships the FIXED-WIDTH keyables first** — the integers, `boolean`, `uuid`, `date`, `timestamp`,
`timestamptz` (exactly GIN's `is_gin_element_type` set) — whose key encoding is collation-free and
infallible. The **variable-width / collation-sensitive** keyables — `text`, `bytea`, `decimal`,
`interval` — are a deferred follow-on (§11): a column of one is `0A000` ("not supported yet", on the
roadmap), the GIN element-staging precedent. A column with no GiST opclass at all (`float` / `json` /
`array` / composite / `jsonpath`) is `42704` ("no default operator class", PG's wording).

**Persistence — no format bump (§8).** A scalar `=` index reuses v20's GiST page types `5`/`6`
(§4.1); the on-disk bound is the `[min, max]` key-blob pair instead of a range body, **distinguished
from a range bound only by the indexed column's catalog type** (range → `range_ops`, scalar →
scalar `=`). The page walk that repopulates the leaf store on load is opclass-agnostic (it copies the
bound bytes verbatim), so the same reader handles either. Golden: `gist_scalar_table.jed`
(`rust == go == ts == ruby`).

**A scalar GiST index is the FALLBACK bound** (cost.md §3, the scan-bound precedence). A `col = const`
over a column that is the primary key or has an ordered B-tree index takes the cheaper PK / index
bound; the GiST `=` gather fires only when a GiST index is the *only* index on that column. The
ordered-index pushdown explicitly **skips** a GiST index (its store key is `[v, v]‖skey`, not the
ordered-index key form, so it must never be probed as a B-tree).

## 7. `EXCLUDE` constraints (GX3) ✅ LANDED

```sql
[CONSTRAINT name] EXCLUDE [USING gist] (expr WITH operator [, expr2 WITH operator2 ...])
```

The constraint guarantees: **no two distinct rows `R1`, `R2` make every element comparison
`R1.expr_i  operator_i  R2.expr_i` simultaneously TRUE.** `UNIQUE` is the degenerate all-`=`
case. The canonical example —

```sql
EXCLUDE USING gist (room WITH =, during WITH &&)   -- no double-booking
```

— forbids two rows with the same `room` *and* overlapping `during`; it needs the scalar `=`
opclass (§6, GX2) for `room` and `range_ops` (§5, GX1) for `during`. A single-column range
exclusion (`EXCLUDE USING gist (during WITH &&)`) needs only GX1.

- **The NULL rule (PG-matched).** The WITH operators are strict; if any `expr_i` is NULL for a
  row, that element comparison is not TRUE, so the row **never conflicts** and is always accepted
  (PostgreSQL behavior — a row with a NULL in an excluded column is exempt). No `23502` here; NULL
  is simply not excludable.
- **Enforcement** lives inside the two-phase / all-or-nothing pass at INSERT and UPDATE
  ([constraints.md §5.4](constraints.md), the UNIQUE precedent), **after** the primary-key
  duplicate check: probe the backing GiST index with a `consistent`-descent over the conjunction
  of operators to gather candidate rows, then evaluate the full `(expr_i operator_i)` conjunction
  against each candidate. Any candidate for which **every** comparison is TRUE → trap **`23P01`**
  (`exclusion_violation`):

  ```
  conflicting key value violates exclusion constraint: <name>
  ```

  `<name>` is the constraint's backing GiST index name. End-state semantics follow UNIQUE's: an
  UPDATE that re-keys is validated against the statement's *end state*, so an end-state-valid swap
  succeeds where PG fails a per-row transient (the documented `UNIQUE` end-state divergence,
  [constraints.md §6.5](constraints.md)).
- **Columns-only WITH exprs first** (a bare column reference); a general expression
  (`EXCLUDE USING gist ((lower(name)) WITH =)`) is a deferred follow-on (§11).
- **Supported WITH operators: `&&` (a range column) and `=` (a fixed-width keyable scalar)** — the
  symmetric operators the §5/§6 opclasses serve. A `&&` over a non-range column or a `=` over a
  no-opclass type is `42704`; a `=` over a deferred keyable (`text`/`bytea`/`decimal`/`interval`) is
  `0A000`; any other operator is `0A000` (the broader operator set is a §11 follow-on). The backing
  GiST index cannot be `DROP INDEX`'d directly (it is owned by the constraint — `2BP01`, the
  UNIQUE-backing precedent; jed has no `ALTER TABLE … DROP CONSTRAINT` yet). `EXCLUDE` on a TEMP
  table is `0A000` (the GiST-on-temp narrowing, §11).
- **Implementation.** The constraint resolves at `CREATE TABLE` into a **multi-column GiST index**
  (one opclass per `WITH` column, §2 generalized to a tuple bound — the per-column component bounds
  concatenated; a single-column index is the degenerate one-component case, byte-unchanged) plus a
  per-table exclusion catalog entry recording the `(column, operator)` vector (§8). Each write's
  `gist_entries` builds the row's tuple leaf key (skipping a row with any NULL excluded column); a
  read never uses the multi-column index (the planner gather is single-operator), so the backing
  tree is probed **only** by the constraint. The probe builds the per-column `(query, strategy)`
  conjunction and descends the resident tree (whose leaf recheck IS the full conjunction, so a hit
  is a genuine conflict); the in-batch new-row-vs-new-row case is a direct pairwise conjunction
  (the resident tree holds only stored rows). Both the empty-range exemption and the NULL rule
  short-circuit the probe (return "exempt"), which also sidesteps the empty-range overlap-descend
  trap (§5).

## 8. Persistence

**GX1 — the index-kind discriminator (a `format_version` bump).** The per-index catalog entry
([../fileformat/format.md](../fileformat/format.md) *Catalog*) already carries the one-byte
`index_kind` between `index_flags` and `index_root_page` (`0` = ordered B-tree, `1` = GIN, added
in v13). GX1 claims **`index_kind = 2` = GiST** (today a value `> 1` is `XX001`), which is a clean
version break (CLAUDE.md §1): GX1 lands `format_version` **20**, a reader accepts only v20. A GiST
index always has `index_flags` bit0 (`unique`) clear (§1). The node/record framing is the ordinary
index tree's; only the entry *key bytes* differ (a bounding key, §4). Golden: `gist_range_table.jed`
(`rust == go == ts == ruby`, the §8 cross-core round-trip).

**GX2 — no further bump.** The scalar `=` opclass (§6) reuses v20's GiST page types `5`/`6` and the
`index_kind = 2` discriminator unchanged; only the *content* of a bound differs (a `[min, max]`
key-blob pair instead of a range body), and which flavor a node holds is determined by the **indexed
column's catalog type** (range → range bounds, scalar → scalar bounds). So a v20 file may now contain
either flavor with no version change, and the page walk that repopulates the leaf store on load is
opclass-agnostic (it copies bound bytes verbatim). Golden: `gist_scalar_table.jed`
(`rust == go == ts == ruby`).

**GX3 — the exclusion-constraint catalog entry (`format_version` 21).** ✅ Unlike UNIQUE, an
exclusion constraint must persist the **operator per column** (UNIQUE is always `=` and records
nothing constraint-specific, [constraints.md §5.5](constraints.md)). GX3 adds a per-table exclusion
list **after the foreign-key list**: a count, then per exclusion the constraint `name`, its backing
GiST index name, and a `(column_ordinal u16, operator_strategy u8)` pair per element (`&&` = 0,
`=` = 1). The backing GiST index itself is stored like any GiST index — the index list now admits
**multi-column** GiST indexes whose leaf/interior bound is the per-column component bounds
concatenated, so a single-column GX1/GX2 index is byte-unchanged; the exclusion entry layers the
operator vector the §7 probe needs on top. A table with no exclusion still moves to v21 by its
version byte + the zero count. Golden: `gist_exclude_table.jed` (`rust == go == ts == ruby`).

## 9. Cost

A new **`gist_descent`** cost unit (per interior node visited during a consistent-descent), beside
the existing `page_read` per node ([cost.md §3](cost.md)). The always-recheck residual
`operator_eval` is metered as today. `CREATE INDEX … USING gist` charges its build scan (`page_read`
× the table node count + `storage_row_read` per row + the bounding-key `operator_eval`s); an empty
table charges 0. `DROP INDEX` charges 0. Cost is cross-core identical (§3) and pinned by `# cost:`
corpus directives when GX1 lands.

## 10. Divergences from PostgreSQL (documented per CLAUDE.md §1)

- **Structural, not behavioral (the §3 decision).** jed's GiST tree *shape* differs from PG's
  (deterministic linear split + least-enlargement penalty vs. PG's quadratic heuristics) and is
  insertion-order-sensitive but cross-core byte-identical. Which rows match / which writes are
  rejected is identical to PG. A plan/cost/bytes divergence, never a result divergence.
- **`btree_gist` is in-core**, not an extension (§6) — jed owns its surface, so a scalar `=`
  column in a GiST exclusion needs no `CREATE EXTENSION`.
- **No system-catalog surface** — PG exposes GiST via `pg_am`/`pg_index`/`pg_constraint`; jed has
  none (auto-names observable via `DROP INDEX`/collisions and the host catalog in per-core tests),
  as for the ordered index and GIN ([gin.md §9](gin.md)).
- **Type names / canonical operators** inherit the range-type divergences ([ranges.md §12](ranges.md));
  no GiST-specific ones beyond the structural note.

## 11. Deferred follow-ons (none foreclosed by GX0's seam)

Each is its own vertical slice with a NoREC/oracle obligation ([conformance.md §8](conformance.md)):

- **The scalar `=` opclass over the VARIABLE-width / collation-sensitive keyables** — `text`,
  `bytea`, `decimal`, `interval` (GX2 ships the fixed-width keyables; these are `0A000` for now,
  §6). `text` additionally needs the column collation threaded into the key encoding (the ordered
  index's `key_collation_ctx` precedent), so its sort-key bound matches the collated probe. Each is
  its own small slice; the codec already length-prefixes the bound, so no node-format change.
- **The `EXCLUDE … WHERE (predicate)` partial form**; **general-expression** WITH operands;
  `EXCLUDE USING btree (a WITH =)` lowering an all-`=` exclude onto an ordered unique index (a
  `UNIQUE` alias); `ALTER TABLE … ADD CONSTRAINT … EXCLUDE`.
- **`DEFERRABLE` / `INITIALLY DEFERRED`** — jed has no deferred-constraint machinery yet; its own
  axis (a constraint queue checked at commit), not GiST-specific.
- **Future opclasses, behind the §2 seam unchanged** — `multirange_ops` (once a multirange type
  lands, [ranges.md §10](ranges.md)); an `hstore`/dictionary-type opclass (`@>`/`?`/`?&`/`?|`, also
  a GIN opclass, [gin.md §10](gin.md)); a `pg_trgm`-style trigram `text` opclass (similarity `%` /
  `LIKE` / `ILIKE` — jed's regex is its own flavor, [regex.md](regex.md), so accelerating it needs
  care); an `intarray`-style signature opclass over array columns (an alternative to the GIN
  `array_ops`). jed ships no geometric types, so the PG built-in `point`/`box`/`poly`/`circle`
  opclasses and GiST **KNN** (`ORDER BY col <-> const`, needs a distance scalar) are absent until
  such a type lands — at which point it slots into the same seam.
- **SP-GiST** (`index_kind = 3`) — a space-partitioning sibling, a separate access method, not
  scheduled.
- **GiST on a TEMP table** — `0A000` in GX1: the resident R-tree (§4.1) would live on the temp
  snapshot, deferred with the rest of the container-on-temp work
  ([temp-tables.md](temp-tables.md)). A persistent table's GiST index is fully supported.
- **Quality / perf refinements** — a better (still deterministic) split than median-linear;
  bulk-load packing for `CREATE INDEX`; node-level skip during descent. GX1's resident tree is
  rebuilt **canonically and whole** at each mutating statement (§4.1) — content-deterministic and
  simple, but O(rows) per mutation; **incremental copy-on-write GiST maintenance** is the perf
  follow-on (the COW table B-tree's precedent), as is **demand-paging** the tree rather than
  eager-loading it on open (the pager seam stays open, [pager.md](pager.md)).

## 12. Slice delivery

GX0 is spec-only and unblocks the rest; GX1 is independently useful (a range overlap accelerator,
the [ranges.md §10](ranges.md) follow-on) before any constraint exists.

**GX0 + GX1 + GX2 + GX3 have LANDED** across all three cores (Rust/Go/TS) + the Ruby golden reference,
byte-identical (`rust == go == ts == ruby`). GX1's implementation realizes §3/§4.1 concretely: the GiST
index's **in-memory** form is the flat leaf-key store (the GIN `term ‖ skey` precedent, so all
insert/update/delete maintenance is reused), with a **resident R-tree** rebuilt **canonically**
(`build_from_leaf_keys`, content-deterministic) at each mutating statement; the planner gather descends
that resident tree (`page_read` per node + `gist_descent` per interior). The **on-disk** form is the
persisted R-tree (pages 5/6, `format_version` 20), serialized from the canonical leaf set at commit and
parsed back into the leaf store + resident tree on open — so the in-memory and on-disk trees are the
*same* pure function of the row set, and the cross-core round-trip holds. **GX2** generalized the tree
core to an opclass seam (the only type-specific part) and added the **scalar `=` opclass** over the
fixed-width keyables — bounds are `[min, max]` over the order-preserving key encoding, compared as raw
bytes — reusing the entire tree machinery, maintenance, gather, persistence (no format bump), and cost
unit unchanged. **GX3** generalized that seam once more to **multi-column** GiST indexes (a tuple bound,
the per-column components concatenated — single-column bytes unchanged) and added **`EXCLUDE`
constraints** (§7): the constraint *is* its backing multi-column GiST index, enforced by a
conjunction probe inside the two-phase INSERT/UPDATE pass (`23P01`), with the NULL rule + end-state
semantics; the exclusion catalog entry (the `(column, operator)` vector) bumps `format_version` 21.
The whole feature has landed.

| Slice | Content |
|---|---|
| **GX0** ✅ | this doc + the §3 determinism decision + the §2 opclass seam + register `23P01` + reserve `index_kind = 2` / the `format_version` bump (no code, no corpus, no format change) |
| **GX1** ✅ | the GiST index *kind* + `range_ops` (§5) + the consistent-descent planner gather (SELECT + UPDATE/DELETE) + the `gist_descent` cost unit + `format_version` 20 (`index_kind = 2`, pages 5/6) + the `gist_range_table.jed` golden + `CREATE UNIQUE … USING gist` → `0A000` (+ multi-column / temp → `0A000`); capabilities `ddl.gist_index` / `query.gist_scan`; the `query/gist_scan.test` corpus (oracle-clean) + the `gist` NoREC relation |
| **GX2** ✅ | the scalar `=` opclass (§6) — the in-core `btree_gist` equivalent over the **fixed-width** keyables (integers / boolean / uuid / date / timestamp / timestamptz); the opclass seam (the tree core generalized to `range_ops` + scalar `=`); bounds are `[min,max]` over the order-preserving key encoding (raw-byte compare); the planner `=` gather (the cost-inferior fallback bound — ordered-index pushdown skips a GiST index); persisted within v20's pages 5/6 (no bump — bound flavor keyed off the column type) + the `gist_scalar_table.jed` golden; deferred keyable (text/bytea/decimal/interval) → `0A000`, no-opclass type → `42704`; capabilities `ddl.gist_scalar_index` / `query.gist_scalar_scan`; the `query/gist_scalar_scan.test` corpus + the `gist_scalar` NoREC relation |
| **GX3** ✅ | `EXCLUDE [USING gist] (col WITH op, …)` (§7) — the backing-index constraint, the conjunction probe (INSERT + UPDATE), `23P01`, the NULL rule + empty-range exemption, end-state semantics; the tree core generalized to **multi-column** (a tuple bound); the per-table exclusion-constraint catalog entry (`format_version` 21) + the `gist_exclude_table.jed` golden; `&&`/`=` operators (others `0A000`), backing-index `DROP INDEX` → `2BP01`, exclude-on-temp `0A000`; capability `ddl.exclusion_constraint`; the `ddl/exclusion_constraint.test` corpus (the multi-column form is a jed in-core divergence — PG needs `btree_gist`) |
