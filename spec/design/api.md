# Host API — design

> The embedding surface: how a host program opens a database file, prepares and runs
> statements, binds parameters, and iterates rows. This is a *design* doc — the API is a
> **per-impl surface, NOT part of the shared conformance corpus** (the corpus fixes SQL
> *semantics*, not the binding API — see [conformance.md](conformance.md) §1.2). It is
> documented canonically here only so the three cores keep **the same shape** (CLAUDE.md
> §1/§2): each core implements it idiomatically, but the operations, lifecycle, and error
> codes match. When a decision here changes, update [CLAUDE.md](../../CLAUDE.md) §3/§9 and
> [storage.md](storage.md) §4 in the same edit.

The north star is an **embeddable library** (CLAUDE.md §1) — a host links the engine in and
drives it through this surface. Before this, the only entry point was a one-shot
`execute(db, sql)` against an in-memory database; this doc is the formal surface that adds
durable database files, prepared statements, bind parameters, and a row cursor.

## 1. The shape (same across cores)

Six concepts. The names below are the *concept*; the idiomatic spelling per core is the
mapping table in §6.

- **`Database`** — **storage identity.** Holds the committed in-memory state plus a persistence
  identity: an optional file `path`, a monotonic commit counter `txid`, the `page_size`
  the file is serialized with, the buffer pool, and the `synchronous` durability mode. The
  *configured-handle* role this concept originally also played (cost ceilings, `read_only`, the
  entropy/clock seam) is **un-fused** onto the `Session` ([session.md](session.md)) — this doc's
  §8/§10 settings are session settings a bare `Database` proxies through its built-in default
  session.
- **`Session`** — **the configured host context** ([session.md](session.md)). The stateful,
  capability-bearing context a host runs statements through: it carries the safety envelope
  (a GRANT/REVOKE-style per-table `SELECT`/`INSERT`/`UPDATE`/`DELETE` + function `EXECUTE`
  privilege model with an `allow_ddl` gate, `max_cost`, `max_sql_length`, a `lifetime_max_cost`
  budget), the semantic settings (session variables, the time zone), and the entropy/clock
  seam, over an explicit transaction state machine ([session.md §2.2](session.md)); it opens
  transactions, runs autocommit one-shots, and runs a **multi-statement script** (`execute_script`,
  built on the **library-level** `split_statements` — a top-level core function with no
  `Session`/`Database` dependency). A bare `Database` owns one long-lived default session, so this
  is additive and back-compatible.
- **`PreparedStatement`** — a parsed, reusable statement. Parameter count/types are fixed at
  prepare time; the same statement runs many times with different bound values.
- **`Outcome`** — the result of running a statement: either a bare statement result carrying
  the accrued `cost` (plus, for DML, the affected-row count — §4), or a query result carrying
  column names, rows, and `cost`. Which variant a statement yields follows from its SQL: a
  `SELECT` (or set operation) is a query result, and so is a DML statement with a `RETURNING`
  clause ([grammar.md](grammar.md) §32); everything else is a bare statement result.
- **`Rows`** — a cursor over a query result, yielding one row at a time, plus the column
  names and the accrued cost.
- **`EngineError` / `SqlState`** — the structured error surface (errors are data, not prose
  — CLAUDE.md §5). Every operation surfaces these idiomatically.

## 2. Lifecycle

### 2.1 Opening a database

Two lifecycle constructors, deliberately split — **`create` vs `open`** — because bringing a
*fresh* database into being and attaching to an *existing* file are genuinely different
operations (different preconditions, error modes, and which options apply). **`create` covers
both backings**, in-memory and file: an in-memory database **is** a create — a fresh empty
database that simply has no file behind it — so the backing is a **property of the create
options**, not a separate constructor. This is why there is **no** in-memory-specific constructor
(the former `new()` / `NewDatabase()` / `newInMemory()` are **removed**); see §2.1.1.

- **`create(opts)`** — make a **new** database, in-memory or file-backed. **`opts.path`** selects
  the backing — **absent → in-memory** (never touches the filesystem), **present → a single file
  at that path**. It is expressed as a genuine *optional*, never an overloaded empty-string
  sentinel: Rust `path: Option<PathBuf>`, TS `path?: string`, Go `Path string` where the zero
  value (`""`) is the documented "unset" (Go has no option type; the field name + doc carry the
  intent, and no *positional* `create("")` exists to be hit by an uninitialized argument). This
  deliberately does **not** follow SQLite/DuckDB's overloaded path string, whose `""` means
  *opposite* things across those two engines (SQLite: a temp on-disk DB; DuckDB: in-memory).
  **`opts.page_size`** (default **8192**, the [storage.md](storage.md) §3 default) is **locked into
  the file's meta at creation** and cannot change thereafter; for an **in-memory** database it fixes
  the tree fan-out the database would serialize to (the page-backed B-tree's fan-out tracks the page
  size — [../fileformat/format.md](../fileformat/format.md), §9), so it is meaningful for both
  backings. It must be a **power of two** in **`[256, 65536]`** — `MIN_PAGE_SIZE` (256) through
  `MAX_PAGE_SIZE` (64 KiB; [../fileformat/format.md](../fileformat/format.md) *Page model*, the nine
  legal values); a page size below the minimum is `0A000 feature_not_supported` "page size too
  small", one above the maximum `0A000` "page size too large" (the cap bounds the largest single
  allocation, including against a hostile file — §2.1 *open*), and a non-power-of-two value in range
  `0A000` "page size must be a power of two". When a **path is given**, `create` writes an initial
  empty durable image immediately (§3), so the file exists with its page size fixed, and if the path
  **already exists** it is `58P02 duplicate_file` — `create` never clobbers. The **in-memory** path
  cannot fail in substance, but `create` still returns the uniform `Result` / `(Database, error)`
  signature (the error is invariably success in-memory) — a caller who wants an infallible in-memory
  handle wraps `create` (§2.1.1).
- **`open(path, opts?)`** — open an **existing** file: load it ([../fileformat/format.md](../fileformat/format.md)),
  adopting its recorded `page_size` and `txid`. The recorded `page_size` is validated to the same
  **power-of-two `[256, 65536]`** rule as `create` (above); a value outside the range *or* not a power
  of two is `XX001 data_corrupted`, so a corrupt or hostile file cannot force a multi-gigabyte
  allocation before its contents are even checked. If the
  path is **absent**, it is `58P01 undefined_file` — `open` never creates. A malformed file is `XX001
  data_corrupted`; an underlying read failure is `58030 io_error`. `opts` is optional open-time
  settings: the **memory budget** and the **read-only flag** below.

**Memory budget — a handle setting (P6.4c, [pager.md](pager.md) §3).** `open`'s `opts.cache_bytes`
sets the **buffer-pool budget in bytes**: the approximate maximum memory the demand-paging leaf cache
holds resident at once (the resident-set bound that lets a database far larger than RAM be served —
pager.md §1). Bytes, not a page count, because a page count silently scales with the file's `page_size`
(the same number means a 256× different footprint across page sizes) — the budget belongs to the
*caller's* memory, so it is stated in the caller's unit and the engine converts it using the file's
page size, known once the file is open: **`cache_leaves = max(1, cache_bytes / page_size)`** (integer
floor). The `max(1, …)` is the floor for the `page_size > cache_bytes` case — a budget smaller than one
page still keeps **one** leaf resident, the minimum to walk a root→leaf path. (The bound is on *cached
on-disk page bytes* — a proxy for resident memory, since a cached leaf is held *decoded*, whose heap
size depends on row content; it bounds the leaf count deterministically, not bytes exactly.) It is a
**handle** setting, **not** stored in the file (unlike `page_size`): a different host may reopen the
same file with a different budget. Default **256 MiB** (`DEFAULT_CACHE_BYTES` — sized so the dominant
RAM-sized-database case stays fully cache-resident, pager.md §3). The budget bounds only the **leaf
cache** — the
interior B-tree skeleton is always resident (pager.md §1/§4) — and it **never changes what a query
observes** (results and cost are invariant to it, pager.md §3/§5), so it is purely a memory/throughput
knob. A read-only gauge, **`resident_leaves`**, reports how many leaf pages are currently
resident — `≤ cache_leaves` by construction for a file; a **real count** for an in-memory
database too (bplus-reshape.md B3 — its pool is pinned/unbounded, so the gauge tracks its
faulted set). An in-memory database ignores the
budget (it is fully resident, nothing to page). Same shape across cores (Rust `OpenOptions {
cache_bytes }` / Go `OpenOptions { CacheBytes }` / TS `{ cacheBytes }`); the bare `open(path)` form uses
the default.

**Read-only open — a handle setting.** `open`'s `opts.read_only` (Rust `OpenOptions {
read_only }` / Go `OpenOptions { ReadOnly }` / TS `{ readOnly }`; default off) opens the file
**read-only**. The handle then behaves like **PostgreSQL hot standby** (the §1 PG-default
applied to a read-only database): every transaction defaults to **READ ONLY** — a plain
`BEGIN` (or `begin(false)`/`view`) works and reads normally — while an explicit READ WRITE
request (`BEGIN READ WRITE`, `begin(true)`, `update`) is **`25006`** ("cannot set transaction
read-write mode on a read-only database"), and an autocommit write statement is **`25006`**
with PG's hot-standby message ("cannot execute INSERT in a read-only transaction" — the
implicit transaction *is* read-only). Because no write transaction can open, no commit ever
publishes; the file is additionally opened **without write access**, so the OS enforces what
the guards promise (a read-only handle works on a read-only filesystem). The handle exposes
`read_only()` / `ReadOnly()` / `.readOnly`. Like the memory budget it is a handle setting, not
stored in the file — another handle may have the same file open writable.

**Work-memory budget — a handle setting ([spill.md](spill.md) §3).** `open`'s `opts.work_mem`
(Rust `OpenOptions { work_mem }` / Go `OpenOptions { WorkMem }` / TS `{ workMem }`; default
**`DEFAULT_WORK_MEM = 256 MiB`**) bounds the memory a single **blocking operator** — currently the
`ORDER BY` external merge sort (spill.md §4) — may hold resident before it **spills to disk**, so a
query over larger-than-RAM data never materializes its whole input in the executor's heap
(CLAUDE.md §9). It is PostgreSQL's `work_mem`, stated in the same unit (**bytes**), and like
`cache_bytes` / `max_cost` it is a **handle** setting (not stored in the file) that **never changes
what a query observes** — results and cost are invariant to it (spill.md §6), it only changes *when*
an operator spills. `db.set_work_mem(bytes)` / `SetWorkMem` / `setWorkMem` sets it on an open handle
(mirroring `set_max_cost`); `0` means **unlimited** (never spill). An **in-memory** database ignores
it — it has nowhere to spill, so a blocking operator stays fully resident regardless (spill.md §2,
mirroring the buffer pool's in-memory residency). Same shape across cores; the bare `open(path)`
form uses the default.

#### 2.1.1 One create, no in-memory constructor

The interface is deliberately **two constructors** — `create` (fresh, either backing) and `open`
(existing file) — and nothing else. The earlier surface had **five** overlapping entry points
(an in-memory `new()`, an in-memory-with-page-size variant, a file `create`, a file `open`, and a
hidden open-with-options); the first two **collapse into `create`** because they are the same
operation on a different backing. Three decisions fix the shape:

- **In-memory is signalled by an absent path, not its own constructor.** The knob analysis is
  what makes this clean: every option an in-memory database will *ever* accept is a **subset** of a
  file's (see the "future create-time knobs" note below), so one `CreateOptions` carries both
  backings and only grows in shared directions — parallel constructors would drift, a single
  `create(opts)` absorbs each new knob in one place.
- **No infallible in-memory convenience in the core surface.** `create` returns the fallible
  `Result` / `(Database, error)` uniformly, even though the in-memory path cannot fail. Wrapping it
  in an infallible helper is trivial and belongs to the **caller**, not the core API — and the
  cores' own **test suites are exactly that caller**, unwrapping the infallible in-memory `create`
  in whatever form suits each test layout (Go an in-package `memDB()` helper; TS a `memDb()` test
  util; Rust, whose integration tests are separate crates, an inline
  `Database::create(CreateOptions::default()).unwrap()` or a shared `tests/common` helper). That is
  where an infallible in-memory handle lives — a test/host convenience, never public core API.
- **`open` is *not* folded into `create`.** Open-existing is a genuinely different operation — the
  file must exist (`58P01` otherwise), it adopts the file's already-locked `page_size`/`txid` and so
  takes **no** `page_size`, and its failure modes differ. Collapsing it into `create` would be
  SQLite's flag-soup (`SQLITE_OPEN_CREATE` toggling create-vs-open on one call); keeping the two
  split keeps each intent explicit.

**Future create-time knobs (the shared `CreateOptions` is their one home).** `page_size` is the only
create-time knob today. The neighbouring embedded engines point at what comes next, and every
candidate is either backing-agnostic or a subset of the file case — so each lands as a new
`CreateOptions` field, not a new constructor:

- **A memory ceiling** — the standout. It is the in-memory twin of the file handle's
  `open` `cache_bytes` buffer-pool budget (§2.1): `cache_bytes` bounds the resident *cache* of a
  larger-than-RAM file; a `memory_limit` would bound the resident *dataset* of an in-memory
  database (which cannot evict, so the bound is a deterministic abort — a `53200`-style
  out-of-memory error, fitting the untrusted-query resource guarantee, CLAUDE.md §13). DuckDB's
  `memory_limit` (default 80% of RAM) and SQLite's `soft_heap_limit`/`cache_size` are this knob.
- **A spill target** — jed already has the `work_mem` *budget* (`open` opts, §2.1) but no *location*
  for where a spilling operator ([spill.md](spill.md)) overflows to; DuckDB's `temporary_directory`
  / SQLite's `temp_store` are this.
- **A thread/parallelism count** — a later lever on the near-lock-free read path (CLAUDE.md §2/§3);
  DuckDB's `threads` / SQLite's worker-thread limit.

Everything else those engines expose at construction — text encoding, default null/sort order,
collation, journal/`synchronous` mode — jed either **fixes by design** (determinism / PG semantics,
CLAUDE.md §8) or attaches to the **file/durability** side, so it never becomes an in-memory create
knob. An in-memory database never touches the filesystem.

### 2.2 Transactions, autocommit, and durability

The full transaction model is [transactions.md](transactions.md); this section fixes the API
shape. **jed autocommits by default** (PostgreSQL behavior — CLAUDE.md §1; this **supersedes**
the original "no autocommit" rule, which was an accident of the whole-image writer —
transactions.md §1). The commit boundary and durability are **decoupled** (transactions.md §9).

> **Revised by [session.md §2.2](session.md).** The transaction state lives on the **session**
> (`Idle`/`Open`/`Failed`); SQL `BEGIN`, `begin()`, and `view`/`update` are three entry points to
> **one** state machine, and the separate `Transaction` object below **collapses** into session
> state + optional per-core RAII sugar. The bullets here are the pre-session shape, re-expressed
> when the S1 session slice lands.

- **Autocommit (default).** Each statement run directly on the handle is its own transaction —
  it commits on success / rolls back on error. The engine infers the access mode from the
  statement kind (read → a snapshot, no write lock; write → the write lock). `db.execute(sql,
  params)` / `db.query(sql, params)` are the autocommit one-shots.
- **Explicit transactions.** **`db.begin(writable) -> Transaction`** opens a read
  (`writable=false`) or read-write (`writable=true`) transaction; statements run on it
  (`tx.execute`/`tx.query`); **`tx.commit()`** / **`tx.rollback()`** end it. A write on a read
  transaction is `25006`. The **closure wrappers** **`db.view(fn)`** (read) and
  **`db.update(fn)`** (read-write) open the transaction, run `fn(tx)`, and **auto-commit on
  success / auto-rollback on error or panic** — the safe default, recommended over a raw
  `begin`. SQL `BEGIN [READ ONLY|READ WRITE]` / `COMMIT` / `ROLLBACK` drive the handle's implicit
  current transaction equivalently.
- **`commit` / `rollback` are uniform across modes.** **In-memory** → commit **packs the dirty
  pages into the database's `MemoryBlockStore`** (bplus-reshape.md B3 — structurally the file
  commit minus `fsync`, whose memory-host `sync` is a no-op); the observable result is the same
  success it always was. Rollback discards the working set. **File-backed** → commit publishes +
  makes durable per the **`synchronous`** setting (below).
- **Durability — `synchronous` (default `on`).** `on` makes a commit durable **before it
  returns** (the §3 crash-safe recipe). `off`/relaxed makes the commit visible immediately and
  **batches/defers** the fsync — faster, may lose the last few commits on a crash, **never
  corrupts** (the on-disk image is always a valid older snapshot). The seam is built now, default
  `on`; the `off` batching policy can land later (transactions.md §9). Set at `create`/`open`
  via `opts`.

The `Transaction` surface + SQL `BEGIN`/`COMMIT`/`ROLLBACK` (with `READ ONLY`/`READ WRITE` and
failed-block poisoning) **landed in Phase 5** ([../../TODO.md](../../TODO.md)); their semantics
are fixed in transactions.md so this doc stays the shape-of-the-API record.

### 2.3 Close

**`close()`** releases the handle. It **rolls back any open explicit transaction** (its
in-progress work is discarded) and does **not** itself start or commit one. Under autocommit,
every prior statement is already committed and durable (per `synchronous`, §2.2), so — unlike
the original surprising rule — `close` does **not** drop committed work; durability is never
hidden in a destructor (error-prone in the GC'd Go/TS cores), it is the explicit result of each
commit. `close` is idempotent.

### 2.4 Prepare / execute / query

- **`prepare(sql) -> PreparedStatement`** parses the SQL once (errors like `42601` surface
  here) and returns a reusable handle. (Introspecting a statement's inferred parameter count
  before binding is deferred — the count is enforced at execute time via the `42601`
  count-mismatch check.)
- **`statement.execute(params) -> Outcome`** runs a (possibly mutating) statement and
  returns the materialized outcome. `statement.query(params) -> Rows` runs a query and
  returns a cursor. `params` is empty when the statement has no placeholders. A prepared
  statement runs **within a transaction** — an explicit one (on a `Transaction`) or, on the
  handle directly, the autocommit single-statement transaction of §2.2.
- One-shot convenience: `db.execute(sql, params)` / `db.query(sql, params)` are sugar for
  prepare-then-run (autocommit). The pre-API free function `execute(db, sql)` is kept unchanged
  (zero parameters) — the conformance harnesses depend on it.
- **Plan cache.** A `PreparedStatement.query` caches its **resolved plan** (not just the parsed
  AST) and reuses it across executes, so a repeated query skips planning entirely — the dominant
  cost of a trivial-plan / high-frequency lookup (planning is ~⅔ of a point lookup's latency and
  ~88% of its allocations). The cache is keyed on a **catalog generation** counter bumped by every
  schema-changing DDL (`CREATE`/`DROP`/`ALTER` of a table, type, or index): a DDL between executes
  invalidates the cached plan and the next execute re-plans (PostgreSQL invalidates prepared plans
  on schema change the same way). To stay collision-free across a rolled-back in-transaction DDL,
  the cache is **filled only from committed state**, making the committed generation strictly
  monotonic; a statement first executed *inside* an open transaction re-plans until it commits. A
  plan is cached only when reusing it is result/cost-**identical** to a fresh plan — so a plan with
  an uncorrelated subquery (whose per-execution constant-fold bakes in one execution's params), a
  precompiled-regex node (whose one-shot compile-cost flag mutates during eval), or a temp / SRF /
  CTE / derived relation is **never** cached and re-plans each execute. The routing was also unified
  to plan a scan-shaped query **once** (streaming and buffered were formerly two separate plans),
  which speeds the ad-hoc `query()`/`Session.query` path too. The behavior is result/cost/byte-
  neutral (planning is unmetered) — no on-disk format change, no conformance-corpus change.
  - **Rust note:** the cached plan is held behind an `Rc` (the plan is `!Sync` via a regex `Cell`,
    so `Arc` buys nothing), which makes the Rust `PreparedStatement` `!Send` (it was `Send + Sync`).
    This is a non-regression in practice — the whole Rust query/cursor path is already thread-affine
    (`Engine`/`Session`/`Rows` hold `Rc`s) — but it *is* an observable capability change: a host that
    wants a prepared statement on another thread re-prepares there (a cheap re-parse). `Database`
    stays `Send + Sync` (it mints a session per thread). Go and TS are unaffected (Go is GC'd; TS is
    single-threaded), so this is the one place the cores' host-API auto-traits differ — recorded in
    [cores.md](cores.md).

### 2.5 Concurrent sessions: parallel readers + a single writer

> **Converged by [session.md §2.4](session.md).** The first design (P5.3b) made this a *separate*
> `SharedDb` handle minting `ReadHandle`/`WriteHandle`. Those fold into `Database` + `Session`: the
> `Database` of §2.1 **is** the shared core, and an additional `Session` **is** the per-caller
> concurrency handle. The shape below is the converged surface; `SharedDb`/`ReadHandle`/`WriteHandle`
> no longer exist as types.

`Database` (returned by `create`/`open`, §2.1) is **cheap to clone and safe to share across
threads** — it holds the committed-roots cell, the single-writer gate, and the live-reader watermark
(transactions.md §8/§10). Its **default session** (§2.4) is the simple, fast single-handle path.
For **concurrent readers running alongside a writer**, a host mints **additional sessions**:

- **`db.read_session(opts) -> Session`** opens a **READ ONLY** session: it pins the committed
  snapshot *now* and serves reads from that one stable, immutable version for its life — never
  blocked by, and never blocking, the writer. A write through it is `25006`. It registers in the
  live-reader set (transactions.md §8); **`session.close()`** (Go/TS — no destructor) / dropping it
  (Rust) deregisters, advancing the watermark. `db.oldest_live_txid()` reports the oldest version
  any open reader still pins.
- **`db.write_session(opts)` / `db.session(opts)` -> Session** opens a **READ WRITE** session. It
  does **not** hold the writer gate idle (the unified lazy-gate rule, session.md §2.4): an autocommit
  write acquires the gate, applies, publishes at the next version (the §3 commit window), and
  releases it; an explicit `BEGIN` pins one snapshot and acquires the gate on its first write,
  holding it until `commit()`/`rollback()`. At most one writer holds the gate at a time — a second
  writer **blocks** until it releases (Rust/Go) or is **rejected `25001`** (TS, which cannot block
  its one thread). Statements run with full transaction semantics (read-your-writes, failed-block
  poisoning).

All three are the **same `Session` type** (session.md §2/§3) — they differ only in access mode and
in `opts` (the `SessionOptions` envelope). The default session and these additional sessions share
the one `Database`'s committed state.

**Per-core reality** (CLAUDE.md §2 — best experience per language): Rust and Go give true OS-thread
parallelism (reader threads run while a writer commits); TS gives snapshot **isolation** across
async interleavings (no shared-memory threads), with a second writer rejected `25001`. Concurrent
sessions work for both **in-memory and file-backed** databases (the file-backed case adds a
thread-safe pager + watermark-gated reclamation, session.md §2.4); the single-handle surface
(§2.1–§2.4) is unchanged and remains the default.

## 3. Persistence & durability

The on-disk model is the **page-backed copy-on-write B-tree** with **incremental commit**
([../fileformat/format.md](../fileformat/format.md), [storage.md](storage.md) §4): a commit
writes only the **dirty pages** a mutation introduced — the copy-on-write path from the changed
leaves up to the root, plus the small rewritten catalog chain — and publishes the new root by
writing the **alternate meta slot** (`txid & 1`). The whole-image serializer survives only as
`create`'s initial from-scratch write and the golden generator; it is no longer the commit path.

The recipe below is the **`synchronous=on`** durable-commit path (§2.2, transactions.md §9): it
fires at **every** durable commit — each autocommit write statement and each explicit `COMMIT`
alike. Under `synchronous=off` the commit is visible immediately and the `fsync` is **batched /
deferred** (still all-or-nothing when it does run). Because a commit writes only dirty pages, an
autocommit write touches a handful of pages, not the whole file — explicit `BEGIN…COMMIT` still
batches many statements into one commit.

**Crash-safe commit recipe** (identical across cores):

1. Write the **dirty body pages** (the copy-on-write tree path + the rewritten catalog chain)
   to their page slots.
2. `sync` the file, so the body pages are durable **before** the meta swap that references them.
3. Write the **alternate meta slot** (`txid & 1`) with the new `txid`, `root_page`, and CRC.
4. `sync` again, committing the atomic root swap.

At every instant the on-disk root is either the previous valid meta slot or the new one — never
a torn mix — because the body pages are durable before the meta swap and the highest-`txid` valid
slot wins on open. The loader validates each meta slot's CRC **and** every body page's per-page
CRC (v7), so residual corruption surfaces as `XX001`, never silent bad data; the target is
SSD/POSIX ([storage.md](storage.md) §1) and the fsync timing is refined by the pager's
preallocation + `fdatasync` path (pager.md §7).

`create` writes its initial empty image from scratch (with `txid` starting at 1), filling **both**
meta slots; every commit thereafter is incremental and alternates the slot.

## 4. Rows and result types

`Rows` iterates over the query's rows **one at a time** and exposes the column names and the
accrued `cost`. The cursor is the seam that keeps the API from hardening a full-residency
assumption (the [storage.md](storage.md) §1 binding rule): the caller-visible contract (yield
row, then row, then column metadata) is exactly what a pull/streaming executor satisfies — so
streaming lands *behind* the cursor without changing any caller. The **true streaming cursor**
is specified in [streaming.md](streaming.md) and landing in slices: `Rows` is a **pull source**
(`Cursor`) where the non-blocking single-table pipeline streams row-at-a-time and the blocking
operators buffer-then-stream their output. **Landed (S3 + S4, all three cores):** a `query()` →
`Rows` over the single-table no-blocking-operator read (the PK-ordered / LIMIT-short-circuit
shape) is a lazy `Streaming` cursor — scan → resolve → `WHERE` → project, one row per `next`
(S3); and a **blocking** read (a non-PK `ORDER BY`, `DISTINCT`, aggregate / `GROUP BY`, window,
or a join) is a lazy `Buffered` cursor (S4) that **buffers its input** (on the first pull) but
**yields the output one row at a time** — bounding peak *output* memory and letting a caller's
early exit skip the projection of the rows it never pulls. Both pull over a pinned snapshot. Two
contract notes that come with them: the cursor **pins its read snapshot for its life**
(PG-faithful — [streaming.md §5](streaming.md), [transactions.md §5/§8](transactions.md)), so it
must be drained or `close`d to release; and `cost` is **final only after the cursor is fully
drained** (it accrues as rows are pulled — [streaming.md §6](streaming.md)). A mid-drain error (a
`54P01` cost abort, a `57014` cancellation, an arithmetic trap) surfaces during iteration — Rust
stashes it for [`Rows::error()`](streaming.md), Go sets `Rows.Err()`, TS throws out of the
iterator; this is so even for the `Buffered` cursor, whose blocking part runs on the first pull
(not at `query()`). **`execute()` still returns a fully materialized `Outcome`** (the conformance
harness drives it, so every `# cost:` value is unchanged — the lazy cursor is a `query()`-only
optimization, internal machinery whose only contract is identical rows + total cost under full
drain). A top-level set-operation / pure-query `WITH` read now streams too (a lazy **deferred** cursor,
[streaming.md §7](streaming.md) S6: it defers the whole run to the first pull and yields the result one
row at a time); and a **prepared** query streams identically to an ad-hoc one — `prepare` + `query_prepared`
routes the prepared AST through the same lazy lanes ([streaming.md §7](streaming.md) S8), so a prepared
`SELECT` pulls row-at-a-time, pins its snapshot, and gets the early-exit win. The internally-streamed
*operators* landed earlier
([spill.md](spill.md)): the `ORDER BY` external merge sort + its streaming single-table feed are
in, with the spilling hash aggregate / `DISTINCT` / hash JOIN as deferred follow-ons (CLAUDE.md
§9, Phase 6).

A statement result carries `cost` and the **affected-row count**: an INSERT, UPDATE, or
DELETE without RETURNING reports how many rows it touched — PostgreSQL's command-tag count
(`UPDATE 3`). A DML statement that matched nothing reports **0**; DDL and transaction
control report **no count** (Rust `Option<i64>` `None` / Go `HasRowsAffected == false` /
TS `null`) — mirroring PG, whose DDL tags carry no row count. DML *with* RETURNING is a
query result; its count is the result's row length. A query result carries `column_names`,
the materialized `rows`, and `cost`.

## 5. Parameters (`$N`)

A bind parameter is `$` followed by a 1-based decimal index (`$1`, `$2`, …; grammar.md §5,
[../grammar/grammar.ebnf](../grammar/grammar.ebnf)). Parameters are an **API construct**:
the corpus stays literal-only (§conformance.md 1.2), but the parser accepts `$N` anywhere a
primary expression is accepted and as an `INSERT` value slot.

**Typing is by context, statically, before execution.** The engine has a strict static type
system (CLAUDE.md §4); a parameter has no intrinsic type, so it adopts one from its context
— the other operand of a comparison/arithmetic, the target column of an `INSERT`/`UPDATE
SET`, or a `CAST` target. The cast-target case covers **both** spellings — `CAST($1 AS int)`
and the postfix `$1 :: int` ([grammar.md](grammar.md) §37) — so `$1 :: int` types `$1` as int
and `$1 :: numeric(10,2)` types it decimal and re-scales the bound value to `(10,2)`. A parameter
with **no derivable type** (e.g. a bare `SELECT $1`,
or a gap in `$1..$N`) is `42P18 indeterminate_datatype`. Conflicting inferences for the same
index (`i16` here, `text` there) are `42804 datatype_mismatch`. Two adaptable operands
with no anchoring type (`$1 = $2`, `$1 = 5`) default the parameter to `i64`, matching the
existing integer-literal default (a documented local-consistency divergence from PG).

**Binding is two-phase / all-or-nothing**, like `INSERT`/`UPDATE`: every supplied value is
coerced to its inferred type up front (out-of-range → `22003`, wrong family → `42804`, NULL
into a NOT NULL target → `23502`, bad `bytea`/`uuid` text → `22P02`) **before any row is
touched**, so a bad binding fails deterministically with no partial work. Supplying the
wrong number of values is `42601` (a malformed invocation — PG's `08P01` is wire-protocol
only and the engine has no wire protocol).

## 6. Idiomatic mapping

| Concept / op | Rust | Go | TS |
|---|---|---|---|
| create (in-memory or file) | `Database::create(opts) -> Result<Database>` (`opts.path: Option<PathBuf>`) | `CreateDatabase(opts) (*Database, error)` (`opts.Path string`) | `createDatabase(opts): Database` (`opts.path?`) |
| open file | `Database::open(path) -> Result<Database>` | `OpenDatabase(path) (*Database, error)` | `openDatabase(path): Database` |
| in-memory (no path) | `Database::create(CreateOptions::default())` | `CreateDatabase(CreateOptions{})` | `createDatabase({})` |
| infallible in-memory (test/host helper) | `fn mem_db()` wrapping `create` | `memDB()` wrapping `CreateDatabase` | `memDb()` wrapping `createDatabase` |
| commit (current tx) | `db.commit() -> Result<()>` | `db.Commit() error` | `commit(db): void` |
| rollback (current tx) | `db.rollback() -> Result<()>` | `db.Rollback() error` | `rollback(db): void` |
| begin | `db.begin(writable) -> Result<Transaction>` | `db.Begin(writable) (*Transaction, error)` | `begin(db, writable): Transaction` |
| view / update (closures) | `db.view(\|tx\| …)` / `db.update(\|tx\| …)` | `db.View(fn) error` / `db.Update(fn) error` | `view(db, fn)` / `update(db, fn)` |
| tx commit / rollback | `tx.commit()` / `tx.rollback()` | `tx.Commit()` / `tx.Rollback() error` | `tx.commit()` / `tx.rollback()` |
| close | `db.close()` + `Drop` | `db.Close() error` | `close(db): void` |
| prepare | `db.prepare(sql) -> Result<PreparedStatement>` | `db.Prepare(sql) (*PreparedStatement, error)` | `prepare(db, sql): PreparedStatement` |
| stmt execute | `stmt.execute(&mut db, &params) -> Result<Outcome>` | `stmt.Execute(params) (Outcome, error)` | `stmt.execute(params): Outcome` |
| stmt query | `stmt.query(&mut db, &params) -> Result<Rows>` | `stmt.Query(params) (*Rows, error)` | `stmt.query(params): Rows` |
| one-shot execute | `db.execute_params(sql, &params)` / free `execute(db, sql)` | `db.ExecuteSQL(sql, params)` / `Execute(db, sql)` | `executeSql(db, sql, params)` / `execute(db, sql)` |
| one-shot query | `db.query_sql(sql, &params) -> Result<Rows>` | `db.QuerySQL(sql, params) (*Rows, error)` | `querySql(db, sql, params): Rows` |
| rows iterate | `impl Iterator<Item = Vec<Value>>` | `for rows.Next() { rows.Row() }` | `for (const row of rows)` |
| rows columns | `rows.column_names()` | `rows.ColumnNames()` | `rows.columnNames` |
| rows cost | `rows.cost()` | `rows.Cost()` | `rows.cost` |
| rows close ([streaming.md §5](streaming.md)) | `rows.close()` + `Drop` | `rows.Close()` | `rows.close()` |
| rows affected (§4) | `Outcome::Statement { rows_affected: Option<i64>, .. }` | `outcome.RowsAffected, outcome.HasRowsAffected` | `outcome.rowsAffected: number \| null` |
| set cost ceiling (§8) | `db.set_max_cost(limit)` | `db.SetMaxCost(limit)` | `db.setMaxCost(limit)` |
| set input-size limit (§8) | `db.set_max_sql_length(bytes)` | `db.SetMaxSQLLength(bytes)` | `db.setMaxSqlLength(bytes)` |
| inject random source (§10) | `db.set_random_source(f)` / `db.clear_random_source()` | `db.SetRandomSource(f)` / `db.ClearRandomSource()` | `db.setRandomSource(f)` / `db.clearRandomSource()` |
| inject clock source (§10) | `db.set_clock_source(f)` / `db.clear_clock_source()` | `db.SetClockSource(f)` / `db.ClearClockSource()` | `db.setClockSource(f)` / `db.clearClockSource()` |
| load Unicode data (collation.md §4) | `db.load_unicode_data(r)` | `db.LoadUnicodeData(r)` | `db.loadUnicodeData(bytes)` |
| upgrade collations — clear a version-skew (collation.md §12) | `db.upgrade_collations() -> usize` | `db.UpgradeCollations() (int, error)` | `db.upgradeCollations(): number` |
| table lookup (catalog) | `db.table(name) -> Option<&Table>` | `db.Table(name) (*Table, bool)` | `db.table(name): Table \| undefined` |
| table names (catalog) | `db.table_names() -> Vec<String>` | `db.TableNames() []string` | `db.tableNames(): string[]` |

**Per-language divergences, deliberate and documented:**

- **Rust** passes `&mut Database` to `PreparedStatement::execute`/`query` (the statement owns
  only the parsed AST, never a `Database` borrow — this sidesteps the aliasing problem of a
  statement holding `&Database` while execution needs `&mut Database`). Go/TS bind the
  database at `prepare` (GC, no borrow checker), so `Execute`/`query` take no database
  argument. The **shape** — prepare → execute/query → rows — is identical.
- The public prepared handle is named **`PreparedStatement`** in all three (in Go it would
  otherwise collide with the AST `Statement` the executor consumes).
- Method names avoid collisions with the kept free functions: Go `ExecuteSQL` (vs package
  `Execute`), TS `executeSql` (vs exported `execute`).
- **The TS Browser/OPFS host is async** ([hosts.md](hosts.md) §5). Because an OPFS sync access handle
  is acquired asynchronously and is usable only in a Web Worker, the browser entry points are
  `createOpfs(name, opts)` / `openOpfs(name, opts)` returning `Promise<Database>` (in the worker), and
  a main-thread `OpfsDatabase` client whose `create`/`open`/`query`/`execute`/`commit`/`close` are all
  `Promise`-returning over `postMessage`. This async surface is a deliberate per-platform divergence
  from the synchronous file `create`/`open` above — the engine itself and the storage seam stay
  synchronous; only the OPFS acquisition edge and the worker RPC are async. No Rust/Go equivalent
  (browser-only).

**Catalog reads** (the last two rows) are the host's introspection surface until an SQL-level
one exists (an `information_schema`-like layer is a possible later feature): both read the
**currently-visible snapshot** (an open transaction's working set, else the committed state).
`table` returns the full definition — columns (name, type, typmod, NOT NULL, PK membership,
default), the primary key's ordinals in key order, CHECK constraints, and secondary indexes.
`table_names` returns every table's **canonical** (CREATE TABLE-spelled) name, sorted
ascending by **lowercased** name — the catalog's standing order, so no hash-map iteration
order leaks (CLAUDE.md §8). Secondary indexes are relations but not tables; they are excluded.

These catalog reads live on **both `Database` and `Session`** (a bare `Database` reads the
committed snapshot; a `Session` reads its currently-visible state). The **low-level single-threaded
core is an internal concern** — Rust `Engine` is `pub(crate)`, Go `engine` is unexported, TS `Engine`
lives only in the internal `tooling.ts` barrel, never the public `lib.ts`. Every host consumer — the
integration tests, the CLI, the conformance harness, and the C-ABI/WASM/Ruby wraps — drives
`Database`/`Session`, never the core directly; the few genuinely white-box storage/byte tests that
reach the core do so through each language's in-package/in-crate/internal-barrel seam.

## 7. Errors

`EngineError` carries a `SqlState` (the 5-char SQLSTATE, [../errors/registry.toml](../errors/registry.toml))
and a message; `.code()` returns the SQLSTATE. Idiomatic surfacing: Rust `Result<T,
EngineError>`, Go `(T, error)` with a `*EngineError`, TS `throw EngineError`. SQL errors keep
their existing codes; the API adds the host-filesystem class-58 codes (`58P01`/`58P02`/
`58030`, §2.1), the parameter code `42P18` (§5), the transaction-state class-25 codes
(`25001`/`25006`/`25P02`, transactions.md §6), and the **cancellation** code `57014`
`query_canceled` (§11, PG's exact code for a canceled statement / `statement_timeout`). The
SQLSTATE class (first two chars) is a stable category (`22` data, `23` integrity, `25`
transaction state, `42` syntax/access, `57` operator intervention, `58` system, `XX` internal).

## 8. Cost ceiling (`max_cost`)

A first-class use case is **safely evaluating untrusted, user-supplied queries** (CLAUDE.md
§13). The handle carries a **`max_cost`** setting — `db.set_max_cost(limit)` /
`db.SetMaxCost(limit)` / `db.setMaxCost(limit)` — that bounds the deterministic execution cost
([cost.md](cost.md)) of every statement run on it:

- `limit <= 0` (the **default**, `0`) ⇒ **unlimited** (the metered cost is still reported on
  `Outcome`/`Rows`, nothing aborts).
- `limit > 0` ⇒ the instant a statement's accrued cost **reaches** `limit`, execution aborts
  with **`54P01`** (`cost_limit_exceeded`). The ceiling is the first *disallowed* value: a query
  whose true cost equals `limit` aborts, one costing `limit − 1` completes.

The abort is **deterministic and cross-core identical** — the same `(query, db, max_cost)`
aborts (or completes) in Rust, Go, and TS alike (cost.md §6) — and it is an **ordinary engine
error**, so it integrates with rollback-on-error: an aborted autocommit `DELETE`/`UPDATE` leaves
the table untouched, and inside an explicit block it poisons the block (transactions.md §6).

It is a **handle setting**, not stored in the file and not a per-statement argument: the host
configures the budget once on whatever handle serves untrusted queries. A per-call override (an
options object on `execute`/`prepare`) stays open for later without changing this surface. The
`# max_cost: N` conformance directive (cost.md §6) exercises it cross-core.

### Input-size limit (`max_sql_length`)

A second untrusted-query safety setting bounds the work done **before** a statement runs — the
parse — which the cost ceiling cannot reach (parsing precedes metering). The handle carries a
**`max_sql_length`** setting — `db.set_max_sql_length(bytes)` / `db.SetMaxSQLLength(bytes)` /
`db.setMaxSqlLength(bytes)` — that caps the input SQL text length, in **bytes**, of every statement
parsed on it ([cost.md](cost.md) §7a):

- The **default** is **1 MiB** (`DEFAULT_MAX_SQL_LENGTH`) — generous for hand-written / ORM SQL, yet
  bounding the parse tree to a few MB.
- `bytes > 0` ⇒ a statement whose UTF-8 byte length **exceeds** `bytes` is rejected with **`54000`**
  (`program_limit_exceeded`) at parse entry, before lexing. The cap is the *maximum allowed* length
  (a statement of exactly `bytes` runs; one byte over aborts).
- `bytes == 0` ⇒ **unlimited** (a trusted caller's opt-out, e.g. a bulk load).

Like `max_cost` it is a **handle setting**, not stored in the file, and the abort is deterministic
and cross-core identical (same `(statement, max_sql_length)` → same outcome in Rust, Go, TS). It
applies on **every** handle-bound parse path — `execute`/`execute_params`, `prepare`, and the
session read/write handles — so the per-handle limit has no hole. Because jed is single-statement
per call, this one byte cap also transitively bounds the parse-tree node count (cost.md §7a). A
companion fixed limit, **`MAX_IDENTIFIER_LENGTH = 63` bytes** (`42622 name_too_long`, not a handle
setting), bounds any single identifier. A third fixed limit, **`MAX_COMPOSITE_DEPTH = 32`**
(`54001 statement_too_complex` at `CREATE TYPE`, `XX001` on a corrupt over-deep file; not a handle
setting), bounds composite-type nesting — a chain of cheap `CREATE TYPE`s that neither the input-size
cap nor the parser nesting limit sees, but whose recursive codec/comparator walks would overflow the
native stack ([cost.md §7b](cost.md)). The `# max_sql_length: N` conformance directive exercises the
input-size cap cross-core.

## 9. Non-goals this slice

- **Streaming rows at the cursor — IN, not a non-goal.** The fully pull-based cursor is specified
  in [streaming.md](streaming.md) and landing in slices (§4); **S3 + S4 have landed (all three
  cores):** a `query()` → `Rows` over the single-table no-blocking-operator read is a lazy `Streaming`
  pull source (S3), and a blocking read (non-PK `ORDER BY` / `DISTINCT` / aggregate / window / join) is
  a lazy `Buffered` cursor that buffers its input but yields the output one row at a time (S4); and a
  top-level set operation / pure-query `WITH` is a lazy **deferred** cursor that defers its run to the
  first pull (S6), the `exec_streaming_sort` output is yielded lazily from the `SortedRows` pull
  iterator (S7), and a **prepared** query (`prepare` + `query_prepared`) routes its AST through those
  same lazy lanes (S8) — all pin their snapshot for their life (`execute()` stays materialized — the
  corpus drives it). The internally-streamed *operators* (the `ORDER BY` external merge sort spilling
  under `work_mem`, [spill.md](spill.md)) landed earlier. What stays deferred is a `Database::query`
  watermark on the bare single-handle path, the spilling hash aggregate / `DISTINCT` / hash JOIN
  ([spill.md §7](spill.md)), and lazy small-inline-column decode ([streaming.md §8](streaming.md)).
- **Transactions are IN, not a non-goal.** The §3 staging buffer, autocommit, the `Transaction`
  surface (`begin`/`view`/`update`), the `synchronous` durability setting, and SQL
  `BEGIN`/`COMMIT`/`ROLLBACK` are specified in [transactions.md](transactions.md) and **landed in
  Phase 5**; §2.2–§2.3 above are revised accordingly (autocommit replaces the original "no
  autocommit" rule; `close` no longer drops committed work). What stays deferred is only
  `SAVEPOINT`/nested transactions, `synchronous=off` batching, and group-commit (transactions.md
  §11).
- **Browser/OPFS host — landed (TS only), not a non-goal.** The Node `fs` host built here has a
  sibling OPFS host in the TS core ([hosts.md](hosts.md) §5): `OpfsBlockStore` over
  `FileSystemSyncAccessHandle`, with the engine in a Web Worker driven by an **async** client. Its
  entry points are `createOpfs`/`openOpfs` (async, see the §6 note); the synchronous core is unchanged.
- **No low-level direct-access API** — kept open, not built ([storage.md](storage.md) §5).

## 10. Entropy + clock seam (`set_random_source` / `set_clock_source`)

The volatile UUID generators (`uuidv4`, `uuidv7`) and the current-time functions
(`now()`/`current_timestamp`, `clock_timestamp()`) read two host inputs behind seams
([entropy.md](entropy.md), [determinism.md](determinism.md) §5) so they stay deterministic given
those inputs. The inputs are injected as **functions**, each defaulting to the platform primitive.
Like `max_cost`/`work_mem`, they are **handle settings** — not stored in the file, not per-statement
arguments — configured once on whatever handle runs them:

- **`set_random_source(f)` / `clear_random_source()`** — inject a function that fills N random bytes
  (the deterministic / reproducible path) or clear it. **The default draws from the OS CSPRNG per
  value, so production UUIDs are unpredictable** — not derived from a single seeded PRNG. The engine
  provides **`seeded_random_source(u64)`** (a byte-exact splitmix64 stream — entropy.md §2) for the
  reproducible path; the conformance corpus injects it via the **`# seed:`** directive
  ([conformance.md](conformance.md) §4).
- **`set_clock_source(f)` / `clear_clock_source()`** — inject a function returning micros since the
  Unix epoch that `uuidv7` embeds and that `now()`/`current_timestamp` (read once per statement) and
  `clock_timestamp()` (read per call) return, or clear it (the default: the wall clock — entropy.md
  §5). The engine provides **`fixed_clock(i64)`** (a frozen instant) and **`advancing_clock(start,
  step)`** (returns `start, start+step, …`, one increment per read — distinguishes the per-call
  `clock_timestamp()` from the statement-stable `now()`); the corpus injects them via the
  **`# clock:`** and **`# clock_advance:`** directives.

Defaults (unset) read **OS entropy per value** and the **wall clock**: Go `crypto/rand` + `time`, TS
`node:crypto` + `Date`, Rust the `getrandom` crate (the one core dependency, CLAUDE.md §14) +
`SystemTime`. Neither setting changes what a non-generator query observes; a generator's *cost* is
invariant to both (one `operator_eval` per call). An out-of-range injected clock makes `uuidv7`
trap `22008`.

**Unicode-data load (a separate, load-time seam).** Collation tables and Unicode casing data are not
built into the engine — a host loads them via **`db.load_unicode_data(bytesOrReader)`** (privileged,
bytes or a reader, **never a path**, **not** SQL-reachable), handing the engine a pinned `JUCD`
bundle's bytes ([collation.md §4/§9](collation.md)). Unlike the entropy/clock seam this is **not** a
per-query draw and introduces no nondeterminism — the bytes are jed's own pinned data; like the
storage seam, the host owns where they come from (a file, a fetch, or a compiled-in `include_bytes!` /
`//go:embed` / bundled asset) and the engine does no I/O. A bare binary with no bundle loaded is `C`
collation + ASCII casing only ([collation.md §16](collation.md)). It is a handle setting, additive
across calls, never a per-statement argument. The sibling **`db.upgrade_collations()`** is the same
kind of privileged, not-SQL-reachable host op: after loading a *newer* bundle, it adopts that version
for this database — rebuilding the collated keys whose pin is now skewed and re-pinning the stamp, so
the affected tables are read-write again ([collation.md §12](collation.md)). Whole-database, atomic,
idempotent (returns the count of collations re-pinned; persisted by the next explicit `commit`).

## 11. Ergonomic bindings (per-core surface)

§2–§5 fix the *typed* surface — `prepare`/`execute`/`query` over the engine's tagged `Value` and
a row cursor of `Vec<Value>`/`[]Value`. That surface is exact and stays the floor; **this section
adds the ergonomic layer host programmers actually want** — pass plain language-native values,
scan into native destinations — modeled on `database/sql`/pgx (Go), `rusqlite` (Rust), the
object-row APIs (TS). It is **additive**: a thin conversion layer over the same `Value` currency,
no engine or executor change beyond the cancellation hook (below). Like the rest of this doc it is
a **per-impl surface, NOT the shared corpus** (§1, [conformance.md](conformance.md) §1.2) — each
core spells it idiomatically; this section fixes the **shape** so the cores stay recognizable.

The design was worked out against the Go core first (`impl/go/ergonomic.go`, `database/sql`/pgx-
shaped). The other cores do **not** transliterate it — each takes its language's *de facto*
embedded-SQL idiom, because the reason to add a per-language surface is to give *that* language's
users the best experience, not to keep the surfaces uniform (CLAUDE.md §2). So: **Go** =
`database/sql`/pgx (`...any` args, `Scan(&dest)`, struct mapping, `iter.Seq`); **Rust** = `rusqlite`
traits (`ToValue`/`FromValue`, tuple/array `Params`, `row.get::<T>`); **TS** = better-sqlite3
(`db.prepare(sql)` → a `Statement` with `run`/`get`/`all`/`iterate`, rows-as-objects). What is
shared is the **contract**, not the spelling: native params in + typed rows out, over the one
`Value` currency, additive over §2–§5, a per-impl surface (not the corpus). The §11.1–§11.3
descriptions below are the *Go* shape (the first landing); the Rust/TS subsections in §11.5 give
their idiomatic equivalents. It rests on a fact already true of the engine: **parameters are typed
by context, not by the value supplied** (§5), so a "loosely-typed" `Value` built from a native value
(`int64 → IntValue`) is still coerced two-phase to the inferred type — the conversion layer **adds
no path around the strict type system**, it only saves the caller from hand-building `Value`s.

### 11.1 Three layers over one `Value` currency

- **Ergonomic (default).** `Query`/`Exec`/`QueryRow` take native args (`...any` in Go, an
  `IntoValue` bound in Rust, JS values in TS) and a cancellation token (§11.4); the cursor's
  `Scan` converts each column into a native destination.
- **Typed fast path.** Per-type accessors (`Rows.Int(col)`/`Text(col)`/…) read one column with a
  single kind-check, skipping `Scan`'s per-destination type switch — for hot loops over millions
  of rows. (The `Scan` switch is itself only a few ns/column when it avoids reflection and lets the
  destination slice stay non-escaping — but the accessor removes even that.)
- **Raw.** `Rows.Value(col)` and the §2–§5 `Value`/`[]Value` methods stay, for full fidelity
  (exact `decimal` bits, the float-verbatim rule, composite/array/range/json trees).

### 11.2 Argument and Scan conversion

**Args (native → `Value`).** A type switch with an escape hatch (`Valuer`): `nil`→NULL; `bool`;
the integer widths (range-checked into `i64`); `f32`/`f64`; `string`→`text`; bytes→`bytea`;
the host time type→a temporal `Value`; the engine's own `Decimal`/`Interval`/`uuid` types; a bare
`Value` passes through. **The one impedance point is temporal**: Go's single `time.Time` (and the
equivalents) maps to three jed types — it is bound as `timestamptz` and the §5 binder re-coerces to
the inferred column type (`timestamp`/`date`); a host that needs an exact target casts (`$1 ::
date`) or passes a `Value`.

**Scan (`Value` → native `*T`).** The mirror, an **inline type switch with explicit cases for the
common destinations** (`*i64/i32/i16/int`, `*string`, `*bool`, `*f64/f32`, `*[]byte`, the temporal
type, the engine value types, `*any`) — **never reflection on this path**, and the destination must
not escape (so it stays allocation-free). Narrowing integer destinations are range-checked
(`22003`-style). **NULL into a plain scalar destination is an error**; the nullable targets are the
language's idiom plus a generic `Null[T]` (Go) — which implements the `Scanner` hook so it needs no
per-type case — and `*any` (→ the language's null). A `Scanner`/`ScanJed` hook covers custom types.

`Values()` returns a whole row as native values (pgx's `Values` — for callers that don't know the
schema), disambiguating `timestamp`/`timestamptz`/`date` via the column type (§4 `ColumnTypes`).
Rich container types (composite/array/range/json) map to the engine value type for now; a richer
native mapping is a follow-on.

### 11.3 Iterators, single-row, struct mapping

- **Iterators** (Go 1.23+ range-over-func; the equivalent elsewhere). `Rows.All()` yields the
  cursor positioned at each row and **closes it on loop exit** — break, return, panic, or
  exhaustion — eliminating the `defer close` boilerplate and the forget-to-close bug; the terminal
  error is read via `Err()` after the loop (the `bufio.Scanner` idiom, because a single-value
  `iter.Seq[Row]` has no slot for it). A generic `Collect(rows, fn)` yields `(T, error)` for the
  fused case. Iterators are a **layer over** the explicit `Next`/`Scan`/`Err` cursor, not a
  replacement — the explicit form is the documented base because it gives the terminal error a
  clean home. Overhead is negligible for a row loop (one indirect `yield` call per row, no
  per-row allocation when designed right), and aligns with the future streaming cursor (§4/§9),
  where "fetch next, yield it" maps onto pull-per-row with no impedance.
- **Single-row.** `QueryRow(...).Scan(dest…)` runs, scans the first row (ignoring extras, like
  `database/sql`/pgx), closes the cursor, and returns `ErrNoRows` when empty.
- **Struct mapping.** `RowToStructByName[T]` (generics/reflection, **off** the hot path) matches
  column names to `db:"…"`-tagged fields; `RowTo[T]` scans a single-column row into a `T`. Both
  compose with `Collect` for `for row, err := range Collect(rows, RowToStructByName[T])`.

### 11.4 Cancellation (`context.Context` → `57014`)

The ergonomic `Query`/`Exec`/`QueryRow` take a cancellation handle — Go `context.Context`
(first parameter, pgx convention), Rust a cancellation token / `AtomicBool`, TS an `AbortSignal`.
It is captured on the operation and **polled at the existing cost-meter checkpoint** (the single
`Guard()` chokepoint already run at the unbounded-work points for `max_cost`, [cost.md](cost.md)
§6) — so cancellation is **one more condition at a seam that already exists**, not a new
cross-cutting concern. A flipped token aborts deterministically with **`57014 query_canceled`**
(PostgreSQL's exact code for a canceled statement / `statement_timeout`), which **rolls back like
any error** (autocommit write → table untouched; inside a block → poisons it, transactions.md §6),
exactly as the `54P01` cost abort does.

It is **not** in the conformance corpus — whether a cancel lands at row 1k or 5k is timing, hence
nondeterministic — so it is **per-core unit-tested only**, the same treatment benchmarks get
(CLAUDE.md §10). This does not weaken the determinism contract: a cancel yields an *error*, never
wrong rows or a different cost; the contract is about queries that *complete*. Overhead is zero
when the token cannot fire (Go's background context has a nil `Done` channel; skip the poll); for a
live token, a one-goroutine watcher setting an atomic keeps the hot path to a single atomic load.

**Per-core reality** (CLAUDE.md §2). Go and Rust have real threads, so a cancel from another
thread interrupts a running statement at the next checkpoint. **TS cannot preempt synchronous
execution** (one event loop; nothing else runs until the call returns), so an `AbortSignal` there
is honored only at operation boundaries, not mid-statement — a deliberate best-experience-per-
language divergence, not a uniform-shape failure.

The meter `Guard()` is the primary mechanism; the cursor *also* captures the token and re-checks it
in `Next()` — largely redundant over today's materialized result (the meter already aborted the
run), but the forward-compatible hook for the streaming cursor (§4), where `Next()` becomes where
work (and thus cancellation) happens.

### 11.5 Status and naming

**Landed (Go, `impl/go/ergonomic.go`):** the arg/scan conversion, `Query`/`Exec`/`QueryRow` on
**`Database`, `Transaction`, *and* `PreparedStatement`** (the latter with no `sql` parameter — its
SQL is fixed at `Prepare`), `Result` command tag, `Scan`/`Values`/`Err`/`Close`, the typed
accessors, `All()`/`Collect`, `RowTo`/`RowToStructByName`, `Null[T]`, `Valuer`/`Scanner`. The
conversion + cancellation logic lives once (`ergoQuery`/`ergoExec`); the per-type methods are
one-liners over the raw `[]Value` primitives. **Cancellation is wired through the cost meter:** each
operation arms a poll (`engine.armCancel`) on the `sessionState` of the engine that runs the
statement, `sessionState.newMeter` copies it into the statement's meter, and the meter's `Guard()`
checkpoint consults it — so a flipped `context.Context` aborts a long-running statement with the
registered **`57014 query_canceled`** at the next metering point, not only at the cursor/entry
boundary (`ctxErr`). The poll is `nil` (zero-overhead, untouched hot path, the §8 cost determinism
intact) unless a cancelable ctx is active; a non-cancelable background ctx (nil `Done`) arms
nothing. Verified by `impl/go/cancellation_test.go` (the meter-`Guard` unit test, the boundary
abort, and a white-box mid-execution abort). The **`Queryer`** interface (`Query`/`QueryRow`/`Exec`)
is satisfied by `Database` and `Transaction`, so a data-access function written against it runs
unchanged on a handle or inside a transaction (pgx's `Querier`). The raw `[]Value` query method on
each type was renamed **`QueryValues`** so the ergonomic `Query` owns the name (the raw-path
`*Values` convention; `Execute([]Value)` keeps its name since `Exec` doesn't collide).
**Landed (Rust, cancellation — `impl/rust/src/cancel.rs`):** the cancellation half of the mirror.
A public **`CancellationToken`** (a clonable `Arc<AtomicBool>` with `cancel()` / `is_cancelled()`)
is the Rust spelling of Go's `context.Context` cancellation — and *simpler*, because Rust has real
threads (CLAUDE.md §2): the token **is** the shared atomic the Go core spins up a watcher goroutine
to feed, so thread B flips the same token thread A is running under, no watcher needed. It is wired
through the **same cost-meter seam**: `Meter` gains a `cancel: Option<CancellationToken>` checked
**first** in `guard()` (`57014` before the `54P01`/`54P02` cost ceilings), `SessionState` gains a
`cancel` field that `new_meter` copies into every statement's meter, and the cancelable methods —
`execute_cancelable` / `query_cancelable` on **`Session`, `Database`, *and* `Transaction`** — arm a
clone for the statement's duration (a cheap boundary `check()` first, then restore the prior token),
so a flipped token aborts a long-running statement with `57014` at the next metering point, not only
at the boundary. `None` (the default) ⇒ zero overhead, the §8 cost determinism untouched. Verified by
`impl/rust/tests/cancellation.rs` (the `Meter::guard` unit test, the boundary abort on `Session`/
`Database`, the un-cancelled-completes regression, and the transaction roll-back) plus the white-box
mid-scan-via-meter test inline in `src/shared.rs` (it reaches the private session state to bypass the
boundary poll and prove the *meter* aborts the running scan). The Rust **arg/scan ergonomic layer**
has since landed (next paragraph). **Follow-ups, ledgered:** (a) richer native mapping for container
types (today they degrade to the raw `Value` in Go / the canonical text in TS); (b) the same surface
on the shared `ReadHandle`/`WriteHandle` (§2.5); (c) thread the poll deeper so even a non-`Guard`-
metered tight loop (if any) is interruptible — today every unbounded-work point is already a `Guard`
site, so this is belt-and-suspenders. Update the `/web` API pages in the same change as the
ergonomic surface stabilizes across cores (CLAUDE.md §10).

**Landed (Rust, arg/scan — `impl/rust/src/ergonomic.rs`, `rusqlite`-style):** the rusqlite idiom,
**additive** — the raw `&[Value]` `execute`/`query` are unchanged (the FFI wraps and the conformance
harness depend on them), so the ergonomic methods are a *new, non-colliding* set rather than a
generic-ized signature. Three pieces mirror rusqlite's `ToSql`/`FromSql`/`Row`: **`ToValue`** (native
→ `Value`: the int/float primitives, `bool`, `&str`/`String`, byte slices, `Decimal`, `Option<T>` as
the nullable binder, and `Value`/`&Value` as the identity) and **`Params`** (a *set* — `()`, tuples
to 12-arity, `[T; N]`/`&[T]`/`Vec<T>`, so a raw `&[Value]` is still a `Params` via `Value: ToValue`);
**`FromValue`** (column `Value` → native, with `Option<T>` the only NULL-accepting target — a bare
scalar rejects NULL with `22004`, a narrowing overflow is `22003`, a family mismatch `42804`) and
**`Row`** (`get::<T>(idx)` / `get_by_name::<T>(name)` / `value(idx)` for the raw `Value`; a bad
index/name is `42703`). The methods on **`Database`, `Session`, *and* `Transaction`** are `run`
(native params → affected-row count), `query_rows` (→ `Vec<Row>`), `query_map` (map each row → `T`),
and `query_row` (→ `Option<T>`, the idiomatic-Rust "maybe a row" rather than rusqlite's no-rows
error). `run` is the write verb because the raw `execute` name is kept for the `&[Value]` path. A
small macro keeps the three handles' method bodies identical (inherent methods can't share a trait
without shadowing the raw pair). Verified by `impl/rust/tests/ergonomic.rs` (the param shapes, typed
`get`, the `Option`/error-code conversions, `query_map`/`query_row`, and the on-Session/Transaction
surface).

**Landed (TS, cancellation — `impl/ts/src/cancel.ts`):** the cancellation mirror, and the
**deliberate per-language divergence** §11.4 calls out. The handle is the platform **`AbortSignal`**
(an `AbortController`) — the same primitive `fetch` and the web APIs use, so there is *no* custom
token type (Rust ships `CancellationToken` only because JS has one built in). The cancelable methods
`executeCancelable` / `queryCancelable` on **`Session`, `Database`, *and* `Transaction`** take an
optional `AbortSignal` and throw the registered **`57014 query_canceled`** when it is already
aborted. Crucially the check is at the **operation boundary only**, *not* the cost meter: TS runs on
one event loop, so nothing (no timer, no other thread) runs *during* a synchronous `query()`/
`execute()` — the signal's `aborted` state is frozen at entry, so a meter poll would re-read the same
value N times. The meter and `SessionState` are therefore left **untouched** (the §8 cost determinism
is trivially intact — no TS cost path changed), unlike the Go/Rust mid-statement poll. It is still
useful: it skips work for an already-canceled operation (a client that disconnected before the
handler ran). Mid-statement cancellation in TS would need an async streaming cursor that `await`s
(§4); the boundary check is the forward-compatible seam. Verified by
`impl/ts/tests/cancellation.test.ts` (the `throwIfAborted` unit test, the boundary abort on
`Database`/`Session`, the un-aborted-completes regression, the transaction roll-back, and a
boundary-only-semantics test proving an abort after a synchronous query has no retroactive effect).
**Cancellation is now mirrored across all three cores** (Go meter poll via `context.Context`, Rust
meter poll via `CancellationToken`, TS boundary via `AbortSignal`).

**Landed (TS, ergonomic — `impl/ts/src/ergonomic.ts`, better-sqlite3-style):** the better-sqlite3
idiom, **additive** — the raw `Value[]` `execute`/`query` are unchanged. `db.prepare(sql)` (on
**`Database`, `Session`, *and* `Transaction`**) returns a **`Statement`** with `run(...params)` (→ a
`RunResult` `{changes, cost}`), `get(...params)` (→ the first row as an object, or `undefined`),
`all(...params)` (→ object rows), and `*iterate(...params)` (lazy object rows); the same four verbs
exist as one-shot shorthands on the handle (`db.run`/`get`/`all`). Params and columns spread/return
**plain JS values**: a param maps `bigint`→int, an integer-valued `number`→int (so `run(1)` binds an
integer — JS can't tell `1` from `1.0`), other `number`→f64, `boolean`/`string`/`Uint8Array`/`Decimal`
to their types, `null`/`undefined`→NULL, a raw `Value` passes through; a result maps int→**`bigint`**
(i64 is exact — jed's identity), `bool`/`f32`/`f64`/`text`/`bytea` to their JS natives, and every
other type (decimal, uuid, the temporal types, array/range/json/composite) to its **canonical text**
— lossless and predictable, with the raw `query` path kept for the engine `Value` itself (a
structured mapping is a follow-up). Rows are objects keyed by output column name (last wins on a
duplicate). The `Statement` re-parses per call (the parser is cheap; parse caching is a future
optimization), so every run routes through the full session envelope. Verified by
`impl/ts/tests/ergonomic.test.ts` (the affected-count, rows-as-objects + scalar mapping, `iterate`,
the param mapping, the rich-type→text mapping, and the on-Session/Transaction surface).

**All three cores now carry both halves** — cancellation *and* an idiomatic arg/scan ergonomic layer
(Go `database/sql`/pgx, Rust `rusqlite`, TS better-sqlite3).

**Landed (`/web`, the "Queries & parameters" doc page):** with all three cores carrying the ergonomic
surface, the website's per-language `CodeTabs` page is cut (CLAUDE.md §10): `web/src/routes/docs/api/
queries/+page.md` documents binding parameters and reading typed rows, with one idiomatic example per
language under `web/examples/queries/{rust.rs,go.go,ts.ts}` (rusqlite `run`/`query_row`/`query_map`;
`database/sql` `Exec`/`QueryRow`/`Scan` + `RowToStructByName`; better-sqlite3 `prepare`/`run`/`get`/
`iterate`). The page is in the Embedding-API nav and guarded by a `web/e2e/docs.spec.ts` test that the
three language variants each render their idiom. The remaining ledgered work is the container-type
native mapping (follow-up (a) above) and the same ergonomic surface on the shared `ReadHandle`/
`WriteHandle` (§2.5).
