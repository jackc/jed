# Temporary tables — design

> **Session-local** and **database-wide shared** temporary tables that make **zero writes to
> the database file** — they live entirely *outside* the serialized snapshot, so they cause no
> catalog write, no page write, and no txid churn on the main file. Memory-only to start, with a
> designed-in (deferred) spill-to-disk seam; both kinds are bounded by a **deterministic storage
> budget** so they preserve the untrusted-SQL safety guarantee (CLAUDE.md §13). The **grammar is
> authoritative** for the surface ([../grammar/grammar.ebnf](../grammar/grammar.ebnf) —
> `create_table`, `table_scope`); the **error registry**
> ([../errors/registry.toml](../errors/registry.toml)) owns the codes (one new code — `54P03`);
> this doc is the *why* and the precise behavior the three cores reproduce identically
> (CLAUDE.md §2, §8). When a decision here changes, change the grammar and this doc in the same edit.
>
> **Status: slices 1–2 landed (all three cores); session-local temp reshaped onto a MemoryBlockStore.**
> Slice 1 (session-local temp tables) and slice 2 (database-wide `SHARED` temp tables — the
> `Database`-level temp snapshot, the two-root commit, `allow_shared_temp_ddl`, `shared_temp_mem`, and
> cross-session visibility via the concurrency schedule format) are implemented byte-identically in
> Rust, Go, and TS. The grammar production (`table_scope`, now including `SHARED`), the `54P03` code,
> and the budget settings landed with their slices. The **temp-blockstore slice** then retired the
> original "fully-resident decoded" storage model for **session-local** temp: it now rides a per-domain
> in-RAM `MemoryBlockStore` + pager with within-session free-list compaction, and its `54P03` budget is
> the domain's committed **page** bytes (§6/§7) — all three cores. **Shared** temp is still
> fully-resident decoded (its flip is a follow-on, §14). **Slice 3 (spill-to-disk, §6) remains
> deferred** — the flip put temp on its seam, so spill is now a `BlockStore` swap. This doc was written
> first, spec-first (CLAUDE.md §11), and tracks the implemented behavior.

Temporary tables are jed's first relations whose lifetime is shorter than the database file's and
whose data is **deliberately never durable**. They diverge from PostgreSQL in six recorded ways
(§12); each is a conscious tradeoff, not an accident, taken against the §1 "match PG unless an
overriding reason" rule.

## 1. Surface

```sql
CREATE [ SHARED ] [ TEMP | TEMPORARY ] TABLE name ( table_element [, ...] )
```

```ebnf
create_table   ::= "CREATE" table_scope? "TABLE" identifier
                   "(" table_element ("," table_element)* ")"
table_scope    ::= "SHARED"? ("TEMPORARY" | "TEMP")
```

- `TEMP`/`TEMPORARY` are synonyms (PG); `SHARED` is jed-specific (§4). All three are **not
  reserved** (grammar.md §3), recognized **positionally** between `CREATE` and `TABLE`. A table may
  still be *named* `temp`/`shared`/`temporary` — the word after `TABLE` is always the table name, so
  `CREATE TABLE shared (...)` is an ordinary persistent table named `shared`.
- `SHARED` must be immediately followed by `TEMP`/`TEMPORARY` (a `SHARED` table is always temporary —
  there is no durable "shared" table); a stray `CREATE SHARED TABLE …` is `42601`.
- Three table kinds result, all sharing the existing `table_element` grammar (columns, `PRIMARY
  KEY`, `CHECK`, `UNIQUE`, `FOREIGN KEY` — with the FK narrowing of §8):
  - **persistent** — `CREATE TABLE` (unchanged; the only kind that touches the file).
  - **session-local temp** — `CREATE [TEMP|TEMPORARY] TABLE` — private to the creating session.
  - **database-wide shared temp** — `CREATE SHARED [TEMP|TEMPORARY] TABLE` — visible to every
    session of the same open `Database`.
- `DROP TABLE name` drops a temp table exactly as it drops a persistent one (it resolves through
  the same namespace, §3). No grammar change for `DROP`.
- **Deferred surface** (each simply absent from the grammar this slice, none foreclosed — §14):
  `ON COMMIT { PRESERVE ROWS | DELETE ROWS | DROP }` (default is **`PRESERVE ROWS`**, the only
  behavior implemented — `DELETE ROWS`/`DROP` are `0A000`); `IF NOT EXISTS` (consistent with
  persistent `CREATE TABLE`, which has none — a collision is `42P07`); and `CREATE TEMP TABLE … AS
  SELECT` (CTAS does not exist yet for any table kind).

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

Two never-serialized stores, by kind:

- **Session-local temp catalog** — a per-`Session` structure (`temp_tables` + `temp_stores` +
  `temp_index_stores` + `temp_sequences`), alongside the session's existing per-session state
  ([session.md §2](session.md): `vars`, `session_seq`, privileges). Born empty with the session,
  dropped wholesale at session close.
- **Shared temp catalog** — a second *committed* in-memory structure on the `Database` core
  (a "temp snapshot"), published by the same commit discipline as the file snapshot but
  **never fsynced and never serialized** (§5). Born empty when the `Database` is opened, gone when
  it is closed.

Because temp stores reuse the existing `TableStore` / B-tree / value codec / comparator verbatim,
their *behavior* (rows, ordering, comparisons, errors, cost) is cross-core byte-identical by
construction — the §5/§8 "derived from already-identical pieces" argument (CLAUDE.md §5,
extensibility.md §4.1). The only thing that is *not* in the cross-core file contract is an eventual
spill file (§6, §11).

## 3. Namespace — overlaps are precluded (a PG divergence)

jed has one flat relation namespace (tables/indexes/sequences compete — indexes.md §2); there is no
schema and no `search_path`. Temp tables join it under a **preclude-overlaps** rule (divergence D2;
PostgreSQL instead *shadows* a permanent table with a like-named `pg_temp` one). Concretely:

- **The shared-temp + persistent namespaces are one global space.** Any collision among them — a
  persistent table named like a shared temp, an index/sequence named like either — is `42P07`
  (`duplicate_table` / "relation already exists"), checked at `CREATE`, exactly as two persistent
  tables collide today. This half is globally enforceable because both are global, gated state (§5).
- **A session-local `CREATE [TEMP] TABLE` is checked against the creating session's *entire visible
  scope*** — its own session-local temps ∪ the shared temps ∪ the persistent relations — and a
  collision is `42P07`. So *within one session* you can never create two relations of the same name;
  there is no "which `t`?" ambiguity to reason about.

**Resolution** walks **session-local → shared → persistent** at the single chokepoint
(`Snapshot::table()` / the executor's name resolver). Given the creation-time checks above, **at
most one of the three ever matches**, so the order is normally just *where the row lives*, not a
precedence contest. A missing name is `42P01` as today.

**The one unpreventable cross-session race, and why the lookup order still has teeth.** Session-local
temps are *private* — session B cannot see session A's session-local `t`, so B's `CREATE SHARED TEMP
TABLE t` (or a persistent `CREATE TABLE t`) cannot be rejected on account of A's private `t`. If that
global `t` is created *after* A already holds a private `t`, A now sees two. We resolve this
deterministically with **session-local-wins** (the first rung of the walk): A keeps resolving `t` to
its own table — the least-surprising outcome for A (it sees what it made), and no other session is
affected. This residual is the *only* case the lookup order ever decides; in all reachable-by-one-
session states, creation already precluded the overlap. (We deliberately do **not** maintain a global
registry of every session's private temp names to slam this window shut: it would force cross-session
coordination on a private, gate-free structure — the opposite of what makes session-local temps cheap,
§6.)

## 4. The two kinds, and what "shared" means

PostgreSQL's `GLOBAL TEMPORARY` shares a table *definition* across sessions but gives each session its
own *data*; that is **not** what `SHARED` means here, which is why jed coins a new keyword rather than
reusing `GLOBAL`. jed's **shared temp table shares the data too** (divergence D3): one set of rows,
visible to and writable by every session of the open `Database`, but still never written to the file.

| | session-local temp | shared temp | persistent |
|---|---|---|---|
| Visibility | creating session only | all sessions of the `Database` | all sessions |
| Lives in | per-`Session` store (§2) | `Database`-level temp snapshot (§2) | serialized `Snapshot` |
| Writes touch the file? | **never** | **never** | yes (commit) |
| Single-writer gate on write? | **no** (private) | **yes** (§5) | yes |
| Transactional? | yes (session txn) | yes (commit boundary, §5) | yes |
| Dropped at | session close (or `DROP`) | `Database` close (or `DROP`) | `DROP` only |
| Survives database reopen? | n/a | **no** (ephemeral, never recovered — D5) | yes |

## 5. Concurrency, transactions, and the commit boundary

This is where the two kinds genuinely differ, and where the single-writer model (CLAUDE.md §3) is
respected without ever fsyncing temp data.

**Session-local temp writes take no global writer gate.** The data is private, so there is nothing to
serialize against other sessions. A session-local `INSERT`/`UPDATE`/`DELETE` mutates the session's
own temp store directly. A direct, useful consequence: **even a read-only session can use a
session-local temp table as scratch space** — it pins an immutable file snapshot for *persistent*
reads while freely writing its *private* temp store, with no contradiction. This matches PostgreSQL,
which explicitly permits temp-table writes inside a read-only transaction, and it is exactly the
property a host wants when handing an untrusted, `{SELECT}`-only session a scratch table.

**Shared temp writes ride the single-writer gate and the commit boundary** (the user-chosen,
snapshot-consistent model). A shared-temp mutation is part of the same write transaction as any
persistent mutation: it acquires the single-writer gate (transactions.md §10, shared.rs), accumulates
in the working set, and is published atomically at commit. The commit publishes **two roots** — the
file snapshot root (fsynced per `synchronous`, §9 of transactions.md) **and** the shared-temp snapshot
root (a pure in-memory pointer swap, **no fsync, nothing written to the file**). A reader pins **both**
roots together, so a reader's view is consistent across persistent and shared-temp tables alike, and
other sessions see shared-temp writes **only after commit**; `ROLLBACK` discards them. A transaction
that touches *only* shared temp tables still takes the gate (to serialize the shared-temp root swap)
and "commits" with no fsync — cheap, and uniform with the existing path.

**Mixing in one transaction** is coherent: a transaction may write persistent + shared-temp +
session-local tables at once. It takes the gate (because it writes persistent and/or shared-temp);
persistent + shared-temp changes go in the working set, session-local changes in the session's private
staging; commit fsyncs only the persistent dirty pages, swaps both committed roots, and folds the
session-local staging into the session's durable-for-session state. Rollback discards all three.
Read-your-own-writes within the transaction works for all three exactly as it does for persistent
tables today.

**DDL gating — three capabilities, split by table kind.** `CREATE`/`DROP` is DDL (`42501` if
denied — session.md §5.3), but temp tables are precisely the safe, bounded scratch space a host
*wants* to expose to an untrusted session that may otherwise touch nothing. So the single
`allow_ddl` gate splits, by the kind of relation the statement creates/drops, into three session
capabilities:

- **`allow_ddl`** *(existing, default on)* — now scoped to **persistent** DDL specifically
  (`CREATE`/`DROP`/`ALTER` of persistent tables, indexes, types, sequences). Its meaning narrows but
  its name and default are unchanged, so existing callers are unaffected.
- **`allow_temp_ddl`** *(new)* — `CREATE`/`DROP` of **session-local** temp tables.
- **`allow_shared_temp_ddl`** *(new)* — `CREATE`/`DROP` of **shared** temp tables (a global-state
  mutation that also charges the *global* budget, §7, so it is the more privileged of the two).

**Back-compat by default-inheritance:** both new gates **default to `allow_ddl`'s value** when not
set explicitly. So a session left as-is behaves exactly as today (one `allow_ddl` governs
everything), while the untrusted-scratch pattern is `allow_ddl = off` (no persistent/shared DDL) **+
explicit `allow_temp_ddl = on`** — private scratch tables only, everything else denied. This keeps
the §5.3 default-deny posture intact (an untrusted `{SELECT}`-only session has `allow_ddl = off`, so
temp/shared DDL are off too until the host deliberately opts in) and relaxes the documented "one
boolean" `allow_ddl` narrowing (session.md §5.3) only for temp tables. The gates are configured per
record by the directives `# allow_ddl:` / `# allow_temp_ddl:` / `# allow_shared_temp_ddl:` (§13),
and land with their slices (`allow_temp_ddl` in slice 1, `allow_shared_temp_ddl` in slice 2).

## 6. Storage model — a per-domain in-RAM MemoryBlockStore, spill-to-disk as a designed-in seam

**Session-local temp rides a per-domain in-RAM `MemoryBlockStore` + pager** — the *same* storage path
as an in-memory database (bplus-reshape.md B3), not a separate fully-resident decoded B-tree. The
temp-blockstore slice retired the original slice-1 "`paging: None`, `Child::Resident`" mode: the
session-local temp domain is now a private `MemoryBlockStore` seeded with the empty from-scratch image,
read/written through the *same* pager + packed-leaf path (`newTempStorage` / `Storage::new_temp`), with
a **pinned, unbounded pool** (a temp domain is resident by definition, §5). A temp `TableStore`
`attach_paging`s that domain's `SharedPaging`, so its leaves demote to `OnDisk` after each commit and
fault back through the temp pool — the **compact packed footprint** (resident memory ≈ `page_count ×
page_size`, not the inflated `Value` tree), which is what makes the §7 byte budget honest. A temp
commit runs `persist_temp`: the same incremental copy-on-write serialize as a file/in-memory commit,
but **no meta slot and no `sync`** (a temp store is never reopened; its memory host has no durability
barrier), so it makes **zero main-file writes** (D1) by construction. **Shared** temp is still the
fully-resident decoded path (a follow-on — its cross-session compaction watermark needs the storage
core-owned; see §14).

**Within-session compaction** is the prerequisite the flip needed. A `MemoryBlockStore` commits
copy-on-write, so every commit orphans its root→leaf path + the rewritten catalog; page reclamation
was previously *reconstruct-on-open only*, and a temp store is **never reopened** — so without
compaction it would leak a page per commit, breaking the `temp_buffers` budget the §13 untrusted-SQL
guarantee rests on. A reclaim domain (`reclaim_within_session`, set only by temp domains) instead
rebuilds its free-list from the **live reachable set** at commit (`maybe_compact`, reusing the same
reachability walk the open-time free-list reconstruction runs — the catalog chain + the in-memory tree
node pages + spillable-leaf overflow). It is **periodic** (walks only once the high-water passes ~2×
the live count at the last compaction, so `page_count` oscillates in `[live, 2×live]` and the walk is
amortized O(height)/commit) and **watermark-gated** (deferred while any older version is pinned — a
read session or an open streaming cursor over the domain — so a page a live reader may still fault is
never freed). This reclamation carries **no cross-core byte contract**: a temp store is never
serialized (D1), so its physical page layout and reclamation are per-core (only the *logical*
observables — rows, cost, and the `54P03` abort point — stay cross-core-identical), which is what makes
the mechanism tractable versus the still-deferred general within-file reclamation. The main/file domain
keeps reconstruct-on-open only (`reclaim_within_session` false) and can opt in later.

**Spill-to-disk is the deferred follow-on, and the flip already put temp on the right seam.** Temp is
now paged against a `MemoryBlockStore` through the pager, so spill is no longer a "flip from resident to
paged" reshape — it is a **`BlockStore` swap plus a bounded pool**: replace the in-RAM
`MemoryBlockStore` with a host-supplied temp-file `BlockStore` (storage.md §2, hosts.md §2), entirely
separate from the main file's `BlockStore` (so the zero-main-file-writes invariant is preserved by
construction), and give the temp pool an eviction bound instead of the pinned/unbounded one it runs
today. The temp `BlockStore` is *not* the
external-merge-sort spill path (spill.rs), which writes sequential run files for `ORDER BY`: a temp
table needs random access by key (point lookups, index scans, ongoing mutation), so it reuses the
**pager + buffer pool + B-tree** machinery (the right tool), while borrowing spill.rs's *idea* of a
host temp directory and a per-core internal codec. Two budgets then apply: a **memory** ceiling
(resident bytes, after which it spills) and a **disk** ceiling (temp-file bytes, after which it
errors `54P03`). Both deterministic (§7), both preserving §13.

## 7. Bounds — the deterministic storage budget (the §13 gate temp tables need)

Temp tables introduce a genuinely new hazard that the existing gates do not cover: **retained**
storage across statements. The cost meter (cost.md) bounds *work per statement*; the depth limit
bounds *parse nesting*; `lifetime_max_cost` bounds *cumulative work*. None of them bounds **bytes
held between cheap statements** — an adversary on a generously-budgeted session could
`CREATE TEMP TABLE t(x text); INSERT …; INSERT …;` and accrue unbounded RAM with little per-statement
cost. So temp tables add a **dedicated storage budget**, a new §13 resource gate independent of the
cost meter (the same way the depth gate is independent of the cost gate):

- **`temp_buffers`** — a per-session byte budget for session-local temp storage. A **handle/session
  setting** in the family of `work_mem` / `max_cost` / `cache_bytes` (api.md §8): not stored in the
  file, never changes the contents, host-settable on an open handle. `0` means unlimited (a trusted
  host). Default modest (proposed 32 MiB; final tuning is a slice-1 detail).
- **`shared_temp_mem`** — a **global** byte budget for all shared temp storage, a `Database`-level
  setting (shared temp data is global, so its budget must be too), charged across
  sessions. `0` = unlimited. Default modest.

**Determinism is the load-bearing requirement** (CLAUDE.md §8/§10, §13). For the **session-local**
domain (which rides a `MemoryBlockStore` + pager since the temp-blockstore slice, §6) the budget is the
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
aborts the *next* temp write — the "already over budget ⇒ further writes abort" contract. The
**shared** temp domain is still fully-resident decoded, so its budget stays the record-byte sum until
the shared flip lands (§14). Either basis is deliberately **not** the `work_mem` spill estimator: spill
is out-of-contract (per-core, §10), whereas `54P03` is **in**-contract. **The check is per-statement:**
after a statement that writes a temp table the domain footprint is measured and the statement aborts
**`54P03 temp_storage_limit_exceeded`** if it *exceeds* `temp_buffers` (`0` ⇒ unlimited); the
over-budget write is staged in `temp_working`, so the abort rolls it back (nothing commits). The
**within-statement** bound is `max_cost` — a single huge temp write hits the cost ceiling first — so
the two gates compose to bound temp resources both per-statement and across statements. Whether a temp
table is resident or (later) spilled must **never** change query results or the deterministic cost —
only the budget *error* (and, later, spill *timing*) is observable, and the error is itself
deterministic and part of the cross-core contract.

This keeps the untrusted-SQL story whole: a host serves untrusted SQL through a session that is
`{SELECT}`-or-narrow-privileged, `max_cost`/`lifetime_max_cost`-capped, `max_sql_length`/depth-bounded
**and now `temp_buffers`-bounded** — so a scratch temp table can be offered safely, its memory (and
later disk) provably bounded.

## 8. Constraints, indexes, sequences on temp tables

Temp tables reuse the full `table_element` machinery; everything is held in the in-memory temp store
(constraints.md, indexes.md), so it costs no file change:

- **`PRIMARY KEY`, `UNIQUE`, `CHECK`, `NOT NULL`, `DEFAULT`** — fully supported, identical semantics
  to persistent tables. A temp table's secondary / unique indexes are in-memory B-trees in the temp
  store, never serialized.
- **Standalone `CREATE INDEX` / `DROP INDEX`** — fully supported on a temp table (session-local and
  `SHARED`), identical to a persistent table (indexes.md). The index lives in the matching in-memory
  temp snapshot, so it makes **zero file writes** (no `format_version` change) and is dropped with its
  table at session/database close; the build is metered (`page_read`/`storage_row_read`), the index is
  maintained on every write, it shares the relation namespace (`42P07`), and the planner uses it to
  bound a scan exactly as for a persistent table — the build/lookup funnels (`table` / `store` /
  `index_store`) route by the resolution walk, and only the catalog `put_index`/`remove_index` write is
  steered to the owning snapshot. The DDL is gated by the temp-scoped split: a session-local temp
  table's index by `allow_temp_ddl`, a `SHARED` temp table's by `allow_shared_temp_ddl` (§5) — a
  `CREATE INDEX` classified by resolving its target table, a `DROP INDEX` by resolving the index. A
  `SHARED` temp index (and the constraint it backs) is visible to and enforced for every session after
  its creating transaction commits (the two-root commit, §5). A `gin` index is admitted on the same
  terms as a persistent table (an array column whose element type has a GIN opclass).
- **`serial` / `bigserial` / `smallserial` / `GENERATED … AS IDENTITY`** — fully supported on a temp
  table (session-local and `SHARED`), identical desugaring and semantics to a persistent column
  (sequences.md §12/§13). The auto-created **owned sequence** is itself a **temp sequence** staged into
  the *same* temp snapshot (a session-local sequence for a session-local table; a shared-temp sequence
  for a `SHARED` table), so — like the table's rows and indexes — it never touches the file (no
  `format_version` change). Every sequence operation routes by a scope-aware **sequence funnel**
  (session-local → shared → persistent), so `nextval` / `currval` / `setval` by name reach a temp
  sequence, and `nextval` on it stays the transactional snapshot field it is for persistent sequences
  (sequences.md §5, determinism.md §5). The owned temp sequence shares the relation namespace (a
  collision with its derived `<table>_<col>_seq` name is `42P07`), is `2BP01` to `DROP SEQUENCE`
  directly (the owner-link dependency), and is **auto-dropped with its table** (`DROP TABLE` sweeps
  every sequence owned by it, from the owning temp snapshot). Only the catalog `put_sequence` /
  `remove_sequence` write is steered to the owning snapshot; the build/advance/validation are
  scope-agnostic. The DDL is gated by the temp-scoped split (§5): the `serial`/IDENTITY `CREATE TABLE`
  by `allow_temp_ddl` / `allow_shared_temp_ddl` (classified statically), and a `DROP`/`ALTER SEQUENCE`
  of a temp owned sequence by the same gate (classified by resolving the sequence). On a `SHARED` temp
  table the owned sequence is cross-session: a session's advance is visible to every other session
  after its transaction commits (the two-root commit, §5), so auto-numbering continues across sessions.
- **Composite-typed columns** — fully supported on a temp table (session-local and `SHARED`), identical
  semantics to a persistent column (composite.md): `ROW(…)` / `'(…)'::type` construction, `record_out`
  rendering, field access, and the element-wise comparison / ordering all behave exactly as on a
  persistent table. The key fact is that a composite **type** is *always persistent* — `CREATE TYPE` is
  persistent DDL, so the type lives in the main image and a temp table only **references** it (a temp
  table can never define one). The deferral existed only because `put_table` resolves a column's
  `Type::Composite` reference into its `ColType` codec/coercion tree against the **snapshot's own** type
  catalog, and a temp snapshot's is empty; the fix resolves the temp table's `ColType`s against the
  **main** snapshot's type catalog at staging time. The resulting tree is **fully self-contained**
  (composite.md §4), so the temp store needs nothing from the catalog per row — no temp snapshot carries
  a type catalog, and the table still makes **zero file writes**. A composite column is **storable but
  never keyable**, so it cannot be a `PRIMARY KEY` (`0A000`, the same scope-agnostic key gate as a
  persistent table, §6). `DROP TYPE` of a type a temp table references is `2BP01`: the dependency check
  is **scope-aware** — it scans the main image *and* the visible session-local + shared temp snapshots,
  so a temp table's reference blocks the drop exactly as a persistent column's does (a `SHARED` temp
  table's reference blocks it for every session; another session's private session-local reference is
  invisible by design — and its self-contained `ColType` keeps working regardless). The DDL is gated by
  the temp-scoped split (§5) like any temp `CREATE TABLE`.
- **`FOREIGN KEY` involving a temp table — `0A000` this slice (deferred).** A reference *between* a
  temp and a persistent table is semantically fraught (a permanent table outliving the temp it points
  at) and PG itself restricts it; jed defers all FK constraints touching a temp table in either
  direction. FKs *among* temp tables of the same kind are a clean follow-on (§14).

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
- **Out-of-contract (deliberately per-core, like spill.rs run files):** the eventual on-disk **temp
  spill file** (§6). It is internal scratch, never the §8 file format, never read by another core or
  host, deleted on close — so it carries no byte-identity obligation. This is the same exemption
  external-merge-sort runs already enjoy.

## 11. Errors

| Code | When |
|---|---|
| `42P07` *(reuse)* | `CREATE [SHARED] [TEMP] TABLE` whose name collides with any relation in the creating scope (§3). |
| `42P01` *(reuse)* | `DROP`/reference of a temp name that resolves to nothing (§3). |
| `42501` *(reuse)* | temp DDL on a session without `allow_ddl` (§5). |
| `0A000` *(reuse)* | deferred temp surface — `ON COMMIT DELETE ROWS`/`DROP`, FK touching a temp table (§1, §8). |
| **`54P03`** *(new)* | `temp_storage_limit_exceeded` — a temp store exceeded `temp_buffers` (session-local) or `shared_temp_mem` (shared); later, the temp-spill disk ceiling (§7). |

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
- **D3 — `SHARED` means shared *data*.** PG's `GLOBAL TEMPORARY` shares only the *definition*
  (per-session data); jed's `SHARED` temp tables share the rows across sessions (§4).
- **D4 — deterministic storage budget + `54P03`.** PG bounds temp storage only via OS limits
  (nondeterministic); jed adds a deterministic, host-configured byte budget (§7).
- **D5 — ephemeral, never recovered.** Shared temp tables vanish at `Database` close and are never
  written or recovered; PG temp tables are session-scoped (jed has no per-session-data shared kind to
  compare, but the never-durable property is the divergence).
- **D6 — `ON COMMIT PRESERVE ROWS` only (this slice).** `DELETE ROWS` / `DROP` are deferred `0A000`
  (§1). Largely moot until cross-statement transactions are common (autocommit makes `ON COMMIT`
  semantics rarely observable).

## 13. Slicing (the build plan — TODO.md tracks it)

The doc covers all of it; the **build is phased** (CLAUDE.md §10 vertical slices), session-local-first
and memory-only per the design decision:

- **Slice 1 — session-local temp tables, memory-only (landed).** `CREATE [TEMP|TEMPORARY] TABLE` + `DROP`, the
  per-`Session` temp store, the namespace preclude-overlaps check (§3), the **`allow_temp_ddl`**
  capability split (§5), `temp_buffers` + `54P03`, constraints/indexes/`serial` (§8), cost (§9),
  dropped at session close. The grammar production (`table_scope` minus `SHARED`), the `54P03`
  registry entry, `allow_temp_ddl`, and `temp_buffers` land here. Driven by a new `ddl.temp_table`
  conformance capability; per-core unit tests for what the corpus can't reach (the §13 budget abort
  point, the no-file-write invariant, the namespace internals, the capability split).
- **Slice 2 — shared temp tables (landed).** The `SHARED` keyword, the **`allow_shared_temp_ddl`**
  capability (§5), the `Database`-level temp snapshot, the two-root commit (§5), `shared_temp_mem`, and
  the cross-session visibility tests — which need the **concurrency schedule format**
  (`# format: concurrency`, concurrency-testing.md), since cross-session visibility is exactly what
  the single-handle sqllogictest corpus can't express. The shared-temp snapshot is held on the
  `Database` core (the shared roots cell, and the per-core equivalent) under **one lock with the
  file-snapshot root**, so a reader pins both atomically (no torn pin) and a writer publishes both in
  one swap. On a lone `Engine` (a session's private handle) the shared-temp snapshot is a plain field,
  visible to every session minted from it. Same constructs and 0A000 narrowings as slice 1.
- **Temp-blockstore slice — session-local temp onto a MemoryBlockStore (landed).** The per-domain
  in-RAM `MemoryBlockStore` + pinned pager (§6), within-session free-list compaction
  (`reclaim_within_session` / `maybe_compact`, watermark-gated), the residency flip on temp leaves, and
  the **page-based** `54P03` budget (§7). Session-local only; **shared** temp stays fully-resident
  decoded (the follow-on below). All three cores; result/cost/byte-neutral (temp is never serialized —
  no `format_version` bump). Per-core white-box tests (compaction bound, reader-defers gate, compact
  footprint, zero-file-writes) for what the corpus can't reach.
- **Slice 3 — spill-to-disk.** With temp already paged (the temp-blockstore slice), spill is the temp
  `BlockStore` swap + a bounded temp pool (§6), the memory→disk budget split, the disk ceiling.
  Out-of-contract spill file (§10).

## 14. Deferred / follow-ons (none foreclosed)

- **Shared temp onto a MemoryBlockStore** — the shared-temp analogue of the temp-blockstore slice.
  Deferred because the storage identity must be **core-owned** (all sessions of a `Database` share one
  shared-temp page space) while temp DDL/commit runs at the engine level, and its compaction watermark
  is the cross-session `live` registry (available at publish, not in the engine's `commit_tx`) — so it
  needs the storage core-owned + the publish path decoupled (persist main only when main changed).
  Until it lands, shared temp keeps the fully-resident decoded path and its record-byte budget (§6/§7).
- `ON COMMIT DELETE ROWS` / `ON COMMIT DROP` (§1, D6).
- `IF NOT EXISTS`, `CREATE TEMP TABLE … AS SELECT` (§1).
- `FOREIGN KEY` involving temp tables (§8) — start with FKs among same-kind temp tables.
- Spill-to-disk (Slice 3, §6) — the only deferred *storage* piece; the seam is already open
  (CLAUDE.md §9).
- Temporary **views** / a session-local relation namespace object beyond tables.
