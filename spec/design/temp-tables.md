# Temporary tables — design

> **Session-local** temporary tables that make **zero writes to the database file** — they live
> entirely *outside* the serialized snapshot, so they cause no catalog write, no page write, and no
> txid churn on the main file. Resident in RAM until they outgrow `temp_buffers`, then spilled to a
> private scratch file (§6); bounded by a **deterministic storage ceiling** so they preserve the untrusted-SQL safety guarantee
> (CLAUDE.md §13). The **grammar is authoritative** for the surface
> ([../grammar/grammar.ebnf](../grammar/grammar.ebnf) — `create_table`, `table_scope`); the **error
> registry** ([../errors/registry.toml](../errors/registry.toml)) owns the codes (one new code —
> `54P03`); this doc is the *why* and the precise behavior the three cores reproduce identically
> (CLAUDE.md §2, §8). When a decision here changes, change the grammar and this doc in the same edit.
>
> **Status: landed (all three cores); session-local temp rides a MemoryBlockStore.** Session-local temp
> tables — `CREATE [TEMP|TEMPORARY] TABLE` + `DROP`, the per-session temp store, the namespace
> preclude-overlaps check (§3), the `allow_temp_ddl` capability split (§5), `max_temp_storage_bytes` +
> `54P03` (§7), constraints / indexes / `serial` / composite columns (§8), cost (§9) — are implemented
> byte-identically in Rust, Go, and TS. The session-local domain rides a per-domain in-RAM
> `MemoryBlockStore` + pager with within-session free-list compaction, and its `54P03` ceiling is the
> domain's committed **page** bytes (§6/§7). **Slice 3 (spill-to-disk) has landed:** past
> `temp_buffers` a file host migrates the domain to a scratch file in its spill directory and bounds
> the domain's page cache by `temp_buffers` (§6). **`ON COMMIT { PRESERVE ROWS | DELETE ROWS | DROP }`
> has landed** (§5.1), PostgreSQL's end-of-transaction actions, and so has **`CREATE TEMP TABLE … AS`**
> (§1, [create-table-as.md](create-table-as.md)). This doc was written first, spec-first
> (CLAUDE.md §11).
>
> **A database-wide `SHARED` temp kind was designed and briefly shipped (former slice 2), then REMOVED**
> — Slice 0 of the attached-databases plan (2026-07-03). Its `Database`-scoped, *in-process* sharing
> boundary would clash with any future **multi-process** file access, so the concept is retired in favor
> of a `Database`-scoped in-memory **attachment** ([attached-databases.md](attached-databases.md) §6).
> The `SHARED` keyword, `allow_shared_temp_ddl`, `shared_temp_mem`, the `Database`-level temp snapshot,
> and the two-root commit are gone; the commit publishes a single durable root again. This doc is now
> **session-local only** — the shared kind survives only as a recorded, removed divergence (§12) and in
> the slicing history (§13). **Slice 3 (spill-to-disk, §6) has landed** — the flip put temp on its
> seam, so spill was a `BlockStore` swap plus a bounded pool.

Temporary tables are jed's first relations whose lifetime is shorter than the database file's and
whose data is **deliberately never durable**. They diverge from PostgreSQL in several recorded ways
(§12); each is a conscious tradeoff, not an accident, taken against the §1 "match PG unless an
overriding reason" rule.

## 1. Surface

```sql
CREATE [ TEMP | TEMPORARY ] TABLE name ( table_element [, ...] )
    [ ON COMMIT { PRESERVE ROWS | DELETE ROWS | DROP } ]
CREATE [ TEMP | TEMPORARY ] TABLE name [ ( column_name [, ...] ) ]
    [ ON COMMIT { PRESERVE ROWS | DELETE ROWS | DROP } ] AS query [ WITH [ NO ] DATA ]
```

```ebnf
create_table   ::= "CREATE" table_scope? "TABLE" identifier
                   ( "(" table_element ("," table_element)* ")" on_commit? | create_table_as )
table_scope    ::= "TEMPORARY" | "TEMP"
on_commit      ::= "ON" "COMMIT" ( "PRESERVE" "ROWS" | "DELETE" "ROWS" | "DROP" )
```

- `TEMP`/`TEMPORARY` are synonyms (PG), **not reserved** (grammar.md §3), recognized **positionally**
  between `CREATE` and `TABLE`. A table may still be *named* `temp`/`temporary` — the word after
  `TABLE` is always the table name, so `CREATE TABLE temp (...)` is an ordinary persistent table named
  `temp`.
- Two table kinds result, both sharing the existing `table_element` grammar (columns, `PRIMARY
  KEY`, `CHECK`, `UNIQUE`, `FOREIGN KEY` — an FK stays within its kind, §8):
  - **persistent** — `CREATE TABLE` (unchanged; the only kind that touches the file).
  - **session-local temp** — `CREATE [TEMP|TEMPORARY] TABLE` — private to the creating session.
- `DROP TABLE name` drops a temp table exactly as it drops a persistent one (it resolves through
  the same namespace, §3). No grammar change for `DROP`.
- `ON COMMIT` chooses what happens to a temp table when a transaction commits (§5.1): `PRESERVE
  ROWS` (the default) keeps it, `DELETE ROWS` empties it, `DROP` drops it. Writing any `ON COMMIT`
  clause on a table that is not temporary, `PRESERVE ROWS` included, is **`42P16`** *"ON COMMIT can
  only be used on temporary tables"* (PostgreSQL).
- **`CREATE TEMP TABLE … AS query [WITH [NO] DATA]`** makes a temp table from a query
  ([create-table-as.md](create-table-as.md)): its columns are the query's output columns and, unless
  `WITH NO DATA`, its rows the query's, filled in the same statement. `ON COMMIT` comes before `AS`
  (PostgreSQL's position): `CREATE TEMP TABLE t ON COMMIT DROP AS SELECT …`. The gate, the namespace,
  and the §8 narrowings are this section's — a collated text column is the §8 `0A000`.
- `CREATE [TEMP|TEMPORARY] TABLE IF NOT EXISTS` is supported exactly as for a persistent table
  ([grammar.md](grammar.md) §13): a collision under the preclude-overlaps namespace (§3) — a temp
  name already taken by a temp **or** persistent relation — is a no-op success rather than `42P07`.
  So with a persistent `p`, `CREATE TEMP TABLE IF NOT EXISTS p (...)` creates nothing, where
  PostgreSQL would create a `pg_temp.p` shadowing it — the §3 divergence, seen through `IF NOT
  EXISTS`. The `allow_temp_ddl` gate (§5) still applies to the no-op.

## 2. The one idea that makes everything else fall out: temp state lives outside the snapshot

jed serializes exactly one thing to the file: the committed `Snapshot` (its `tables` / `stores` /
`index_stores` / `sequences` / `types`, written by `incremental_image()` →
[storage.md §4](storage.md), [transactions.md §2](transactions.md)). **Anything not in that
`Snapshot` is never serialized.** So the entire "no file writes" requirement (CLAUDE.md divergence
D1) reduces to a single structural rule:

> **Temp-table catalog entries and stores are held in a separate in-memory structure that is never
> part of the serialized `Snapshot`.**

No `format_version` bump, no catalog-page change, no golden-fixture move, no Ruby-reference change —
because the on-disk format is *literally untouched*. This is the headline property and the cheapest
possible way to satisfy D1: temp tables cannot dirty a file page because they are not reachable from
any root that the commit path walks.

The never-serialized store:

- **Session-local temp catalog** — a per-`Session` structure (`temp_tables` + `temp_stores` +
  `temp_index_stores` + `temp_sequences`), alongside the session's existing per-session state
  ([session.md §2](session.md): `vars`, `session_seq`, privileges). Born empty with the session,
  dropped wholesale at session close.

Because temp stores reuse the existing `TableStore` / B-tree / value codec / comparator verbatim,
their *behavior* (rows, ordering, comparisons, errors, cost) is cross-core byte-identical by
construction — the §5/§8 "derived from already-identical pieces" argument (CLAUDE.md §5,
extensibility.md §4.1). The only thing that is *not* in the cross-core file contract is an eventual
spill file (§6, §11).

## 3. Namespace — overlaps are precluded (a PG divergence)

jed has one flat relation namespace (tables/indexes/sequences compete — indexes.md §2); there is no
schema and no `search_path`. Temp tables join it under a **preclude-overlaps** rule (divergence D2;
PostgreSQL instead *shadows* a permanent table with a like-named `pg_temp` one). Concretely:

- **The persistent namespace is one global space.** Any collision within it — a persistent table,
  index, or sequence named like another — is `42P07` (`duplicate_table` / "relation already exists"),
  checked at `CREATE`, exactly as two persistent tables collide today. This half is globally
  enforceable because persistent state is global, gated state (§5).
- **A session-local `CREATE [TEMP] TABLE` is checked against the creating session's *entire visible
  scope*** — its own session-local temps ∪ the persistent relations — and a collision is `42P07`. So
  *within one session* you can never create two relations of the same name; there is no "which `t`?"
  ambiguity to reason about.

**Resolution** walks **session-local → persistent** at the single chokepoint (`Snapshot::table()` /
the executor's name resolver). Given the creation-time checks above, **at most one of the two ever
matches**, so the order is normally just *where the row lives*, not a precedence contest. A missing
name is `42P01` as today.

**The one unpreventable cross-session race, and why the lookup order still has teeth.** Session-local
temps are *private* — session B cannot see session A's session-local `t`, so B's persistent
`CREATE TABLE t` cannot be rejected on account of A's private `t`. If that global `t` is created
*after* A already holds a private `t`, A now sees two. We resolve this deterministically with
**session-local-wins** (the first rung of the walk): A keeps resolving `t` to its own table — the
least-surprising outcome for A (it sees what it made), and no other session is affected. This residual
is the *only* case the lookup order ever decides; in all reachable-by-one-session states, creation
already precluded the overlap. (We deliberately do **not** maintain a global registry of every
session's private temp names to slam this window shut: it would force cross-session coordination on a
private, gate-free structure — the opposite of what makes session-local temps cheap, §6.)

## 4. Session-local temp vs. persistent

There are two table kinds (§1). A session-local temp table is private to the session that created it
and never touches the file; a persistent table is global and durable.

| | session-local temp | persistent |
|---|---|---|
| Visibility | creating session only | all sessions |
| Lives in | per-`Session` store (§2) | serialized `Snapshot` |
| Writes touch the file? | **never** | yes (commit) |
| Single-writer gate on write? | **no** (private) | yes |
| Transactional? | yes (session txn) | yes |
| Dropped at | session close (or `DROP`) | `DROP` only |
| Survives database reopen? | n/a | yes |

> **Historical note — a removed third kind.** A database-wide `SHARED` temp table (`CREATE SHARED TEMP
> TABLE`), visible to and writable by every session of one open `Database` but never written to the
> file, was designed and briefly shipped (former slice 2), then **removed** (Slice 0, 2026-07-03). Its
> sharing boundary was an *in-process object* (the `Database` lifetime), which would clash with any
> future multi-process file access; the capability is re-provided, if needed, by a `Database`-scoped
> in-memory **attachment** ([attached-databases.md](attached-databases.md) §6). See §12 (removed
> divergences) and §13 (slicing history).

## 5. Concurrency, transactions, and the commit boundary

This is where a session-local temp table differs from a persistent one, and where the single-writer
model (CLAUDE.md §3) is respected without ever fsyncing temp data.

**Session-local temp writes take no global writer gate.** The data is private, so there is nothing to
serialize against other sessions. A session-local `INSERT`/`UPDATE`/`DELETE` mutates the session's
own temp store directly. A direct, useful consequence: **even a read-only session can use a
session-local temp table as scratch space** — it pins an immutable file snapshot for *persistent*
reads while freely writing its *private* temp store, with no contradiction. This matches PostgreSQL,
which explicitly permits temp-table writes inside a read-only transaction, and it is exactly the
property a host wants when handing an untrusted, `{SELECT}`-only session a scratch table.

**Session-local temp state rides the transaction, but never the file.** A session-local temp mutation
inside a write transaction accumulates in the session's private temp staging alongside any persistent
working set; commit folds it into the session's durable-for-session temp state (its own
`MemoryBlockStore` domain, §6) and `ROLLBACK` discards it. Read-your-own-writes within the transaction
works for the temp table exactly as it does for a persistent one. So does a data-modifying `WITH`'s
read pin ([writable-cte.md](writable-cte.md) §2): every sub-statement reads the temp domain's
pre-statement state, as it does main's, and only foreign keys among temp tables see the earlier
sub-statements' writes. Commit publishes a **single** durable
root — the file snapshot root (fsynced per `synchronous`, §9 of transactions.md); the session-local
temp domain is materialized into its own in-RAM store with **no fsync, nothing written to the main
file** (§6). A transaction that touches *only* session-local temp tables takes **no** global writer
gate at all (the data is private) and makes zero file writes.

**DDL gating — two capabilities, split by table kind.** `CREATE`/`DROP` is DDL (`42501` if denied —
session.md §5.3), but temp tables are precisely the safe, bounded scratch space a host *wants* to
expose to an untrusted session that may otherwise touch nothing. So the single `allow_ddl` gate splits,
by the kind of relation the statement creates/drops, into two session capabilities:

- **`allow_ddl`** *(existing, default on)* — now scoped to **persistent** DDL specifically
  (`CREATE`/`DROP`/`ALTER` of persistent tables, indexes, types, sequences). Its meaning narrows but
  its name and default are unchanged, so existing callers are unaffected.
- **`allow_temp_ddl`** *(new)* — `CREATE`/`DROP` of **session-local** temp tables.

**Back-compat by default-inheritance:** the new gate **defaults to `allow_ddl`'s value** when not set
explicitly. So a session left as-is behaves exactly as today (one `allow_ddl` governs everything),
while the untrusted-scratch pattern is `allow_ddl = off` (no persistent DDL) **+ explicit
`allow_temp_ddl = on`** — private scratch tables only, everything else denied. This keeps the §5.3
default-deny posture intact (an untrusted `{SELECT}`-only session has `allow_ddl = off`, so temp DDL is
off too until the host deliberately opts in) and relaxes the documented "one boolean" `allow_ddl`
narrowing (session.md §5.3) only for temp tables. The gate is configured per record by the directives
`# allow_ddl:` / `# allow_temp_ddl:` (§13).

### 5.1 `ON COMMIT` — end-of-transaction actions

`ON COMMIT` follows PostgreSQL (`PreCommit_on_commit_actions`). It records one of three actions on the
temp table's (never-serialized) catalog entry:

- **`PRESERVE ROWS`** (the default, and the only behavior before this slice) — nothing happens at
  commit.
- **`DELETE ROWS`** — every commit empties the table: its rows and every index's entries go. The
  table, its constraints, and indexes stay; its `ANALYZE` statistics stay too, marked stale as after
  any other write. Owned `serial`/IDENTITY sequences are **not** reset, as PostgreSQL's
  commit-time truncation does not restart them; the no-PK synthetic rowid counter is not reset either
  (it is internal and monotonic, §7 of transactions.md).
- **`DROP`** — the commit drops the table exactly as `DROP TABLE … CASCADE` would (PostgreSQL's drop
  is `CASCADE`): its indexes and the sequences it owns go with it, and a surviving temp table's
  `FOREIGN KEY` that references it is dropped (§8), and so is every temporary view that reads it or a
  sequence it owns, with the views over those ([views.md](views.md) §10). Only a temporary view can
  depend on a temporary table.

**When the actions run.** At every successful commit of a read-write transaction — `COMMIT`, an
autocommit statement's implicit commit, a host `commit()`/`update` closure, and the implicit
transaction around a script — the commit applies them to the transaction's working temp snapshot,
**before** the temp domain stages (§6) and before any budget precheck, truncations first and then
drops (PostgreSQL's order). The result is then committed like any other temp change, so the actions
are atomic with the transaction: if the commit itself fails, the whole transaction is discarded
(including the actions). A rolled-back or failed transaction (a failed block's `COMMIT` is a
`ROLLBACK`) and a read-only transaction publish no temp changes, so they run no action — nothing they
discard needs one.

Two invariants follow, and they are all the observable behavior: **a committed temp state never holds
an `ON COMMIT DROP` table, and an `ON COMMIT DELETE ROWS` table is always empty when a transaction
starts.** So under autocommit, rows written to a `DELETE ROWS` table are visible only within the
writing statement itself (its `RETURNING` rows, which a writable CTE passes on), and a `CREATE TEMP TABLE
… ON COMMIT DROP` is dropped as soon as its statement commits. Inside an explicit block both tables
behave like ordinary temp tables until `COMMIT`. A `DROP`-table's action survives renames and other
`ALTER TABLE` edits within its transaction, since it is part of the table's catalog entry.

**Interactions.**

- **Cost:** none. Commit is unmetered, and neither action reads a row, so a statement's cost is the
  same whatever its target's `ON COMMIT` action.
- **`max_temp_storage_bytes`:** the per-statement check (§7) runs before the commit, so rows written to
  a `DELETE ROWS` table count against the ceiling for the statement that writes them, and in an
  explicit block until `COMMIT`. The commit-time truncation or drop frees their pages, which the
  within-session compaction (§6) reclaims; repeated autocommit writes to a `DELETE ROWS` table
  therefore do not accumulate.
- **Prepared statements:** temp relations are never plan-cached, so a statement prepared against an
  `ON COMMIT DROP` table that has since been dropped fails `42P01` on its next execution, as after an
  explicit `DROP TABLE`.
- **`IF NOT EXISTS`:** a no-op create leaves the existing table's action unchanged.
- **Ordering of `42P16`:** like PostgreSQL (`transformCreateStmt` checks existence before
  `DefineRelation` checks `ON COMMIT`), the check runs after the `IF NOT EXISTS` skip and before any
  column or constraint is examined. A `main.`- or attachment-qualified table is not temporary (`42P16`);
  a `temp.`-qualified one is.
- **Foreign keys** (§8) follow PostgreSQL's `heap_truncate_check_FKs`. Before any action runs, if a
  table **outside** the `DELETE ROWS` set has an FK referencing a table **inside** it, the commit fails
  with **`0A000`** *"unsupported ON COMMIT and foreign key combination"* (detail: *Table "c" references
  "p", but they do not have the same ON COMMIT setting.*), and the transaction is discarded like any
  other failed commit. A `DELETE ROWS` table may reference itself, another `DELETE ROWS` table, or any
  other temp table; only an inbound reference from outside the set fails. Truncation precedes the
  drops, so an `ON COMMIT DROP` table referencing a `DELETE ROWS` one fails too, although the same
  commit would drop it — PostgreSQL's order. The check runs whether or not the tables hold rows, and
  in practice fails the very commit that creates the combination (a `CREATE TEMP TABLE … REFERENCES`
  or `ALTER TABLE … ADD FOREIGN KEY`), so a committed temp state never holds one; this is why running
  the actions at every read-write commit is observably the same as PostgreSQL running them only when
  the transaction touched a temp relation. When several pairs qualify, the error names the first
  truncated parent and then its first outside referencing table, each in ascending name byte order;
  PostgreSQL orders by OID (creation order), which jed does not have — a difference only in which pair
  the detail names.
- **Savepoints** (transactions.md §11) need no special handling. A table and its action are temp
  snapshot state, so a table created under a savepoint that is rolled back is gone with its action,
  and one created under a released savepoint is acted on at the top-level commit — PostgreSQL's
  subtransaction behavior (`transactions/savepoint.test`).

## 6. Storage model — a per-domain MemoryBlockStore that spills to a scratch file

**Session-local temp rides a per-domain in-RAM `MemoryBlockStore` + pager** — the *same* storage path
as an in-memory database (bplus-reshape.md B3), not a separate fully-resident decoded B-tree. The
temp-blockstore slice retired the original slice-1 "`paging: None`, `Child::Resident`" mode: the
session-local temp domain is now a private `MemoryBlockStore` seeded with the empty from-scratch image,
read/written through the *same* pager + packed-leaf path (`newTempStorage` / `Storage::new_temp`), with
a **pinned, unbounded pool** while it is resident. A temp `TableStore` `attach_paging`s that domain's
`SharedPaging`, so its leaves demote to `OnDisk` after each commit and fault back through the temp
pool — the **compact packed footprint** (resident memory ≈ `page_count × page_size`, not the inflated
`Value` tree). A temp commit **stages** its dirty pages (`stage_temp`): the same incremental
copy-on-write serialize as a file/in-memory commit, but **no meta slot and no `sync`** (a temp store is
never reopened; it has no durability barrier), so it makes **zero main-file writes** (D1) by
construction. It stages **before** the main persist and adopts its page accounting only after it (the
in-memory-attachment recipe, attached-databases.md §5): staged pages occupy only free-list slots and
pages past the high-water, so a failure anywhere in between — a scratch-file I/O error, a failed main
persist — leaves the temp store matching its still-committed root and commits nothing.

**Within-session compaction** is the prerequisite the flip needed. A `MemoryBlockStore` commits
copy-on-write, so every commit orphans its root→leaf path + the rewritten catalog; page reclamation
was previously *reconstruct-on-open only*, and a temp store is **never reopened** — so without
compaction it would leak a page per commit, breaking the storage ceiling the §13 untrusted-SQL
guarantee rests on. A reclaim domain (`reclaim_within_session`, set only by temp domains) instead
rebuilds its free-list from the **live reachable set** at commit (`maybe_compact`, reusing the same
reachability walk the open-time free-list reconstruction runs — the catalog chain + the in-memory tree
node pages + spillable-leaf overflow). It is **periodic** (walks only once the pages off the free list
pass ~2× the live count at the last compaction, so `page_count` stays within ~2× live and the walk is
amortized O(height)/commit — the trigger of [../fileformat/format.md](../fileformat/format.md)
*Reclamation*) and **watermark-gated** (deferred while any older version is pinned — a
read session or an open streaming cursor over the domain — so a page a live reader may still fault is
never freed). This reclamation carries **no cross-core byte contract**: a temp store is never
serialized (D1), so its physical page layout and reclamation are per-core (only the *logical*
observables — rows, cost, and the `54P03` abort point — stay cross-core-identical), which is what makes
the mechanism tractable versus the still-deferred general within-file reclamation. The main/file domain
keeps reconstruct-on-open only (`reclaim_within_session` false) and can opt in later.

**Spill-to-disk (slice 3) — a `BlockStore` swap plus a bounded pool.** When a temp commit leaves the
domain's committed page bytes (`page_count × page_size`, measured on the staged commit — not one
behind) **above `temp_buffers`**, and the host has a **spill directory**, the domain **spills**: a
private scratch file is created in that directory (exclusive, mode `0600` — the external-sort spill
file's creation rule, spill.md §6), the in-RAM store's bytes are copied into it in bounded chunks, and
the pager adopts it in place of the `MemoryBlockStore` (`migrate_store`). The page layout is
byte-for-byte unchanged, so every page id, every decoded pool entry, and every `OnDisk` reference
stays valid across the swap — nothing above the pager notices. From then on the domain's leaf pool is
**bounded** to `max(1, temp_buffers / page_size)` leaves (CLOCK eviction, pager.md §3) instead of
pinned, re-applied at each temp commit so a later `temp_buffers` change takes effect; the interior
skeleton stays resident, as in a demand-paged file (pager.md §2). A domain spills **once** and stays
spilled until the session closes (dropping a table does not migrate it back). The trigger is evaluated
only at commit, so a single large statement first stages in RAM and then spills — its working set was
resident anyway (it is the transaction's pending write set, bounded by `max_query_memory_bytes`,
memory.md §7).

- **The scratch file is internal and out-of-contract** (§10): never the §8 file format, never read by
  another core or host, never fsynced (its `sync` and grow barrier are no-ops — a crash loses only
  session-scoped data that was never durable). It is **unlinked immediately after creation** where
  the OS allows unlinking an open file (Unix), so a crash leaves nothing behind, and otherwise removed
  when the session closes (Windows). It is entirely separate from the main database file's
  `BlockStore`, so the zero-main-file-writes invariant (D1) holds by construction.
- **Where the spill directory comes from** is the host's, exactly as for the external sort (spill.md
  §2): a native file host uses the OS temp directory (never the database's own directory, so a
  read-only database directory never receives scratch files). An **in-memory database, an OPFS
  (browser) database, and a WASI host have no spill target**, so their temp domains stay resident past
  `temp_buffers` — bounded then only by the `54P03` ceiling (§7), the same "no spill target keeps it
  resident" rule the external sort follows.
- **Residency is invisible to the contract.** Spilling changes *where* pages live and when they are
  resident, never what a query observes: rows, cost (`page_read` is logical, §9), and the `54P03`
  point (§7, measured on the logical page count whether resident or spilled) are identical with or
  without a spill. Per-core white-box tests pin this neutrality (`temp_spill` tests).
- **It is not the external-merge-sort spill path** (spill.md), which writes sequential run files: a
  temp table needs random access by key (point lookups, index scans, ongoing mutation), so it reuses
  the **pager + buffer pool + B-tree** machinery, borrowing only spill.md's host scratch directory and
  creation rule.

## 7. Bounds — the deterministic storage budget (the §13 gate temp tables need)

Temp tables introduce a genuinely new hazard that the existing gates do not cover: **retained**
storage across statements. The cost meter (cost.md) bounds *work per statement*; the depth limit
bounds *parse nesting*; `lifetime_max_cost` bounds *cumulative work*. None of them bounds **bytes
held between cheap statements** — an adversary on a generously-budgeted session could
`CREATE TEMP TABLE t(x text); INSERT …; INSERT …;` and accrue unbounded RAM with little per-statement
cost. So temp tables add a **dedicated storage budget**, a new §13 resource gate independent of the
cost meter (the same way the depth gate is independent of the cost gate):

Two per-session settings govern temp storage, split by what they bound (the PostgreSQL shape: PG's
`temp_buffers` is the per-session temp *buffer* memory, and temp data beyond it lives in files):

- **`max_temp_storage_bytes`** — the **storage ceiling**, the §13 gate: the temp domain's committed
  page bytes may not exceed it, whether resident or spilled; an over-ceiling temp write aborts
  **`54P03`**. In-contract. `0` means unlimited (a trusted host). Default **1 GiB**. Named for the
  `max_storage_bytes` / `max_query_memory_bytes` family it belongs to (the same `page_count ×
  page_size` measure `max_storage_bytes` applies to a database, memory.md §8) — not PG's
  `temp_file_limit`, which counts only spilled file bytes and so would make the abort point depend on
  out-of-contract residency.
- **`temp_buffers`** — the **resident budget**: past it a host with a spill directory spills the
  domain to a scratch file and bounds its page cache by it (§6). Out-of-contract (residency only — it
  never changes results, cost, or the `54P03` point). `0` means never spill. Default **32 MiB**.

Both are **handle/session settings** in the family of `work_mem` / `max_cost` / `cache_bytes` (api.md
§8): not stored in the file, never change the contents, host-settable on an open handle. Before slice 3
`temp_buffers` *was* the `54P03` ceiling (there was nothing to spill to); slice 3 moved that role to
`max_temp_storage_bytes` and gave `temp_buffers` PostgreSQL's meaning. A host on a no-spill target (an
in-memory, OPFS, or WASI database) holds its whole temp domain in RAM, so its RAM bound is
`max_temp_storage_bytes` — an untrusted-SQL host there sets it as tight as it formerly set
`temp_buffers`.

**Determinism is the load-bearing requirement** (CLAUDE.md §8/§10, §13). For the **session-local**
domain (which rides a `MemoryBlockStore` + pager since the temp-blockstore slice, §6) the ceiling is the
domain's **committed page bytes** — `page_count × page_size` off the temp domain's one pager. This is a
deliberate **re-spec of the basis** from the original on-disk-record-bytes measure, for two reasons.
(a) **Honesty about real RAM:** once temp is paged, its leaves demote to `OnDisk`, so a record-byte walk
sees only the one leaf a write touches and *undercounts* a multi-leaf temp table — the §13 bound would
never fire; the page count charges every allocated page (interior nodes, per-page headers, post-delete
sparsity a B+tree never compacts), which is what memory actually costs. (b) **Simplicity:** one field
per domain, no per-store sum. It stays **in-contract**: `page_count` is a pure function of
`(operations)` via the deterministic B+tree + the deterministic at-commit compaction (whose ~2×-live
trigger is a spec constant, §6), evaluated at commit boundaries — so `54P03` fires at the *same* point
across Rust/Go/TS, independent of allocator/GC/pointer width. Two consequences are recorded: the
measure uses the **logical** `page_count` (not the physical buffer length, which folds in the
geometric-preallocation slack that is explicitly *not* a byte contract), and it reflects the state **one
commit behind** (the check runs before the statement's own commit), so a domain already over budget
aborts the *next* temp write — the "already over budget ⇒ further writes abort" contract. The page basis
is deliberately **not** the `work_mem` spill estimator nor the spill state: spill is out-of-contract
(per-core, §10), whereas `54P03` is **in**-contract. **The check is per-statement:**
after a statement that writes a temp table the domain footprint is measured and the statement aborts
**`54P03 temp_storage_limit_exceeded`** if it *exceeds* `max_temp_storage_bytes` (`0` ⇒ unlimited); the
over-budget write is staged in `temp_working`, so the abort rolls it back (nothing commits). The
**within-statement** bound is `max_cost` — a single huge temp write hits the cost ceiling first — so
the two gates compose to bound temp resources both per-statement and across statements. Whether a temp
table is resident or spilled **never** changes query results, the deterministic cost, or the abort
point — only the ceiling *error* is observable, and it is itself deterministic and part of the
cross-core contract.

This keeps the untrusted-SQL story whole: a host serves untrusted SQL through a session that is
`{SELECT}`-or-narrow-privileged, `max_cost`/`lifetime_max_cost`-capped, `max_sql_length`/depth-bounded
**and now `max_temp_storage_bytes`-bounded** — so a scratch temp table can be offered safely, its
storage provably bounded (and, on a host with a spill target, its RAM bounded by `temp_buffers`).

## 8. Constraints, indexes, sequences on temp tables

Temp tables reuse the full `table_element` machinery; everything is held in the in-memory temp store
(constraints.md, indexes.md), so it costs no file change:

- **`PRIMARY KEY`, `UNIQUE`, `CHECK`, `NOT NULL`, `DEFAULT`** — fully supported, identical semantics
  to persistent tables. A temp table's secondary / unique indexes are in-memory B-trees in the temp
  store, never serialized.
- **Standalone `CREATE INDEX` / `DROP INDEX`** — fully supported on a temp table, identical to a
  persistent table (indexes.md). The index lives in the session-local temp snapshot, so it makes **zero
  file writes** (no `format_version` change) and is dropped with its table at session close; the build
  is metered (`page_read`/`storage_row_read`), the index is maintained on every write, it shares the
  relation namespace (`42P07`), and the planner uses it to bound a scan exactly as for a persistent
  table — the build/lookup funnels (`table` / `store` / `index_store`) route by the resolution walk, and
  only the catalog `put_index`/`remove_index` write is steered to the owning snapshot. The DDL is gated
  by the temp-scoped split: a temp table's index by `allow_temp_ddl` (§5) — a `CREATE INDEX` classified
  by resolving its target table, a `DROP INDEX` by resolving the index. A `gin` index is admitted on the
  same terms as a persistent table (an array column whose element type has a GIN opclass).
- **`serial` / `bigserial` / `smallserial` / `GENERATED … AS IDENTITY`** — fully supported on a temp
  table, identical desugaring and semantics to a persistent column (sequences.md §12/§13). The
  auto-created **owned sequence** is itself a **temp sequence** staged into the *same* temp snapshot, so
  — like the table's rows and indexes — it never touches the file (no `format_version` change). Every
  sequence operation routes by a scope-aware **sequence funnel** (session-local → persistent), so
  `nextval` / `currval` / `setval` by name reach a temp sequence, and `nextval` on it stays the
  transactional snapshot field it is for persistent sequences (sequences.md §5, determinism.md §5). The
  owned temp sequence shares the relation namespace (a collision with its derived `<table>_<col>_seq`
  name is `42P07`), is `2BP01` to `DROP SEQUENCE` directly (the owner-link dependency), and is
  **auto-dropped with its table** (`DROP TABLE` sweeps every sequence owned by it, from the temp
  snapshot). Only the catalog `put_sequence` / `remove_sequence` write is steered to the owning
  snapshot; the build/advance/validation are scope-agnostic. The DDL is gated by the temp-scoped split
  (§5): the `serial`/IDENTITY `CREATE TABLE` by `allow_temp_ddl` (classified statically), and a
  `DROP`/`ALTER SEQUENCE` of a temp owned sequence by the same gate (classified by resolving the
  sequence).
- **Composite-typed columns** — fully supported on a temp table, identical semantics to a persistent
  column (composite.md): `ROW(…)` / `'(…)'::type` construction, `record_out` rendering, field access,
  and the element-wise comparison / ordering all behave exactly as on a persistent table. The key fact
  is that a composite **type** is *always persistent* — `CREATE TYPE` is persistent DDL, so the type
  lives in the main image and a temp table only **references** it (a temp table can never define one).
  The deferral existed only because `put_table` resolves a column's `Type::Composite` reference into its
  `ColType` codec/coercion tree against the **snapshot's own** type catalog, and a temp snapshot's is
  empty; the fix resolves the temp table's `ColType`s against the **main** snapshot's type catalog at
  staging time. The resulting tree is **fully self-contained** (composite.md §4), so the temp store
  needs nothing from the catalog per row — no temp snapshot carries a type catalog, and the table still
  makes **zero file writes**. A composite column is **storable but never keyable**, so it cannot be a
  `PRIMARY KEY` (`0A000`, the same scope-agnostic key gate as a persistent table, §6). `DROP TYPE` of a
  type a temp table references is `2BP01`: the dependency check is **scope-aware** — it scans the main
  image *and* the visible session-local temp snapshot, so a temp table's reference blocks the drop
  exactly as a persistent column's does (another session's private session-local reference is invisible
  by design — and its self-contained `ColType` keeps working regardless). The DDL is gated by the
  temp-scoped split (§5) like any temp `CREATE TABLE`.
- **`FOREIGN KEY` among temp tables** — fully supported, identical semantics to persistent tables
  (constraints.md §6): a temp table may reference another temp table of its session, or itself, via
  `CREATE TEMP TABLE` or `ALTER TABLE … ADD [COLUMN … REFERENCES | FOREIGN KEY]`. MATCH SIMPLE/FULL,
  the child-side `23503` at every write site, NO ACTION/RESTRICT, the CASCADE/SET NULL/SET DEFAULT
  actions (recursive, metered exactly as in the main database), ALTER TABLE validation, and `DROP
  TABLE`'s `2BP01`/`CASCADE` all reuse the main-database machinery. An FK never crosses databases, so
  its child and parent live in the same **domain**: every FK lookup — the child-side parent probe, the
  parent-side reverse match, the inbound-referencer scan, a generated action's keyed child scan —
  reads that domain's snapshot explicitly (never the temp-first name walk), and a generated action
  names its child with the domain's qualifier (`temp.`/`main.`). The DML target's domain is its
  explicit `temp`/`main` qualifier or, for a bare name, the resolution walk. `DROP TABLE` scans each
  domain's drop set for FK dependents in that domain's snapshot (the main database first). Like every
  temp catalog fact, the FK is never serialized (no `format_version` change).
  - **An FK never crosses between the temp domain and the main database: `42P16`** (PostgreSQL's
    `ATAddForeignKeyConstraint`). The parent name takes the temp-first resolution walk; a temp table
    whose parent resolves to a persistent table is *"constraints on temporary tables may reference
    only temporary tables"*, and a persistent table whose parent resolves to a temp table is
    *"constraints on permanent tables may reference only permanent tables"* — at `CREATE TABLE` and
    `ALTER TABLE` alike, checked as soon as the parent resolves. A missing parent is still `42P01`.
    PostgreSQL's third case (a temp table of *another* session) cannot arise: another session's temp
    tables are invisible. An FK on an attached-database table stays `0A000`
    ([attached-databases.md](attached-databases.md) §8).
  - **`ON COMMIT`** interacts through PostgreSQL's truncation rule — §5.1.

## 9. Cost

Temp-table reads and writes accrue the **existing** cost units (cost.md, spec/cost/schedule.toml) —
`page_read` per B-tree node touched (counted **logically**, even for a resident in-memory tree, so a
resident-vs-spilled temp table costs the same — the §9 logical-cost rule), `storage_row_read`,
`row_produced`, `operator_eval`, `sequence_advance`. **No new cost unit.** The storage budget (§7) is
a *separate* gate from the cost meter; a temp-table query is bounded by **both** (work *and* retained
bytes), the two firing independently, mirroring the depth-vs-cost independence in §13.

## 10. Determinism & the cross-core contract

- **In-contract (must be byte-identical across cores):** every observable of a temp-table query —
  rows, multiset, ordering under `ORDER BY`, types, names, errors, and **cost** — and the **`54P03`
  abort point** given a fixed budget. These hold by construction (§2: reused codec/comparator;
  §7: deterministic logical-byte budget).
- **Out-of-contract (deliberately per-core, like spill.rs run files):** the on-disk **temp scratch
  file** (§6) and *when* a domain spills. The file is internal scratch, never the §8 file format, never
  read by another core or host, unlinked at creation or deleted on close — so it carries no
  byte-identity obligation. This is the same exemption external-merge-sort runs already enjoy.

## 11. Errors

| Code | When |
|---|---|
| `42P07` *(reuse)* | `CREATE [TEMP] TABLE` whose name collides with any relation in the creating scope (§3). |
| `42P01` *(reuse)* | `DROP`/reference of a temp name that resolves to nothing (§3). |
| `42501` *(reuse)* | temp DDL on a session without `allow_ddl` (§5). |
| `0A000` *(reuse)* | deferred temp surface (§8); a commit whose `ON COMMIT DELETE ROWS` set is referenced by a foreign key from outside it (§5.1). |
| `42P16` *(reuse)* | an `ON COMMIT` clause on a table that is not temporary (§1, §5.1); a foreign key between a temp and a persistent table (§8). |
| **`54P03`** *(new)* | `temp_storage_limit_exceeded` — the temp domain's page bytes (resident or spilled) exceeded `max_temp_storage_bytes` (§7). |
| `58030` *(reuse)* | a scratch-file I/O failure while spilling or writing a spilled temp domain (§6); the transaction commits nothing. |

**`54P03`** joins the §13 untrusted-query safety gates in class **54** (`program limit exceeded`),
the sibling of `54P01` (per-statement `max_cost`) and `54P02` (session `lifetime_max_cost`). It is a
`P`-subclass code, the established jed pattern for a deterministic, configured ceiling that PostgreSQL
has no equivalent for (PG relies on nondeterministic OS OOM / disk-full — divergence D4). It is
**not** put in PG's class 53 (`insufficient_resources`) precisely because that class denotes *actual*
resource exhaustion (platform-dependent, nondeterministic), whereas `54P03` is a deterministic
host-configured limit — exactly why `54P01`/`54P02` are class-54 jed codes rather than reused PG codes.
The code lands in [../errors/registry.toml](../errors/registry.toml) with slice 1.

## 12. Recorded divergences from PostgreSQL

- **D1 — zero file writes.** PG temp tables still cause catalog writes and consume txids; jed's make
  *no* file I/O. The core requirement, satisfied structurally (§2).
- **D2 — preclude overlaps, no shadowing.** PG shadows a permanent table via an implicit `pg_temp`
  first in `search_path`; jed rejects the overlap at `CREATE` (`42P07`), with a deterministic
  session-local-wins tie-break for the one unpreventable cross-session race (§3).
- **D4 — deterministic storage ceiling + `54P03`.** PG bounds temp storage only via OS limits and
  `temp_file_limit` over spilled file bytes (residency-dependent); jed adds a deterministic,
  host-configured ceiling over all temp pages, resident or spilled (§7). `temp_buffers` keeps PG's
  meaning (resident temp memory).

*(Removed divergences: **D6** — `ON COMMIT PRESERVE ROWS` only; `DELETE ROWS` and `DROP` now follow
PostgreSQL (§5.1). **D3** — a `SHARED` kind that shared *data* across sessions, unlike PG's
per-session `GLOBAL TEMPORARY` — and **D5** — that shared kind being ephemeral / never recovered. Both
described the database-wide shared temp table, removed in Slice 0 in favor of a `Database`-scoped
in-memory attachment, [attached-databases.md](attached-databases.md) §6. The `D3`/`D5`/`D6` numbers
are retired, not reused.)*

## 13. Slicing (the build plan — TODO.md tracks it)

The doc covers all of it; the **build is phased** (CLAUDE.md §10 vertical slices), session-local-first
and memory-only per the design decision:

- **Slice 1 — session-local temp tables, memory-only (landed).** `CREATE [TEMP|TEMPORARY] TABLE` + `DROP`, the
  per-`Session` temp store, the namespace preclude-overlaps check (§3), the **`allow_temp_ddl`**
  capability split (§5), the storage budget + `54P03` (then named `temp_buffers`), constraints/indexes/`serial` (§8), cost (§9),
  dropped at session close. The grammar production (`table_scope` minus `SHARED`), the `54P03`
  registry entry, `allow_temp_ddl`, and `temp_buffers` land here. Driven by a new `ddl.temp_table`
  conformance capability; per-core unit tests for what the corpus can't reach (the §13 budget abort
  point, the no-file-write invariant, the namespace internals, the capability split).
- **Slice 2 — shared temp tables (landed, then REMOVED in Slice 0).** A database-wide `SHARED` kind:
  the `SHARED` keyword, `allow_shared_temp_ddl`, a `Database`-level temp snapshot, a two-root commit,
  `shared_temp_mem`, and cross-session visibility tests over the concurrency schedule format. It shipped
  briefly and was then retired (see Slice 0 below); recorded here for history.
- **Temp-blockstore slice — session-local temp onto a MemoryBlockStore (landed).** The per-domain
  in-RAM `MemoryBlockStore` + pinned pager (§6), within-session free-list compaction
  (`reclaim_within_session` / `maybe_compact`, watermark-gated), the residency flip on temp leaves, and
  the **page-based** `54P03` budget (§7). All three cores; result/cost/byte-neutral (temp is never
  serialized — no `format_version` bump). Per-core white-box tests (compaction bound, reader-defers
  gate, compact footprint, zero-file-writes) for what the corpus can't reach.
- **Slice 0 — retire the `SHARED` temp surface (landed, 2026-07-03).** The subtractive removal of the
  former slice-2 kind (grammar, corpus, manifest caps, all three cores): the `SHARED` keyword,
  `allow_shared_temp_ddl`, `shared_temp_mem`, the `Database`-level temp snapshot, and the two-root
  commit are gone; the commit publishes a single durable file root again. The capability is re-provided,
  if needed, by a `Database`-scoped in-memory **attachment** ([attached-databases.md](attached-databases.md)
  §6, of which this is "Slice 0"). No `format_version` change (shared temp was never serialized).
- **Slice 3 — spill-to-disk (landed).** With temp already paged (the temp-blockstore slice), spill is
  the temp `BlockStore` swap (`migrate_store` into an unlinked scratch file in the host spill
  directory) + a bounded temp pool (§6), and the budget split: `temp_buffers` becomes the resident
  budget, the new `max_temp_storage_bytes` the in-contract `54P03` ceiling (§7; the corpus directive
  `# temp_buffers:` became `# max_temp_storage_bytes:`). Temp commits now stage before the main persist
  and adopt after it. Out-of-contract scratch file (§10); per-core white-box tests pin the
  results/cost/`54P03` neutrality of spilling (the corpus runs in-memory databases, which have no spill
  target). No `format_version` change.

- **`ON COMMIT` slice (landed).** `ON COMMIT { PRESERVE ROWS | DELETE ROWS | DROP }` (§5.1): the
  grammar, `42P16` on a non-temporary table, and the commit-time truncate/drop applied to the working
  temp snapshot before it stages. All three cores; no `format_version` change (temp catalog entries
  are never serialized). Driven by a new `ddl.temp_on_commit` conformance capability.

- **`CREATE TABLE … AS` (landed).** The query form of `CREATE TABLE`, temp and persistent alike
  ([create-table-as.md](create-table-as.md)): a temp target follows this doc's gate, namespace, `ON
  COMMIT`, and narrowings. All three cores; capability `ddl.create_table_as`.

## 14. Deferred / follow-ons (none foreclosed)

- **Database-wide shared temp — removed, not a follow-on.** The former `SHARED` kind (slice 2) is
  retired (Slice 0, §13); a `Database`-scoped in-memory **attachment** is its replacement
  ([attached-databases.md](attached-databases.md) §6). Not deferred — deliberately not part of temp.
- A host-configurable spill directory (shared with the external sort — spill.md), so a WASI or OPFS
  host could offer a temp spill target; and migrating a shrunken spilled domain back into RAM.
- ~~Temporary views~~ — **landed** ([views.md](views.md) §10): `CREATE TEMP VIEW`, `CREATE VIEW
  temp.v`, and a view over a temporary relation live in the session's temporary domain, in its
  preclude-overlaps namespace, gated by `allow_temp_ddl` when spelled temporary.
