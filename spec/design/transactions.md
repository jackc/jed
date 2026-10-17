# Transactions & the commit model — design

> How the engine realizes the CLAUDE.md §3 concurrency rule — single writer, readers
> blocked only during the commit window, exactly one committed version plus one writer's
> pending set, **not** MVCC. This is a *design* doc and the canonical record for the
> transaction model. The SQL surface (`BEGIN`/`COMMIT`/`ROLLBACK`) and its conformance
> corpus **landed in P5.2** (§4); this doc is the canonical model they implement. The
> per-impl host API (the `Transaction` handle, `view`/`update`, the `synchronous` setting) is
> in [api.md](api.md); the storage realization is in [storage.md](storage.md) §4. When a
> decision here changes, update [CLAUDE.md](../../CLAUDE.md) §3/§9, [storage.md](storage.md)
> §4, and [api.md](api.md) in the same edit.
>
> **This doc supersedes the old "no autocommit" policy** ([api.md](api.md) §2.2 as first
> shipped). That policy was an accident of the whole-image writer (durability cost dictated
> the transaction model), not a purposeful choice; jed now adopts **PostgreSQL autocommit**
> (CLAUDE.md §1) and **decouples the commit boundary from durability** (§9).

## 1. What this realizes, and the accident it corrects

CLAUDE.md §3 fixes the concurrency model: **at most one writer**; a writer accumulates all
its changes in a **private in-memory staging area** (the pending write set) while the last
committed state stays continuously readable; readers **never block** against an in-flight
writer; the only globally-exclusive moment is the **commit** itself, which publishes the
staged changes atomically. There is exactly **one committed version plus one writer's
pending set** — no version chains, no per-row timestamps, no vacuum.

**The accident this corrects.** The first host API fused two different things into one
`commit()` call: the **transaction boundary** (when changes become atomic and visible) and
**durability** (the `fsync`). Because durability was a whole-image rewrite (expensive), the
path of least resistance was "make the host call `commit` explicitly and rarely" — so the
*cost of durability dictated the transaction model*. That is backwards, and it produced two
surprises: no autocommit (every mutation needed a manual `commit` to persist), and a `close`
that silently discarded committed-looking work.

The correction is to **un-fuse the two concerns** (§9):

- **Transaction commit** = the snapshot swap (§2). Atomic, visible, cheap; happens at the end
  of *every* transaction, including an autocommit single-statement one.
- **Durability** = the `fsync`, governed by a `synchronous` setting (default **on**), orthogonal
  to the commit boundary.

Once un-fused, **autocommit is just the PostgreSQL default** (§4): each statement is its own
transaction that commits on success / rolls back on error, unless inside an explicit
`BEGIN … COMMIT`. This is what hosts expect (SQLite, MySQL, PG all autocommit by default) and
what CLAUDE.md §1 selects (the prior "no autocommit" was an undocumented divergence with no
overriding reason). The remaining job of this slice is to make the pending set **first-class,
rollback-able, and snapshot-isolated** — the three properties the always-mutate-live model
lacked.

## 2. The model: immutable snapshots + a working root

The committed state is an **immutable `Snapshot`**. A transaction is *a view of a `Snapshot`*
— a **read** transaction is a reference to a committed snapshot; a **write** transaction is a
*working* snapshot built from it that has not been swapped in yet. So the §3 "staging area,"
the "read snapshot," and the "pending write set" are **one structure**, not three.

```
Snapshot (immutable) = { txid, tables: PersistentMap<name → TableEntry> }
TableEntry           = { def, store: PersistentTree<key,Row>, next_rowid }

Database handle      = { committed: ref<Snapshot>,   # last committed, what fresh readers see
                         current_tx,                  # the open transaction, if any (§4)
                         write_lock,                  # held by the one write tx (§10)
                         live_snapshots,              # liveness registry (§8)
                         synchronous }                # durability mode (§9)

Transaction          = read:  { snapshot: ref<Snapshot> }                 # no write lock
                       write: { working: Snapshot, base_txid, … }          # holds write_lock
```

- A **write** transaction builds each statement's effect against `working` — the persistent
  structures (§3) copy every touched path that is shared or published, so producing the new
  `working` does **not** mutate `committed`. Rust may reuse a uniquely owned, dirty touched-path
  suffix under the exact rule in §3; a committed root, cursor pin, or writable-CTE read pin makes
  `Arc::get_mut` fail and therefore takes the ordinary copy-on-write path. After the statement's
  two-phase validation (§6) the new root is adopted into `working`. Read-your-writes within the
  transaction falls out: a write is immediately visible to the next statement on the same
  transaction because that statement reads `working`.
- A **read** transaction holds a committed `Snapshot` by reference and never builds a working
  root — it cannot mutate (§4.3). Many may be open at once.
- **Commit** (of a write tx) publishes `working` — `committed := working`, **a single pointer
  swap** (the §3 short commit window) — makes it durable per the `synchronous` setting (§9),
  releases the write lock, and bumps `txid` when that database changed. Committing a read tx is a no-op.
- **Rollback** drops the pending root (`working` discarded) and releases the write lock. For a
  read tx it just releases the snapshot.
- A `Rows` cursor captures its transaction's `Snapshot` and is thereby stable for its life and
  lock-free; the writer cannot disturb it because the writer never mutates a published snapshot
  in place.

This is the bbolt model (a read tx is a `View`, a write tx is an `Update` owning its own root;
commit swaps the meta root), here realized in memory first ([storage.md](storage.md) §4,
CLAUDE.md §12).

### 2.1 Commits without persistent mutations

A successful writable transaction with no mutations to main or attachments finishes without
serializing a database, writing commit metadata, issuing a durability barrier, or advancing a
database's `txid`. This applies to explicit blocks, scoped host callbacks, scripts, and autocommit.
The transaction still finishes normally: release writer gates and pins, retain successful session
state, and adopt any session-local temporary changes. Read-only mode still enforces restrictions
and avoids writer contention.

Track mutations per database domain, not by statement kind or affected-row count. Catalog,
sequence, row/index, and persisted statistics mutations count; `SELECT nextval(...)` may write,
and a zero-row write may have sequence or writable-CTE side effects. A zero-row DML operation
without such effects does not invalidate statistics or persist. Merely requesting a writable
store is not a mutation. Empty/SELECT-only blocks and temp-only work leave main unchanged.
An actual rewrite (including an UPDATE assigning the existing value), or mutations subsequently
undone by further SQL, may still commit: transaction-wide value equality is not required.

Commit reports main and attachment changes separately to shared publication. Only changed domains
are packed/persisted; attachment-only work publishes the attached roots without rewriting main or
advancing main's version. A main version remains the main-domain reclamation watermark; attachment
reuse continues to require the existing conservative absence of any live reader. The one-durable-
writer check counts changed file domains. No-op completion must not enter validated COW, including
recovered-generation stabilization; the next actual write still performs all required barriers.

**Carve-out: the page-service maintenance commit** ([page-service.md](page-service.md) §6). A page
client never rebuilds the free list, so after a client commit crosses the reclamation threshold the
owner commits the unchanged snapshot at the next `txid` with in-commit reclamation. It mutates no
table, sequence, statistic, or catalog fact, yet it writes pages (the catalog chain, which every
commit's manifest must name, and the rebuilt free list), issues the durability barrier, and advances
main's `txid`. It is not a transaction and no session observes it except as a newer version.

Detection must not scan rows, fault pages, or serialize an image for comparison. The current cores
use catalog/data-metadata generations and structurally shared table/index roots (plus rowid state)
to refine the transaction's candidate dirty domains. Generations are runtime-only and clone with
the working snapshot; this changes neither the file format nor SQL cost charges.

## 3. The persistent ordered map

The one new primitive. It replaces the current per-table store (a mutable
`BTreeMap`/hash-map-plus-sort) with a **persistent (immutable, structurally-shared) ordered
map** keyed by the encoded-key bytes (memcmp order — [encoding.md](encoding.md)). Required
operations: `get`, logical `insert→new`, `remove→new`, in-order `iter`, and `range` (for later).
Each mutation **path-copies every shared/published node** from root→leaf and shares the untouched
siblings, so every prior root is provably unchanged and a snapshot is an O(1) reference clone.

**Dirty INSERT transients (2026-07, no format change).** All three cores avoid rebuilding a dirty,
unpublished root-to-leaf suffix on every INSERT, but the ownership proof is language-specific. Rust
requires both `Arc::get_mut` uniqueness and `page == 0`. Go and TypeScript cannot infer uniqueness
from an ordinary pointer/object reference, so each `PMap` instead carries a private opaque
**mutation-generation token** and every node created by its INSERT path is stamped with that token.
A Go/TypeScript node may be edited only when `page == 0`, its stamp is the map's current token, and
that token is still active. The Go token is a non-zero-sized private pointer with an atomic active
bit; the TypeScript token is a private object identity (not a wrapping number) and its node stamp is
stored under a module-private symbol, which structured cloning and serialization do not expose.

Creating an immutable alias invalidates the current token. The next INSERT obtains a fresh token,
copies every old-generation node on the touched path, and stamps the replacements; later INSERTs may
reuse that newly owned dirty suffix. The invalidation boundaries are the complete root-alias
inventory: snapshot/store clone (transaction fork, statement rollback/read pin, writable-CTE
pre-statement pin, and attachment working-root fork), pull-cursor/scan creation, a streaming cursor's
clone of any mutable working snapshot, and commit/publication of main, temp, and attachment roots.
Loaded/Packed nodes and post-commit nodes are clean and therefore ineligible independently of the
token. Go code must call the explicit `PMap` clone operation; a raw struct copy is not an ownership
boundary.

In every core a qualifying leaf edits its key/row/weight vectors in place; a qualifying interior
replaces the changed child or inserts a propagated separator in place. Overflow still runs the same
size-fit and split builders, preserving the exact split point, separator, tree shape, and bytes. In Go
and TypeScript this transient covers insert/overwrite only; their delete/rebalance remains pure
path-copy.

**Rust dirty DELETE transients (2026-10, no format or cost change).** Rust applies the same
`Arc::get_mut` + `page == 0` rule to remove and rebalance. A qualifying leaf removes the record from
its vectors in place; a qualifying interior replaces the changed child and then runs the unchanged
merge-then-maybe-split rebalance on itself: the parent's separator/child lists are edited in place,
and a leaf merge moves a qualifying operand's vectors into the merged leaf instead of copying them.
Every fallible step (faulting a sibling, decoding a Packed/clean sibling) completes before any vector
moves. An interior merge still copies its two operands' separator/child lists, because an abandoned
merge must leave them untouched. The merge, split, and abandon decisions and their inputs are those of
the path-copy form, so tree shape and bytes are identical; a focused test pins the edited tree to a
pinned (always path-copied) twin after every step. The measurements are in
[benchmarks.md](benchmarks.md) ("DELETE unique-dirty result").

**Decided shape: a copy-on-write B+tree** (v24 — [bplus-reshape.md](bplus-reshape.md); it was
a CLRS-style B-tree with records at every level through v23). Chosen deliberately as the
in-memory form of the on-disk tree (Phase 6, [storage.md](storage.md) §6): the incremental
copy-on-write commit **page-backs the tree we already have** rather than building one. Records
live **only in leaves**; an interior node is a record-free routing skeleton of **separator
keys** (a copy of a boundary key, possibly stale after deletes — it keeps routing: left < sep
≤ right holds forever) plus child pointers, so `get` always descends to a leaf and a range
scan is a leaf walk driven by a **cursor stack** — deliberately **no leaf sibling pointers**,
which would break copy-on-write (fixing a neighbour's back-link would copy the neighbour on
every split; bbolt avoids them for the same reason). The original *"B-tree, not a persistent
BST"* call stands — a binary node never maps to a page; the reshape doubled down on the
page-mappable-tree bet, and the BST fallback once noted here is **retired** (the goldens + the
cross-core round-trip keep the lockstep tractable — bplus-reshape.md §11).

**Cross-core contract — widened at Phase 6.** Through Phase 5 only **iteration order**
(ascending encoded key) and the **serialized on-disk bytes** were contractual; the in-RAM node
structure (fan-out, split points) was a **private per-core detail**. **P6.1 closed that
freedom:** the in-memory copy-on-write tree *is* the on-disk tree (node ↔ page), so its node
layout and its **size-driven split/merge rules are a §8 byte contract**, spec'd with golden
fixtures in [../fileformat/format.md](../fileformat/format.md). All four implementations
(Rust/Go/TS + the Ruby reference) run the identical v24 rules (format.md "Fan-out"): a **leaf**
that overflows `C` splits 2-way and **copies** the right half's first key up as the parent's
separator; an **interior** node that overflows **pushes up** its median separator (with the
pinned degenerate `N = 2 → m = 1` split for near-cap separators); delete rebalances underfull
(`payload < C/2`) nodes by **merge-then-maybe-split** (a leaf merge removes the parent
separator; an interior merge pulls it down; an interior merge whose result cannot 2-way split
is abandoned) — over the kept `RECORD_MAX(K) = (C − max(12, 12+16·K))/2` leaf-record cap. The
trees — and therefore the bytes — are identical. Fan-out is governed by **page fit**, not a
tuning constant.

**In-memory reclamation is free.** An old `Snapshot` is reclaimed by the language's own
mechanism the instant nothing references it — `Arc` refcount in Rust, GC in Go/TS — so the
§3 "old version becomes free after commit" is automatic in memory. The explicit free-list
that replaces it for *pages* is Phase 6, and it leans on the §8 watermark.

## 4. Modes, control surface, and access modes

> The grammar ([../grammar/grammar.ebnf](../grammar/grammar.ebnf), [grammar.md](grammar.md)),
> the parsers, and the conformance corpus for the SQL statements **landed in the P5.2 sub-slice**
> ([TODO.md](../../TODO.md) Phase 5), spec-first as always. This section fixes their semantics;
> the host-API equivalents are in [api.md](api.md).

### 4.1 Autocommit (the default)

Between explicit transactions the handle is in **autocommit** mode. Each statement runs in its
own implicit single-statement transaction:

- The engine **infers the access mode from the statement kind**: a read statement (`SELECT`, a
  read-only query expression / set operation) → a **read** transaction (a committed snapshot,
  no write lock); a write statement (`INSERT`/`UPDATE`/`DELETE`/`CREATE`/`DROP`/…) → a **write**
  transaction (a working root + the write lock).
- On **success** the implicit transaction **commits** — snapshot swap + durability per the
  `synchronous` setting (§9). On **error** it **rolls back** (the statement's two-phase pass
  already guarantees no partial write — §6); autocommit continues and subsequent statements run
  normally. This is PostgreSQL autocommit behavior, and because per-statement atomicity already
  matched it, **the conformance harness stays green** (each statement commits, the next sees it
  — read-your-writes across statements is preserved).

### 4.2 Explicit transaction blocks

`BEGIN [TRANSACTION] [READ ONLY | READ WRITE]` (also `START TRANSACTION …`; default access
mode **READ WRITE** — on a **read-only handle** the default flips to READ ONLY and an explicit
READ WRITE is `25006`, PostgreSQL hot-standby behavior, api.md §2.1) opens an explicit block;
subsequent statements run within it until it ends:

- **`COMMIT`** (`COMMIT [TRANSACTION|WORK]`, `END`) publishes + makes durable (§9) and returns
  to autocommit. Committing a **failed** block (§6) performs a `ROLLBACK` instead (PostgreSQL).
- **`ROLLBACK`** (`ROLLBACK [TRANSACTION|WORK]`) discards `working` and returns to autocommit;
  it also clears a **failed** block.
- **`BEGIN` while already in an explicit block** has no defined action (nesting is
  `SAVEPOINT`'s job — §11) → **`25001 active_sql_transaction`**. PostgreSQL only warns here;
  jed has no warning channel, and silently ignoring the statement would hide a nesting bug, so
  it errors without aborting the block.
- **`COMMIT`/`ROLLBACK` in autocommit mode** (no open block) → a **lenient no-op success**.
  PostgreSQL warns ("there is no transaction in progress"); jed has no warning channel
  (CLAUDE.md §4), so it silently succeeds rather than raising — a documented, deliberate
  divergence. (No `25P01` is raised; only the savepoint statements raise it — §11.)

The asymmetry — `BEGIN`-in-block errors, `COMMIT`/`ROLLBACK`-with-no-block do not — is
principled: `COMMIT`/`ROLLBACK` always have a well-defined action (publish/discard the current
work), while a nested `BEGIN` does not. Error where the action is undefined; succeed where it
is defined.

### 4.3 Access modes: read-only vs read-write

The access mode is **load-bearing for concurrency** (§10): a **read** transaction takes **no
write lock**, so any number run concurrently with each other and with the one writer; a
**write** transaction takes the **exclusive write lock**. Because the lock cannot be acquired
lazily mid-transaction without upgrade/deadlock hazards, the mode is **fixed when the
transaction opens** — declared for explicit blocks, inferred for autocommit (§4.1):

- **READ WRITE** (the default) may read and write; it takes the write lock at `BEGIN` and holds
  it for the whole block (even across its read-only statements — a host wanting maximal read
  concurrency should use READ ONLY).
- **READ ONLY** may only read; it takes no write lock and pins **one committed snapshot across
  all its statements** (the reason a host opens one even under single-writer: a multi-statement
  *consistent* read — read a balance, then the matching ledger rows, against one snapshot). A
  write statement attempted in a READ ONLY transaction → **`25006 read_only_sql_transaction`**
  (PostgreSQL's code). A READ ONLY transaction needs no working root at all.

These long-lived read snapshots are exactly the **live readers** the §8 watermark tracks, so
this is also what makes Phase 6 page reclamation safe.

### 4.4 The host API surface (api.md)

The same model, programmatically (idiomatic per core — [api.md](api.md) §6):

- **`db.begin(writable) -> Transaction`** opens an explicit transaction; statements run on it
  (`tx.execute(…)`, `tx.query(…) -> Rows`); `tx.commit()` / `tx.rollback()` end it.
- **`db.view(fn)`** (read) and **`db.update(fn)`** (read-write) are closure wrappers
  (bbolt-style): open the transaction, run `fn(tx)`, **auto-commit on success / auto-rollback
  on error or panic** — the safe default that cannot forget to end the transaction.
- The **autocommit one-shots** `db.execute(sql)` / `db.query(sql)` wrap `begin → run → commit`
  with the inferred access mode (§4.1) — they are how the conformance harness and simple hosts
  drive the engine.
- The **SQL** `BEGIN`/`COMMIT`/`ROLLBACK` drive the handle's implicit current transaction (for
  SQL-string-only hosts and the corpus); they and the API forms are two surfaces over one
  mechanism.

### 4.5 DDL is transactional

`CREATE TABLE` / `DROP TABLE` stage into `working` like any mutation and roll back with it
(PostgreSQL behavior). The atomic unit a commit publishes is **catalog + every table's rows +
the rowid counters** as one swappable `Snapshot` — which is also why Phase 6's incremental
commit must copy-on-write the catalog page chain, not only data pages.

## 5. Isolation & visibility

- **Snapshot isolation, per transaction.** Every transaction sees a stable snapshot for its
  life: a read transaction pins its committed snapshot across all its statements (§4.3); a write
  transaction reads its own `working` root (read-your-writes). With a single writer (§10) there
  are no write-write conflicts, so no serialization failures and no retry loop. We commit to
  snapshot isolation and **nothing weaker** — there is no `READ UNCOMMITTED` (a reader never
  sees another transaction's unpublished working set).
- **Autocommit reads see the latest committed state.** Each autocommit `SELECT` is its own read
  transaction, so consecutive autocommit reads may observe different committed states as the
  writer advances. A host that needs several reads against *one* state uses an explicit
  `READ ONLY` transaction (§4.3).
- **A `Rows` cursor is snapshot-stable for its life** — its rows cannot change mid-iteration
  even if a writer commits, because a published snapshot is never mutated in place.

## 6. Error & abort semantics

Statement-level atomicity is already two-phase / all-or-nothing (CLAUDE.md §11 step 6:
`INSERT`/`UPDATE` validate every row before writing any). Transaction-level abort composes on
top of it and **depends on the mode**, faithfully mirroring PostgreSQL:

- **Autocommit** (§4.1): a statement error rolls back **only that statement** (its two-phase
  pass guarantees it wrote nothing partial); autocommit continues and subsequent statements run
  normally. This is PostgreSQL autocommit error behavior and exactly today's behavior — so the
  corpus stays green.
- **Explicit block** (§4.2): a statement error **aborts the transaction** — it enters the
  **failed** state. Every subsequent statement except `ROLLBACK` (and `COMMIT`, treated as
  `ROLLBACK`) and `ROLLBACK TO` a savepoint is rejected with **`25P02
  in_failed_sql_transaction`** until the block ends. `ROLLBACK` clears the failed state, and so
  does `ROLLBACK TO` an existing savepoint, which returns the block to that savepoint's state (§11). This matches PostgreSQL's "current transaction is aborted,
  commands ignored until end of transaction block."

New error codes (class 25, *invalid transaction state*), in
[../errors/registry.toml](../errors/registry.toml):

| code | name | raised when |
|---|---|---|
| `25001` | `active_sql_transaction` | `BEGIN` issued inside an explicit block (no nesting — §4.2) |
| `25006` | `read_only_sql_transaction` | a write statement issued in a READ ONLY transaction (§4.3) |
| `25P01` | `no_active_sql_transaction` | `SAVEPOINT` / `RELEASE` / `ROLLBACK TO` with no open block (§11) |
| `25P02` | `in_failed_sql_transaction` | any statement but `ROLLBACK`/`COMMIT`/`ROLLBACK TO` in a failed block (§6) |
| `3B001` | `invalid_savepoint_specification` | `RELEASE` / `ROLLBACK TO` naming no savepoint of the block (§11) |

A `COMMIT`/`ROLLBACK` with no open block is a no-op success, **not** an error (§4.2); `25P01` is
raised only by the savepoint statements, which have no meaning outside a block. These class-25 codes are a per-core SQL/API surface,
asserted by the shared corpus only via the deterministic `BEGIN/…/ROLLBACK` visibility tests
(§10).

## 7. The rowid counter and other transactional state

The no-PK synthetic rowid is a **monotonic counter** per table (`next_rowid`, reconstructed
on load as `max(key)+1` — [../fileformat/format.md](../fileformat/format.md)). Under staging
it is **part of the `Snapshot`** (per-`TableEntry`): pending `INSERT`s advance the `working`
copy, and a **rollback** discards those allocations along with the rows (the counter returns to
the committed value). Because the rowid is an **internal** key and never a user-visible
sequence, this is correct and simpler than PostgreSQL sequences (which deliberately *burn* a
value on rollback); jed has no such observable to preserve. Any future transactional counter
(e.g. an `IDENTITY`/sequence, if ever added) would have to decide burn-vs-restore explicitly;
the internal rowid restores.

## 8. The reader-liveness watermark (realized on the `Database` core; gates file-backed reclamation)

Every `Snapshot` carries its `txid` (its **version**). The **oldest live version** is the
minimum version any still-open reader has pinned, or the committed version when no reader is
live. In Phase 5 this watermark does no work — memory reclamation is the language's job (§3). It
is built now because **Phase 6's page free-list needs it**: a page freed at commit `T` may be
reused only once `oldest_live_txid > T`, otherwise a still-open reader holding an older snapshot
would observe a recycled page. Tracking liveness now (where it is nearly free) is what keeps
Phase 6 reclamation safe without a retrofit — the single tightest coupling between the two phases.

**P6.2 consumes the watermark (the free-list gate).** Phase 6's page free-list
([../fileformat/format.md](../fileformat/format.md) *Reclamation*) is the consumer the watermark
was built for: a page freed at `txid T` may be reused only once `oldest_live_txid > T`, else a
still-open reader on an older snapshot would observe a recycled page. P6.2's first form
**reconstructs the free-list on open** (the file's dead pages = `[2, page_count)` minus the
committed root's reachable set) and reuses them during the session; every such page was already
dead at the opened committed version, and a single file-backed handle has
`oldest_live_txid == committed.txid`, so the gate holds **trivially**. It became load-bearing when
*continuous* within-session reclamation paired with file-backed reader sharing landed — a just-orphaned
page (reachable at version `T`) must stay out of reuse until `oldest_live_txid ≥ T+1` — and is now
enforced by the free-list generation gate (below).

**Within-session reclamation has landed for temp domains (the watermark is now load-bearing there).**
The temp-blockstore slice ([temp-tables.md §6](temp-tables.md)) routes session-local temp stores onto a
never-reopened in-RAM `MemoryBlockStore`, which reconstruct-on-open can never reclaim — so it rebuilds
the free-list *within the session* at commit (`maybe_compact`), and that reuse is gated on exactly this
watermark: `oldest_live_version(new_txid) == new_txid` (no live reader or streaming cursor pins an older
version), deferring compaction while a pin is held. This is the same coupling the general file
follow-on will need, exercised first on a surface with **no cross-core byte contract** (a temp store is
never serialized, so its physical page layout is per-core). The **file/in-memory main** domain **has since
opted in** — continuous within-session reclamation landed with on-disk free-list persistence
(`format_version` 25): the file main reclaims **in-commit** (`plan_free_list`), the in-memory main
**post-commit** (`maybe_compact`), and both **reuse** the free-list under the watermark gate below.

**The watermark lives on the `Database` core (§10).** A lone `Engine` (the single-threaded handle a
session owns privately) has only one live snapshot at a time (`committed`, or an open tx's
`working`), so its `oldest_live_txid` is trivially the committed version. The interesting case is the
shared **`Database`**: it owns a **live-reader registry** — a multiset of pinned versions
(`version → refcount`, several readers may pin the same version). A read-only session, on open, pins
the committed snapshot and registers its version; on close/drop it deregisters. `oldest_live_txid` is
the registry's minimum, or the committed version when the registry is empty. The minimum is
order-independent, so no hash-map iteration order leaks into it (CLAUDE.md §8). The per-core tests
assert it tracks pinned readers (a reader pinning an old version holds the watermark back; closing it
lets the watermark advance).

**The watermark is now load-bearing: the free-list reuse gate (✅ landed).** With continuous
within-session reclamation, the commit allocator DOES re-add mid-session orphans, so the gate is no
longer trivially satisfied — a page reachable at a live reader's pinned version can enter the reusable
free-list, and reusing it would recycle a page that reader still observes (a snapshot-isolation
violation). Two mechanisms, in every core (`shared`/`format`/`persist` — Go/Rust/TS), close it:

- **Free-list generation gate (the reuse check).** Each storage tracks `free_gen_txid` — the version
  the current free-list is "as of" (the last compaction's `txid`, or the committed version at open;
  every page in the list is dead at that version). A commit **reuses** the free-list only when
  `oldest_live_txid ≥ free_gen_txid` — no live reader pins a version older than the generation, so no
  reader references any page in it. When the gate defers reuse, the commit allocates from the
  high-water and the free-list waits (still persisted), reused as soon as the pins drain. The gate
  covers **both** data-page reuse **and** the free-list **chain** pages (which overwrite in place: they
  are torn-write-safe at the fallback snapshot but not reader-safe, so a deferred commit grows the
  high-water for them too). With no reader pinning an older version (single-handle, reconstruct-on-open,
  all readers current) `oldest_live_txid == committed`, the gate always passes, and the **on-disk byte
  layout is byte-for-byte unchanged** — the gate only *defers* reuse under an older-pinning reader; it
  never changes *which* pages a single-handle commit picks. So no `format_version` bump, and every
  golden is identical.

- **Atomic pin registration (the watermark can't be fooled).** A reader must not be invisible to the
  gate in the window between reading the committed root and registering its pin: were it, a writer could
  reclaim a version the reader is about to pin. So the load and the registration happen under one
  acquisition of the live-registry lock (`pin_latest`), and **publish takes the same lock**, so a
  reader either sees the new committed and pins it, or is counted at the old version — never missed.
  (In the single-threaded TS core this is automatic — no thread can interleave mid-registration — so
  only the reuse gate is needed there; Go and Rust make it explicit.)

The **fallback reader** is the case that motivates both: a reader that loads the committed root in a
writer's persist→publish window pins the *prior* version just as that commit's compaction places one of
its pages into the reusable free-list. The generation gate defers reuse of that page while the reader is
live; atomic registration guarantees the reader is counted. Each core has a deterministic regression
test (a `AFTER_PERSIST_HOOK`-style seam pins the fallback reader, then reuse-commits hammer the pool)
that fails "snapshot isolation violated" with the gate disabled and passes with it.

## 9. Durability: the `synchronous` setting, this slice vs. Phase 6

Commit (the snapshot swap, §2) and durability (the `fsync`) are **separate** (§1). A
database-level **`synchronous`** setting governs *when* the fsync fires relative to the commit:

- **`on` (default)** — a commit makes its changes durable **before it returns**: the existing
  crash-safe recipe ([api.md](api.md) §3 — temp file + `fsync` + atomic `rename` + dir `fsync`
  in the whole-image era; dirty-page write + meta-slot publish + `fsync` in Phase 6). Safe; the
  per-statement cost under autocommit is the familiar SQLite/PG `synchronous_commit = on` cost,
  and the escape hatch is an explicit `BEGIN … COMMIT` (one fsync for many statements).
- **`off` / relaxed** — a commit is **visible in memory immediately** but its fsync is
  **batched / deferred**: one later barrier (a *flush*) makes a whole run of commits durable at
  once. Faster; a power loss or OS crash may lose the **commits acknowledged since the last flush**
  but **never corrupts**, because the on-disk image always holds a valid *older* snapshot (the root
  is published atomically — §2, [storage.md](storage.md) §4). This is the standard
  `PRAGMA synchronous=OFF` / `synchronous_commit=off` trade. **Built** — the protocol, its flush
  points, and the precise guarantee are [validated-cow.md](validated-cow.md) §4. In short:

  - every relaxed commit writes its pages and meta before it returns (meta last), so a crash of the
    process alone loses no acknowledged commit;
  - a flush happens when a batch reaches the **cap** (its pending pages fill one page's worth of
    manifest entries), on `close`, before a page client's write transaction is granted the owner's
    gate, and at compaction or the stabilization of an adopted generation; the cap flush runs on a
    background goroutine/thread in Go and Rust and inline in TypeScript, and the next commit waits for
    it, so every core writes the same bytes;
  - while a batch is pending, commits allocate only from the high-water (a page the durable fallback
    still needs is never overwritten) and their manifests vouch for every page written since the
    floor, so recovery lands **between the last flush and the newest attempted commit**.

  Results, plans, and cost are identical in both modes; only when the barrier runs differs. The
  setting is a **handle** setting (`create`/`open`, [api.md](api.md) §2.1), not stored in the file,
  and applies to the handle's file attachments too; an in-memory database ignores it. There is
  deliberately **no time-based flush yet**: the interval and its knob are an open decision
  ([../../TODO.md](../../TODO.md)), so an idle relaxed handle stays undurable until its next cap
  flush or `close`.

**The seam, default `on`.** The fsync is the single chokepoint at the commit boundary; `off`
(batching/group-commit) is an additive change behind it and can land later. **Phase 5** kept
durability whole-image (the §3 recipe behind the §2 block seam); **P6.1** changed the
materialization to incremental copy-on-write — write the dirty pages the new root introduced +
the rewritten catalog, `sync`, publish the alternate meta slot, `sync` — under a **frozen**
transaction API, making the per-commit fsync write `O(dirty path)` pages instead of the whole
image ([../fileformat/format.md](../fileformat/format.md), *Allocation & incremental commit*).
The `synchronous` setting is orthogonal to both. (This refines CLAUDE.md §9's "writes … land
durably on the SSD at commit": durably at commit under `synchronous=on`, batched under `off`.)

**The `synchronous=on` fsync was then made cheap** without moving the commit boundary
([pager.md](pager.md) §7): the storage seam **preallocates file growth in 1 MiB chunks** (real,
durably-allocated zero blocks ahead of the committed `page_count`) and the per-commit body+meta
barrier uses **`fdatasync`**, so a steady-state commit overwrites already-allocated space with no
ext4 file-growth metadata journaling — `insert_commit_durable` fell ~9 ms → ~2.8 ms p50 across all
three cores. This changes only fsync *timing/flavor*, never the commit-visibility boundary, and is
byte- and cost-neutral (the slack is trailing zeros past the high-water).

**What "durable" means per platform — and why jed is slower than SQLite/PostgreSQL on macOS.** A
committed write passes three layers: the OS page cache, the drive's volatile write cache, and stable
media. `synchronous=on` promises the commit is on **stable media** before it returns, so it survives
a sudden **power loss**, not just a process or OS crash. Each platform's flush meets that differently:

- **Linux** (ext4/xfs, default barriers): `fsync`/`fdatasync` write the OS cache *and* issue a
  drive cache flush. Ordinary `fsync` is the full guarantee, so jed, SQLite (`synchronous=FULL`),
  and PostgreSQL (default `wal_sync_method = fdatasync`) are equally durable there.
- **macOS / Apple platforms:** ordinary `fsync`/`fdatasync` only hand the data to the drive. It may
  sit in the drive's volatile cache and reach media **in a different order** from how it was written.
  Only `fcntl(F_FULLFSYNC)` drains that cache. Every jed core's durable flush on Apple targets
  is `F_FULLFSYNC` (Go `File.Sync`, Rust `sync_data`/`sync_all`, libuv `fdatasync`). SQLite's default
  (`PRAGMA fullfsync = OFF`) and PostgreSQL's default `wal_sync_method` use the ordinary calls.
  Measured on an Apple M3 Max: ~4 ms per `F_FULLFSYNC` versus 20–60 μs for `fsync`/`fdatasync`
  ([durable-commit-experiments.md](durable-commit-experiments.md#macos-measurements-2026-10-13)).

The weaker flush matters only if the drive itself loses power before its cache drains. An app crash
or kernel panic is safe, because the drive stays powered and empties its cache. Under a sudden power
cut, it can fail in two ways: (1) **lost commits**: transactions reported committed are gone; and
(2) **corruption**: crash safety depends on write *ordering* (SQLite's journal before its database
file, jed's body pages before its meta page), and a drive that persists out of order can leave a state
that was never valid.

Those defaults are reasonable for those projects. SQLite leaves `fullfsync` opt-in, accepting the
power-loss window on hardware where it is rare (battery-backed laptops and phones). PostgreSQL treats macOS as a
development platform: production runs on Linux, where its default is fully durable, and its
reliability docs point macOS users at `wal_sync_method = fsync_writethrough`. **jed does not
follow them here** (a deliberate divergence from the PG-default rule, CLAUDE.md §1). The overriding
reason: durability at commit is a core promise of `synchronous=on` (CLAUDE.md §9), and an embedded
library also runs on battery-less desktop Macs, where the failure is real. A platform-dependent
weakening would make the same setting mean different things on different hosts. Speed on macOS
comes from fewer flushes per acknowledged transaction (explicit `BEGIN … COMMIT` batching, or the
relaxed mode above), never from a weaker flush under `synchronous=on`. Consequently, durable-write
benchmarks against default SQLite/PostgreSQL on macOS compare different guarantees
([benchmarks.md](benchmarks.md) §8, *Durability caveats*).

**`skip_fsync` — the dev/testing knob (distinct from `synchronous=off`).** `synchronous=off` above
is a *durability-preserving* relaxation (deferred fsync, **never corrupts** — a valid older snapshot
always remains). Realizing it on a copy-on-write engine is subtle — the durable on-disk root lags the
in-memory one, so the allocator must not reuse a page the lagging durable snapshot still references —
which is why it carries its own allocation rules ([validated-cow.md](validated-cow.md) §4).
**`skip_fsync`** (api.md §2.1; Go `.SkipFsync`, TS `{ skipFsync }`) is different: a create/open handle
setting that makes the per-commit `fdatasync` barriers (and the create-time image + durable-grow
fsyncs) **no-ops**. A commit writes the *identical bytes in the same order* — byte/cost/result-neutral,
and reclamation is untouched (it still reclaims exactly as `synchronous=on`, because `skip_fsync` makes
no durability promise it must protect) — but the flush is skipped entirely. So it is **not** "never
corrupts": durable across a **process** crash (the OS page cache flushes) but **not** an OS crash /
power loss, for throwaway/rebuildable databases, bulk loads, and the conformance disk harness (which
reopens a temp image before every record to exercise the on-disk read path, where the
fsync-per-commit was ~78% of disk-mode wall time). Production that must not lose a commit keeps the
default fsync-on path; production that can lose the last few commits but never its file uses
`synchronous=off`.

## 10. Concurrency mechanism & the testing split

- **Single writer, lock-free readers.** A read transaction takes **no** lock and never blocks —
  not even during another transaction's commit swap, since it holds its own snapshot and does
  not observe the pointer change (CLAUDE.md §3's "readers block only during commit" is the
  conservative statement; the immutable-snapshot model does better). A read-write transaction
  takes the **exclusive write lock** at `begin`/`BEGIN` and holds it until commit/rollback, so
  at most one writer exists at a time (§3). Within one `Database`, snapshots + this local gate are the
  mechanism. For separate processes over one file, [locking.md](locking.md) §5 extends the same rule
  through the page service: the owner (the `presence EX` holder) keeps this exact fast path, a page
  client's writer takes the owner's writer gate with `begin_write`, and a client's read pins the
  owner's snapshot with `begin_read`, so the owner's gate and watermark cover every process.

- **`Database` is the shared core; a `Session` is the per-caller handle** (the converged shape,
  [session.md §2.4](session.md) — was a separate `SharedDb` minting `ReadHandle`/`WriteHandle`).
  A single-threaded **`Engine`** (the renamed executor handle a session owns privately) is fast and
  simple but **not safe to share across threads**: its reads borrow the engine while a write mutates
  it, so one engine cannot serve a reader thread and a writer thread at once. Real parallelism —
  readers running *concurrently with* an in-flight writer — needs the committed state behind a
  **thread-safe cell decoupled from any one engine**. So **`Database` holds** exactly the §3 shape,
  and is cheap to clone/share across threads:
  - a **committed cell** holding the published snapshot — a reader pins it with a single cheap
    read (an `Arc` clone under a momentary read lock in Rust; a lock-free `atomic.Pointer` load in
    Go), a writer publishes a new one with a single swap (the §3 short commit window);
  - a **single-writer gate** — a writer **blocks** until the prior writer ends (a `Mutex`/condvar
    in Rust, a held `sync.Mutex` in Go), so at most one writer is ever open;
  - the **live-reader registry** of §8.
  A **read-only session** (`db.read_session()`) pins the committed snapshot, registers its version,
  serves reads from that immutable snapshot (a write through it is `25006`), and deregisters on
  drop/close. A **writable session** (`db.session()`/`write_session()`) acquires the gate **only to
  write** (the unified lazy-gate rule, session.md §2.4): an autocommit write takes the gate, captures
  committed as a working set, publishes at the next version, and releases; an explicit `BEGIN` holds
  it from the block's first write until commit/rollback. Isolation is free from the persistent stores
  (§3): a pinned snapshot shares structure with later versions and is never mutated, so pinning is a
  pointer copy, not a deep copy.

- **Per-core reality differs, and that is expected (CLAUDE.md §2 — best experience per language,
  not uniform parallelism).** Rust and Go get **true OS-thread parallelism**: reader threads run
  on cores while a writer commits. The TS core has **no shared-memory threads for live objects**,
  so it offers the *other* half of the model — snapshot **isolation** across async interleavings
  (a pinned reader sees one stable version even as a writer commits between its calls) — via the
  same machinery, minus the parallelism (and a second open writer is **rejected** rather than
  blocked, since JS cannot block its one thread). Concurrent sessions work for both **in-memory and
  file-backed** databases; the file-backed case additionally requires a thread-safe pager and
  **watermark-gated page reclamation** (§8), reusing the same publish point plus the §9 persist
  chokepoint (the concurrency mechanism and durability are orthogonal axes).

- **Testing splits along the determinism line:**
  - **Logical transaction semantics → the shared conformance corpus.** A
    `BEGIN / INSERT / ROLLBACK / SELECT-shows-nothing` sequence, abort-poisoning (`25P02`), a
    read-only-violation (`25006`), and visibility are all deterministic and single-handle, so
    they are ordinary corpus entries (a `transactions` profile + `txn.*` capabilities in
    [../conformance/manifest.toml](../conformance/manifest.toml)).
  - **The concurrency mechanism → per-core tests.** "A reader does not block during a concurrent
    commit," "the writer is exclusive," "a reader pinned before a commit still yields the
    pre-commit rows," and "the watermark tracks pinned readers" depend on scheduling (Rust/Go) or
    interleaving (TS) and are **not** cross-core deterministic, so they live in each core's own
    test suite — Rust/Go fan out real threads (Go under `-race`), TS asserts isolation across
    interleaved calls — exactly as the `$N` bind parameters are tested per-core, not in the
    corpus ([conformance.md](conformance.md) §1.2).

## 11. Savepoints

`SAVEPOINT name`, `RELEASE [SAVEPOINT] name`, and `ROLLBACK [TRANSACTION|WORK] TO [SAVEPOINT]
name` ([grammar.md](grammar.md) §27) nest work inside an explicit block, with PostgreSQL's
semantics. ORMs use them for nested transactions.

**Model.** The open transaction carries a **stack of savepoint frames**, oldest first. A frame
captures everything about the transaction that a statement can change: the main `working`
snapshot, the session temp snapshot (`temp_working`), the per-attachment working snapshots, the
dirty flags, and the session's `currval`/`lastval` state. The snapshots are persistent structures,
so a frame is an O(1) copy that shares every unchanged node with `working`. (In the Go and TS cores
the clone also invalidates the working map's mutation generation, so later writes path-copy instead
of editing a node the frame shares.) `SAVEPOINT` pushes a frame; nothing is published, the writer
gate is not touched, and no page is written. Because each of these is snapshot state, restoring the
snapshots restores, together:

- table and index roots (rows, index entries, the synthetic rowid counters — §7);
- the catalog, so DDL since the savepoint is undone;
- sequence counters, which are snapshot fields because `nextval` is transactional
  ([sequences.md](sequences.md) §5) — a rolled-back advance is handed out again, a divergence
  from PostgreSQL that follows from the existing one;
- P9 statistics and estimator revisions ([statistics.md](statistics.md));
- session temp tables, including their `ON COMMIT` actions, which apply at the top-level commit
  to whatever the working temp snapshot then holds ([temp-tables.md](temp-tables.md) §5.1);
- the writes to each host-attached database, including the absence of any write
  ([attached-databases.md](attached-databases.md) §5);
- the pending-write bytes ([memory.md](memory.md) §7). They are counted per store and travel with
  the snapshots, so after `ROLLBACK TO` the balance is the frame's. A frame shares the versions
  staged before it, so `SAVEPOINT` and `ROLLBACK TO` forget the per-key record that makes the
  balance net (memory.md §7.2): a later rewrite or delete of such a version returns nothing, and
  savepoints add no unaccounted residency.

Prepared-plan caches need no invalidation. A cache entry is filled only from committed state
([api.md](api.md) §2.4), and catalog generations and estimator revisions are snapshot fields: after
`ROLLBACK TO` the visible signature is exactly the one the savepoint saw, which either equals the
committed signature (with committed schema) or names a working state no entry was filled from.

**Statements.**

- **`SAVEPOINT name`** pushes a frame. Names may repeat; a later one **shadows** an earlier one.
- **`RELEASE name`** finds the newest frame named `name` and removes it and every later frame.
  Their changes stay in the transaction.
- **`ROLLBACK TO name`** finds the newest frame named `name`, restores its state, and removes every
  later frame. The frame itself stays, so it can be rolled back to again. It also **clears the
  failed state** (§6): it is the way to keep a block alive after an error.
- **`COMMIT`** publishes the current state regardless of open savepoints; **`ROLLBACK`** discards
  everything. Both drop the stack. A failed block's `COMMIT` is still a `ROLLBACK`.
- Names are identifiers: unquoted names fold to lower case, quoted names are exact. The
  `SAVEPOINT` noise word after `RELEASE`/`ROLLBACK TO` is consumed only when a name follows it, so
  `RELEASE savepoint` releases a savepoint named `savepoint`.

**Errors** (PostgreSQL's):

- No open block: every form is `25P01` (no_active_sql_transaction). There is no block to abort.
- Failed block: `SAVEPOINT` and `RELEASE` are `25P02`. `ROLLBACK TO` an unknown name is `3B001`
  and the block stays failed.
- Clean block: an unknown name is `3B001`, which aborts the block like any other statement error.
- Inside `execute_script` every savepoint statement is `0A000`, like the other transaction-control
  statements ([session.md](session.md) §4.2).

Savepoints work the same in a READ ONLY block (its frames pin the same snapshot) and in a shared
write session: the frames are the session's private working state, so a concurrent reader never
sees them and a queued writer stays queued until the holder's top-level commit
(`concurrency/savepoint.test`). The host API needs no savepoint methods: `tx.execute("SAVEPOINT
a")` drives the same mechanism ([api.md](api.md) §2.2).

## 12. Deferred / explicitly not foreclosed

- **Lazy write-lock acquisition** — a READ WRITE transaction takes the write lock at `begin`
  (bbolt's model), holding it across any read-only prologue. Deferring acquisition until the
  first write (more read concurrency) is a future optimization; not foreclosed, but it carries
  upgrade/deadlock hazards a single up-front grab avoids.
- **Group-commit / async durability** — the `synchronous=off` batching machinery is built (§9,
  [validated-cow.md](validated-cow.md) §4): the batch cap, the background cap flush, and fsync
  coalescing across every committer of the handle (its sessions on any thread, and its owner-side
  maintenance commits) — successive commits share one barrier. Still open: a **time-based** flush
  (its interval and knob are undecided), and coalescing for page clients' commits (a client's
  `synchronous` setting does not reach the owner, which barriers every client commit). Under
  `synchronous=on` a commit holds the writer gate through its barrier, as PostgreSQL makes a commit
  visible only once durable; overlapping the next writer with that barrier would need it to build on
  an unpublished snapshot (early lock release), which is not foreclosed and not planned.
- **MVCC / multiple concurrent committed versions** — explicitly **not** this (CLAUDE.md §3):
  one committed version plus one working set, period.
- **Isolation levels other than snapshot** — no `SET TRANSACTION ISOLATION LEVEL`; snapshot
  isolation is the single level (§5). `READ UNCOMMITTED` is impossible by construction.
- **Two-phase commit / distributed transactions** — out of scope (single embedded file).
