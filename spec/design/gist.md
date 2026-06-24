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
  - **`penalty`** (choose-subtree): descend into the child whose bounding key needs the **least
    enlargement** to cover the new entry; ties broken by the **smaller resulting bounding-key
    bytes**, then by **lower child slot index** — a *total* order, never a coin-flip.
  - **`picksplit`**: when a node overflows, **sort its entries by their bounding-key bytes** (the
    order-preserving encoding §4 gives a total byte order) and split at the **median**; recompute
    each half's `union`. A deterministic linear split — *not* PG's quadratic heuristic. Index
    *quality* (fan-out balance) is a cost concern, never a correctness one (§9).
  - **node entry order** within a page is the sorted bounding-key-byte order, so a node's bytes are
    a pure function of its entry *set*.
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

Bounding keys reuse the engine's existing **order-preserving key encodings** — no new on-disk
primitive. `range_ops`' bounding key is the `range-bounds` encoding ([encoding.md §2.11](encoding.md))
covering `[min lower bound, max upper bound]` with the empty / ±∞ / inclusivity framing of the §6
range total order; a scalar opclass' bounding key is `[min, max]` over the value's existing scalar
key encoding (§6). Because the bounding key is just bytes with a total order, `picksplit`'s
median-sort and `penalty`'s tie-breaks are well-defined for *every* opclass uniformly (§3).

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
full-scan residual, lower cost. Accelerates `&&`/`@>`/`<@`/`<<`/`>>`/`&<`/`&>`/`-|-`/`=` for SELECT
and GiST-bounded UPDATE/DELETE.

## 6. Scalar `=` opclasses — the in-core `btree_gist` equivalent (GX2)

A GiST opclass over each **keyable scalar** (the integers / `boolean` / `uuid` / `date` /
`timestamp` / `timestamptz` / `text` / `bytea` / `decimal` / `interval` — exactly the set with an
order-preserving key encoding) whose bounding key is `[min, max]` over that encoding and whose only
strategy is **`=`** (`consistent`: descend iff the node's `[min,max]` brackets the query value).
PostgreSQL needs the `btree_gist` *extension* for this; jed owns its surface (§1) so it ships
in-core. Its purpose is to let a **multi-column** GiST index carry a `=` column beside a `&&` range
column — the canonical exclusion shape (§7). Standalone it is a (cost-inferior) alternative to the
ordered B-tree and not something a user would reach for; it exists for the constraint case.

## 7. `EXCLUDE` constraints (GX3)

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

## 8. Persistence

**GX1 — the index-kind discriminator (a `format_version` bump).** The per-index catalog entry
([../fileformat/format.md](../fileformat/format.md) *Catalog*) already carries the one-byte
`index_kind` between `index_flags` and `index_root_page` (`0` = ordered B-tree, `1` = GIN, added
in v13). GX1 claims **`index_kind = 2` = GiST** (today a value `> 1` is `XX001`), which is a clean
version break (CLAUDE.md §1): GX1 lands `format_version` **20**, a reader accepts only v20. A GiST
index always has `index_flags` bit0 (`unique`) clear (§1). The node/record framing is the ordinary
index tree's; only the entry *key bytes* differ (a bounding key, §4). Golden: `gist_range_table.jed`
(`rust == go == ts == ruby`, the §8 cross-core round-trip).

**GX3 — the exclusion-constraint catalog entry (a further `format_version` bump).** Unlike UNIQUE,
an exclusion constraint must persist the **operator per column** (UNIQUE is always `=` and records
nothing constraint-specific, [constraints.md §5.5](constraints.md)). GX3 adds a per-table exclusion
list after the index list: each entry the constraint `name`, its backing-index reference, and a
`(key_ordinal, operator_strategy)` pair per element. The backing GiST index itself is stored like
any GiST index; the exclusion entry layers the operator vector the §7 probe needs on top.

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
- **Quality / perf refinements** — a better (still deterministic) split than median-linear;
  bulk-load packing for `CREATE INDEX`; node-level skip during descent.

## 12. Slice delivery

GX0 is spec-only and unblocks the rest; GX1 is independently useful (a range overlap accelerator,
the [ranges.md §10](ranges.md) follow-on) before any constraint exists.

| Slice | Content |
|---|---|
| **GX0** | this doc + the §3 determinism decision + the §2 opclass seam + register `23P01` + reserve `index_kind = 2` / the `format_version` bump (no code, no corpus, no format change) |
| **GX1** | the GiST index *kind* + `range_ops` (§5) + the consistent-descent planner gather + the `gist_descent` cost unit + `format_version` 20 (`index_kind = 2`) + the `gist_range_table.jed` golden + `CREATE UNIQUE … USING gist` → `0A000`; capabilities `ddl.gist_index` / `query.gist_scan` |
| **GX2** | the scalar `=` opclass family (§6) — the in-core `btree_gist` equivalent over the keyable scalars |
| **GX3** | `EXCLUDE [USING gist] (col WITH op, …)` (§7) — the backing-index constraint, the conjunction probe, `23P01`, the NULL rule, multi-column; the exclusion-constraint catalog entry (a further `format_version` bump); capability `ddl.exclusion_constraint` |
