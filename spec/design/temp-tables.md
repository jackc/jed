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
> **Status: slices 1–2 landed (all three cores).** Slice 1 (session-local temp tables, memory-only)
> and slice 2 (database-wide `SHARED` temp tables — the `Database`-level temp snapshot, the two-root
> commit, `allow_shared_temp_ddl`, `shared_temp_mem`, and cross-session visibility via the concurrency
> schedule format) are implemented byte-identically in Rust, Go, and TS. The grammar production
> (`table_scope`, now including `SHARED`), the `54P03` code, and the budget settings landed with their
> slices. **Slice 3 (spill-to-disk, §6) remains deferred.** This doc was written first, spec-first
> (CLAUDE.md §11), and tracks the implemented behavior.

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
- **Shared temp catalog** — a second *committed* in-memory structure on the `Database` / `SharedDb`
  handle (a "temp snapshot"), published by the same commit discipline as the file snapshot but
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

## 6. Storage model — memory-only now, spill-to-disk as a designed-in seam

**Slice 1 is memory-only**, as requested. A temp `TableStore` is created with `paging: None` — the
exact "fully resident in-memory B-tree" mode that pure in-memory databases already use (pager.md §1,
pmap.rs `Child::Resident`). No new storage code: the resident B-tree, its splits, its value codec,
its indexes, and its key encoding are the ones persistent tables use. Memory-only means a temp table
that outgrows its budget (§7) **errors** (`54P03`) rather than spilling — the bound is hard.

**Spill-to-disk is the deferred follow-on, and the seam already exists** (CLAUDE.md §9
non-foreclosure; the agents confirmed the B-tree is parameterized over a `LeafSource`/`SharedPaging`,
not hardwired to the main file). When it lands, a temp store that crosses its **memory** budget flips
from resident to **paged against a second, temp-only `BlockStore`** — a host-supplied temp file
(storage.md §2, hosts.md §2), entirely separate from the main file's `BlockStore`, so the
zero-main-file-writes invariant is preserved by construction. The temp `BlockStore` is *not* the
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
- **`shared_temp_mem`** — a **global** byte budget for all shared temp storage, a `Database` /
  `SharedDb`-level setting (shared temp data is global, so its budget must be too), charged across
  sessions. `0` = unlimited. Default modest.

**Determinism is the load-bearing requirement** (CLAUDE.md §8/§10, §13). The budget is measured in
**byte-identical on-disk record bytes** — the sum, over every temp table store *and* its index
stores, of each stored record's on-disk encoding size (`record_size`, the exact weight the page B-tree
splits on, byte-identical across cores by the §8 file-format contract). This is deliberately **not**
the `work_mem` spill estimator: spill is out-of-contract (per-core, §10), so its estimate need not
agree across cores, whereas `54P03` is **in**-contract — every core must abort at the same point. So
"budget exceeded" is a **pure function of `(operations, budget)`**, identical across Rust/Go/TS, never
dependent on allocator behavior, GC, or pointer width. **The check is per-statement:** after a
statement that writes a temp table, the session's total temp footprint is summed and the statement
aborts **`54P03 temp_storage_limit_exceeded`** if it *exceeds* `temp_buffers` (`0` ⇒ unlimited); the
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
- **`serial` / `GENERATED … AS IDENTITY`** — supported; the auto-created owned sequence is itself a
  **temp sequence** in the same temp store (session-local sequence for a session-local table; shared
  temp sequence for a shared table), so it too never touches the file. `nextval` on a temp sequence
  stays the transactional snapshot field it is for persistent sequences (sequences.md §5,
  determinism.md §5).
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
  the single-handle sqllogictest corpus can't express. The shared-temp snapshot is held on the shared
  handle (`SharedDb`/`Shared` in the Rust core, and the per-core equivalent) under **one lock with the
  file-snapshot root**, so a reader pins both atomically (no torn pin) and a writer publishes both in
  one swap. On a single (non-shared) handle the shared-temp snapshot is a plain field on the handle,
  visible to every session minted from it. Same constructs and 0A000 narrowings as slice 1.
- **Slice 3 — spill-to-disk.** The temp `BlockStore` + the resident→paged flip (§6), the memory→disk
  budget split, the disk ceiling. Out-of-contract spill file (§10).

## 14. Deferred / follow-ons (none foreclosed)

- `ON COMMIT DELETE ROWS` / `ON COMMIT DROP` (§1, D6).
- `IF NOT EXISTS`, `CREATE TEMP TABLE … AS SELECT` (§1).
- `FOREIGN KEY` involving temp tables (§8) — start with FKs among same-kind temp tables.
- Spill-to-disk (Slice 3, §6) — the only deferred *storage* piece; the seam is already open
  (CLAUDE.md §9).
- Temporary **views** / a session-local relation namespace object beyond tables.
