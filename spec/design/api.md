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
  the accrued `cost`, or a query result carrying column names, rows, and `cost`. Unchanged
  from the pre-API engine.
- **`Rows`** — a cursor over a query result, yielding one row at a time, plus the column
  names and the accrued cost.
- **`EngineError` / `SqlState`** — the structured error surface (errors are data, not prose
  — CLAUDE.md §5). Every operation surfaces these idiomatically.

## 2. Lifecycle

### 2.1 Opening a database

Two file constructors, deliberately split (open ≠ create):

- **`create(path, opts)`** — make a **new** file-backed database. `opts.page_size` (default
  **8192**, the [storage.md](storage.md) §3 default; validated ≥ the format minimum) is
  **locked into the file's meta at creation** and cannot change thereafter. `create` writes
  an initial empty durable image immediately (§3), so the file exists with its page size
  fixed. If the path **already exists**, it is `58P02 duplicate_file` — `create` never
  clobbers.
- **`open(path)`** — open an **existing** file: load it ([../fileformat/format.md](../fileformat/format.md)),
  adopting its recorded `page_size` and `txid`. If the path is **absent**, it is `58P01
  undefined_file` — `open` never creates. A malformed file is `XX001 data_corrupted`; an
  underlying read failure is `58030 io_error`.

In-memory databases use the **existing constructors** (`Database::new()` / `NewDatabase()` /
`new Database()`) — no backing file, default settings, kept verbatim for back-compat (the
conformance harnesses and unit suites use them). An in-memory database never touches the
filesystem.

### 2.2 Commit

**`commit()`** is a *uniform* operation:

- **File-backed** → durably persist the whole current image and increment `txid` (§3).
- **In-memory** → a **no-op success** (there is no path to write). This keeps `commit` a
  single operation across both modes and forward-compatible with the future §3
  staging-buffer transactions, whose `commit`/`rollback` will apply to in-memory databases
  too. It is **not** an error to commit an in-memory database.

Commit is **explicit**. There is no autocommit: a mutation made by `execute` is in the
in-memory state immediately but is not durable until `commit()`. (With no cross-statement
transactions yet — the §3 staging buffer is future — this is the simplest correct model and
the one that the future transaction model extends without a reshape.)

### 2.3 Close

**`close()`** releases the handle. It does **NOT** commit — uncommitted changes since the
last `commit()` are discarded. This is the single most surprising rule and is deliberate:
durability is the caller's explicit decision, never hidden in a destructor (which is
especially error-prone in the GC'd Go/TS cores). `close` is idempotent.

### 2.4 Prepare / execute / query

- **`prepare(sql) -> PreparedStatement`** parses the SQL once (errors like `42601` surface
  here) and returns a reusable handle. (Introspecting a statement's inferred parameter count
  before binding is deferred — the count is enforced at execute time via the `42601`
  count-mismatch check.)
- **`statement.execute(params) -> Outcome`** runs a (possibly mutating) statement and
  returns the materialized outcome. `statement.query(params) -> Rows` runs a query and
  returns a cursor. `params` is empty when the statement has no placeholders.
- One-shot convenience: `db.execute(sql, params)` / `db.query(sql, params)` are sugar for
  prepare-then-run. The pre-API free function `execute(db, sql)` is kept unchanged (zero
  parameters) — the conformance harnesses depend on it.

## 3. Persistence & durability

The on-disk model is **whole-image** ([storage.md](storage.md) §4 step-5b status): a commit
serializes the entire database to one byte image. Incremental copy-on-write stays deferred;
nothing here forecloses it (CLAUDE.md §9).

**Crash-safe commit recipe** (identical across cores):

1. Serialize `to_image(page_size, txid + 1)`.
2. Write the bytes to a **temp file in the same directory** as the target.
3. `fsync` the temp file.
4. **Atomically `rename`** the temp file over the target path.
5. `fsync` the **containing directory** (so the rename itself is durable).
6. Bump `txid`.

At every instant the on-disk path is either the previous complete valid image or the new
complete valid image — never a torn mix — because the new bytes are fully written and
fsync'd to a *separate* file before the atomic rename. A crash before step 4 leaves the old
file intact (the temp is an orphan, ignored on next open); a crash during step 4 resolves
atomically to one name or the other; the loader additionally validates the CRC and the
double-meta slots, so any residual corruption surfaces as `XX001`, never silent bad data.
The directory fsync (step 5) is a no-op on platforms without one (Windows); the target is
SSD/POSIX ([storage.md](storage.md) §1).

`create` uses the same recipe to write its initial empty image (with `txid` starting at 1).
The whole-image writer fills **both** meta slots with the same `txid`; the double-meta
slots are the forward-compatible hook for the future incremental in-place path
([storage.md](storage.md) §4), not needed for whole-image durability.

## 4. Rows and result types

`Rows` iterates over the query's rows **one at a time** and exposes the column names and the
accrued `cost`. The cursor is the seam that keeps the API from hardening a full-residency
assumption (the [storage.md](storage.md) §1 binding rule): today the executor **materializes**
all rows before the cursor walks them, but the caller-visible contract (yield row, then row,
then column metadata) is exactly what a future streaming/pull executor satisfies — so
streaming can land later without changing any caller. True streaming and spill-to-disk
operators are a separate deferred change (CLAUDE.md §9, Phase 6).

`Outcome` is unchanged: a statement result carries `cost`; a query result carries
`column_names`, the materialized `rows`, and `cost`.

## 5. Parameters (`$N`)

A bind parameter is `$` followed by a 1-based decimal index (`$1`, `$2`, …; grammar.md §5,
[../grammar/grammar.ebnf](../grammar/grammar.ebnf)). Parameters are an **API construct**:
the corpus stays literal-only (§conformance.md 1.2), but the parser accepts `$N` anywhere a
primary expression is accepted and as an `INSERT` value slot.

**Typing is by context, statically, before execution.** The engine has a strict static type
system (CLAUDE.md §4); a parameter has no intrinsic type, so it adopts one from its context
— the other operand of a comparison/arithmetic, the target column of an `INSERT`/`UPDATE
SET`, or a `CAST` target. A parameter with **no derivable type** (e.g. a bare `SELECT $1`,
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
| commit | `db.commit() -> Result<()>` | `db.Commit() error` | `commit(db): void` |
| close | `db.close()` + `Drop` | `db.Close() error` | `close(db): void` |
| prepare | `db.prepare(sql) -> Result<PreparedStatement>` | `db.Prepare(sql) (*PreparedStatement, error)` | `prepare(db, sql): PreparedStatement` |
| stmt execute | `stmt.execute(&mut db, &params) -> Result<Outcome>` | `stmt.Execute(params) (Outcome, error)` | `stmt.execute(params): Outcome` |
| stmt query | `stmt.query(&mut db, &params) -> Result<Rows>` | `stmt.Query(params) (*Rows, error)` | `stmt.query(params): Rows` |
| one-shot execute | `db.execute_params(sql, &params)` / free `execute(db, sql)` | `db.ExecuteSQL(sql, params)` / `Execute(db, sql)` | `executeSql(db, sql, params)` / `execute(db, sql)` |
| one-shot query | `db.query_sql(sql, &params) -> Result<Rows>` | `db.QuerySQL(sql, params) (*Rows, error)` | `querySql(db, sql, params): Rows` |
| rows iterate | `impl Iterator<Item = Vec<Value>>` | `for rows.Next() { rows.Row() }` | `for (const row of rows)` |
| rows columns | `rows.column_names()` | `rows.ColumnNames()` | `rows.columnNames` |
| rows cost | `rows.cost()` | `rows.Cost()` | `rows.cost` |

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

## 7. Errors

`EngineError` carries a `SqlState` (the 5-char SQLSTATE, [../errors/registry.toml](../errors/registry.toml))
and a message; `.code()` returns the SQLSTATE. Idiomatic surfacing: Rust `Result<T,
EngineError>`, Go `(T, error)` with a `*EngineError`, TS `throw EngineError`. SQL errors keep
their existing codes; the API adds the host-filesystem class-58 codes (`58P01`/`58P02`/
`58030`, §2.1) and the parameter code `42P18` (§5). The SQLSTATE class (first two chars) is a
stable category (`22` data, `23` integrity, `42` syntax/access, `58` system, `XX` internal).

## 8. Non-goals this slice

- **No cost ceiling.** Cost is metered ([cost.md](cost.md), `Outcome` carries it) but a
  caller-supplied `max_cost` that aborts is deferred (cost.md §6). The shape is kept open: an
  options object on `prepare`/`execute` can carry it later without changing the surface.
- **No streaming rows** — the cursor walks materialized rows (§4).
- **No transactions** — `commit`/`close` are per the single-writer, no-staging model (§2.2);
  `rollback` and multi-statement transactions arrive with the §3 staging buffer.
- **No browser/OPFS host** — the Node `fs` host is built here; the OPFS host is a sibling
  storage host added later ([storage.md](storage.md) §2, CLAUDE.md §9).
- **No low-level direct-access API** — kept open, not built ([storage.md](storage.md) §5).
