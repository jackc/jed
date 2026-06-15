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

Five concepts. The names below are the *concept*; the idiomatic spelling per core is the
mapping table in §6.

- **`Database`** — the handle. Holds the committed in-memory state plus a persistence
  identity: an optional file `path`, a monotonic commit counter `txid`, and the `page_size`
  the file is serialized with.
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

Two file constructors, deliberately split (open ≠ create):

- **`create(path, opts)`** — make a **new** file-backed database. `opts.page_size` (default
  **8192**, the [storage.md](storage.md) §3 default) is **locked into the file's meta at creation**
  and cannot change thereafter. It must lie in the **valid range `[52, 65536]`** — the format
  minimum (the meta header floor) through `MAX_PAGE_SIZE` (64 KiB; [../fileformat/format.md](../fileformat/format.md)
  *Page model*); a page size below the minimum is `0A000 feature_not_supported` "page size too small"
  and one above the maximum `0A000` "page size too large" (the cap bounds the largest single
  allocation, including against a hostile file — §2.1 *open*). `create` writes an initial empty durable
  image immediately (§3), so the file exists with its page size fixed. If the path **already exists**,
  it is `58P02 duplicate_file` — `create` never clobbers.
- **`open(path, opts?)`** — open an **existing** file: load it ([../fileformat/format.md](../fileformat/format.md)),
  adopting its recorded `page_size` and `txid`. The recorded `page_size` is validated to the same
  `[52, 65536]` range as `create` (above); a value outside it is `XX001 data_corrupted`, so a corrupt
  or hostile file cannot force a multi-gigabyte allocation before its contents are even checked. If the
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
knob. A read-only gauge, **`resident_leaves`** (`0` for an in-memory database), reports how many leaf
pages are currently resident — `≤ cache_leaves` by construction. An in-memory database ignores the
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

In-memory databases use the **existing constructors** (`Database::new()` / `NewDatabase()` /
`new Database()`) — no backing file, default settings, kept verbatim for back-compat (the
conformance harnesses and unit suites use them). An in-memory database never touches the
filesystem.

### 2.2 Transactions, autocommit, and durability

The full transaction model is [transactions.md](transactions.md); this section fixes the API
shape. **jed autocommits by default** (PostgreSQL behavior — CLAUDE.md §1; this **supersedes**
the original "no autocommit" rule, which was an accident of the whole-image writer —
transactions.md §1). The commit boundary and durability are **decoupled** (transactions.md §9):

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
- **`commit` / `rollback` are uniform across modes.** **In-memory** → commit is a **no-op
  success** (no path to write; never an error); rollback discards the working set. **File-backed**
  → commit publishes + makes durable per the **`synchronous`** setting (below).
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

### 2.5 Shared handle: parallel readers + a single writer (P5.3b)

The handle of §2.1–§2.4 is single-threaded: it is the simple, fast path and is **not** safe to
share across threads. For **concurrent readers running alongside a writer** there is a separate
**shared handle** (`SharedDb` — Rust/Go/TS), the faithful realization of the §3 model
(transactions.md §10). It is cheap to clone/share, and mints two kinds of per-caller handle:

- **`db.read() -> ReadHandle`** pins the committed snapshot *now* and serves reads from that one
  stable, immutable version for its life — never blocked by, and never blocking, a writer. A
  write attempted through it is `25006`. It registers in the live-reader set (transactions.md §8);
  **`read.close()`** (Go/TS — no destructor) / dropping it (Rust) deregisters, advancing the
  watermark. `db.oldest_live_txid()` reports the oldest version any open reader still pins.
- **`db.write() -> WriteHandle`** opens the single writer: it captures the committed snapshot as a
  private working set, runs statements with full transaction semantics (read-your-writes, failed-
  block poisoning), and **`commit()`** publishes the working set at the next version (the §3
  commit window) / **`rollback()`** discards it. At most one writer is open at a time — a second
  `write()` **blocks** until the first ends (Rust/Go) or is **rejected `25001`** (TS, which cannot
  block its one thread).

**Per-core reality** (CLAUDE.md §2 — best experience per language): Rust and Go give true
OS-thread parallelism (reader threads run while a writer commits); TS gives snapshot **isolation**
across async interleavings (no shared-memory threads). This slice's shared handle is **in-memory**;
file-backed sharing reuses the §3 publish point + the §9 persist chokepoint and is wired later.
The single-handle surface (§2.1–§2.4) is unchanged and remains the default.

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
assumption (the [storage.md](storage.md) §1 binding rule): today the executor **materializes**
all rows before the cursor walks them, but the caller-visible contract (yield row, then row,
then column metadata) is exactly what a future streaming/pull executor satisfies — so
streaming can land later without changing any caller. True streaming and spill-to-disk
operators are landing operator-by-operator ([spill.md](spill.md)): the `ORDER BY` external
merge sort + its streaming single-table feed have landed; the spilling hash aggregate /
`DISTINCT` / hash JOIN are deferred follow-ons (CLAUDE.md §9, Phase 6).

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
index (`int16` here, `text` there) are `42804 datatype_mismatch`. Two adaptable operands
with no anchoring type (`$1 = $2`, `$1 = 5`) default the parameter to `int64`, matching the
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
| create file | `Database::create(path, opts) -> Result<Database>` | `Create(path, opts) (*Database, error)` | `create(path, opts): Database` |
| open file | `Database::open(path) -> Result<Database>` | `Open(path) (*Database, error)` | `open(path): Database` |
| open in-memory | `Database::new()` | `NewDatabase()` | `new Database()` |
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
| rows affected (§4) | `Outcome::Statement { rows_affected: Option<i64>, .. }` | `outcome.RowsAffected, outcome.HasRowsAffected` | `outcome.rowsAffected: number \| null` |
| set cost ceiling (§8) | `db.set_max_cost(limit)` | `db.SetMaxCost(limit)` | `db.setMaxCost(limit)` |
| inject random source (§10) | `db.set_random_source(f)` / `db.clear_random_source()` | `db.SetRandomSource(f)` / `db.ClearRandomSource()` | `db.setRandomSource(f)` / `db.clearRandomSource()` |
| inject clock source (§10) | `db.set_clock_source(f)` / `db.clear_clock_source()` | `db.SetClockSource(f)` / `db.ClearClockSource()` | `db.setClockSource(f)` / `db.clearClockSource()` |
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

**Catalog reads** (the last two rows) are the host's introspection surface until an SQL-level
one exists (an `information_schema`-like layer is a possible later feature): both read the
**currently-visible snapshot** (an open transaction's working set, else the committed state).
`table` returns the full definition — columns (name, type, typmod, NOT NULL, PK membership,
default), the primary key's ordinals in key order, CHECK constraints, and secondary indexes.
`table_names` returns every table's **canonical** (CREATE TABLE-spelled) name, sorted
ascending by **lowercased** name — the catalog's standing order, so no hash-map iteration
order leaks (CLAUDE.md §8). Secondary indexes are relations but not tables; they are excluded.

## 7. Errors

`EngineError` carries a `SqlState` (the 5-char SQLSTATE, [../errors/registry.toml](../errors/registry.toml))
and a message; `.code()` returns the SQLSTATE. Idiomatic surfacing: Rust `Result<T,
EngineError>`, Go `(T, error)` with a `*EngineError`, TS `throw EngineError`. SQL errors keep
their existing codes; the API adds the host-filesystem class-58 codes (`58P01`/`58P02`/
`58030`, §2.1), the parameter code `42P18` (§5), and the transaction-state class-25 codes
(`25001`/`25006`/`25P02`, transactions.md §6). The SQLSTATE class (first two chars) is a stable
category (`22` data, `23` integrity, `25` transaction state, `42` syntax/access, `58` system,
`XX` internal).

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

## 9. Non-goals this slice

- **No streaming rows at the cursor** — the cursor still walks materialized rows (§4). (The
  *operators* are being bounded internally — the `ORDER BY` external merge sort spills to disk
  under `work_mem` ([spill.md](spill.md)) — but the `Rows` cursor itself is fed a materialized
  result; a fully pull-based cursor is the further step.)
- **Transactions are IN, not a non-goal.** The §3 staging buffer, autocommit, the `Transaction`
  surface (`begin`/`view`/`update`), the `synchronous` durability setting, and SQL
  `BEGIN`/`COMMIT`/`ROLLBACK` are specified in [transactions.md](transactions.md) and **landed in
  Phase 5**; §2.2–§2.3 above are revised accordingly (autocommit replaces the original "no
  autocommit" rule; `close` no longer drops committed work). What stays deferred is only
  `SAVEPOINT`/nested transactions, `synchronous=off` batching, and group-commit (transactions.md
  §11).
- **No browser/OPFS host** — the Node `fs` host is built here; the OPFS host is a sibling
  storage host added later ([storage.md](storage.md) §2, CLAUDE.md §9).
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
