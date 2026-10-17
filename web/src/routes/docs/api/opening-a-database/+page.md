<script>
	import CodeTabs from '$lib/components/CodeTabs.svelte';
</script>

<svelte:head>
	<title>Opening a database — jed</title>
	<meta name="description" content="Open or create a single-file jed database from Rust, Go, or TypeScript." />
</svelte:head>

# Opening a database

A jed database is a single file on disk. Open or create one, run SQL against it, and commit when
you're done. Pass a path for a durable file, or open a transient **in-memory** database for tests
and scratch work.

Opening or creating returns a **`Database`** — the handle you run SQL through. Its `execute`, `query`,
`executeScript`, and the `update` / `view` transaction helpers each run on a **fresh session** and
commit it, so a bare statement autocommits. For durable per-connection state — a transaction spanning
several calls, session variables, or a configured/untrusted caller — mint a separate **session** from
the same handle (see [Authorization](../authorization/) and
[Resource limits](../resource-limits/)).

Use the **language selector** in the top bar to switch this example between Rust, Go, and
TypeScript.

<CodeTabs topic="open-database" />

## Durability

A bare `execute` **autocommits durably**: it runs on a fresh session that commits before the call
returns, so the new state is on disk (an in-memory database has nothing to flush). To apply several
statements **atomically**, run them in one `update` closure — or on a single session's explicit
`begin` / `commit` block, where a `rollback` (or dropping the session) discards the uncommitted work.

jed keeps durable data in one file without a WAL. Commits write changed pages once, record their
checksums with the new root, and finish with one durable flush in the steady state. Opening validates
the latest commit before adopting it; file growth and resuming writes after recovery can require
additional flushes. All processes sharing a file must use a jed that reads the same format version.
After a storage write or flush error, close and reopen the handle before writing again so recovery
can determine the committed outcome. An encoding or size error before storage writes does not
require reopening; discard the failed transaction and continue using the handle.

### Relaxed durability

If losing the last few commits in a power failure is acceptable but a damaged file is not, open or
create the database with `synchronous` off: `synchronous: Synchronous::Off` (Rust),
`Synchronous: jed.SynchronousOff` (Go), or `{ synchronous: "off" }` (TypeScript, including the
browser's OPFS `create` / `open`). A commit then returns as soon as its pages are written, and a single
flush later makes a whole run of commits durable together — when the run fills its batch, and when
you close the database. A power loss or OS crash can lose the commits made since the last flush, but
the file always reopens to one complete committed state, never a mix; a crash of your process alone
loses nothing it committed. Query results and costs are the same in both modes.

`close` reports a flush that failed. After any failed flush the handle refuses further reads and
writes until you reopen it. There is no timed flush yet, so a relaxed database that sits idle stays
unflushed until its next batch fills or it closes. This is different from `skip_fsync`, which skips
the flushes entirely and can leave a corrupted file after an OS crash: use it only for scratch data.

## Opening a file from a newer jed

A newer jed can store SQL that an older one cannot parse: a view, a `CHECK`, an expression
`DEFAULT`, or an expression or partial index written with syntax added since. The older jed still
opens the file and runs everything else. Only a statement that needs such an object fails, with
`XX003`: reading the view (and `jed_columns`, which plans every view), a write that would evaluate
the check or apply the default, a write to a table with such an index, and `ALTER TABLE` actions
that would rewrite the stored text. Reads skip the index and return the same rows. Each object can
still be dropped, along with its table, and a view can be redefined with `CREATE OR REPLACE VIEW`.
The stored text is never changed, so the newer jed reads the original definition back.

## Sharing a file between processes

Several Rust, Go, or Node processes may open the same local file at once. The first to open it
becomes its **owner**: only the owner reads and writes the file, and it serves the database to every
later opener over a local socket. Each of those processes is a **page client** that runs its own
engine, reading pages from the owner and sending its commits to the owner. Results, plans, and query
costs are the same either way, readers keep stable snapshots, and one writer at a time commits. The
owner's own queries and commits add no lock calls or extra metadata reads. When the owner closes or
exits, a client takes over as owner and the others carry on.

The open/create `locking` option chooses this. `auto` (the default) uses the page service on hosts
that support it (Unix local files) and behaves like `exclusive` elsewhere (Windows, for now).
`exclusive` owns the file without serving it, so another process's open waits and then fails with
`55006`. `none` skips coordination and is only for when an external coordinator provides the same
safety. `file_lock_timeout_ms` / `FileLockTimeoutMs` / `fileLockTimeoutMs` bounds how long an open
waits to own the file or reach its owner (default 5 seconds); the separate session
`lock_timeout_ms` / `LockTimeoutMs` / `lockTimeoutMs` bounds writer waiting and reports `55P03`.
An attached file uses the page service too: the first process to attach (or open) it owns and serves
it, and any other process that attaches it reads and commits through that owner, taking over when it
leaves. A write transaction takes the writer lock of the main database and of every read-write
attached file, in the same order in every process, so writers in processes that share several files
wait in line rather than deadlock; each wait is bounded by `lock_timeout_ms` (`55P03`). One
transaction still writes at most one file-backed database.

Node uses a small first-party native helper solely for OS file locks because Node has no standard
`flock`/`LockFileEx` API. SQL and storage still run in the independent TypeScript engine; browser/OPFS
builds do not load the helper. A missing platform artifact fails closed instead of using PID or mtime
leases.

## Opening a database on another machine

The same page service runs over SSH, with no server to install and no port to open. The `jed` CLI
opens `ssh://[user@]host[:port]/path`:

```sh
jed ssh://app@db1/srv/app.jed -c "SELECT count(*) FROM orders"
jed --readonly ssh://db1/~/app.jed --dump > app.sql
```

It runs your `ssh` as `ssh app@db1 jed pages /srv/app.jed`, so the remote host needs `jed` on its
`PATH`. Your engine runs the queries, fetching pages over SSH (a scan fetches up to 64 pages per
round trip), and commits go to whichever process owns the file there. If an application on that
host has the file open, `jed pages` passes the connection to it. If nothing does, `jed pages` opens
the file itself and hands it over when an application opens it. `--readonly` asks for a read-only
connection. On the server, `command="jed pages --read-only /srv/app.jed"` in `authorized_keys`
limits a key to reading that one database. SSH handles authentication and encryption, using your ssh
configuration.

In Rust, `Database::open_remote(command, name, options)` opens a database through any command whose
stdin and stdout carry the connection (`["ssh", "--", "db1", "jed pages '/srv/app.jed'"]`).
`Database::serve_page_stream` implements the serving side. Go and Node have no remote client yet.

## Reclaiming space (compaction)

jed reuses the pages that deletes and updates free. When the free pages are at the end of a file, a
later commit cuts them off and the file gets shorter by itself, but free pages in the middle of the
file stay there, so after a big delete the file can stay close to its peak size. To give all of that
space back, compact the database:
`db.compact("main")` (Rust), `db.Compact("main")` (Go), or `db.compact("main")` (TypeScript). Pass
an attachment's name to compact that database instead. The name is matched exactly, as given when
the database was attached.

Compaction rewrites the committed data as a fresh, contiguous file next to the original
(`<path>.jedtmp`), syncs it, and atomically renames it into place, so a crash at any point leaves
either the old file or the new one, never a mix. Afterwards the file is exactly as large as its live
data. The database version advances by one; rows, query results, query costs, and prepared
statements are unchanged. An in-memory database compacts too, which lowers its `storage_bytes`.

Compaction is an explicit maintenance step and never waits. It fails with `55006` while anything
else is using the database — an open write transaction, a read session or open cursor on the
handle, or a read in progress on a page client — so close those and retry. Only the owner can
compact; a page client's `compact` also fails with `55006`. A read-only database is
`25006`, and the browser (OPFS) host does not support compaction yet (`0A000`). The rewrite needs
free disk space for a second copy of the live data, and keeps the file's permission bits but not
its owner.

## In-memory databases

Every example on the **SQL** pages of these docs runs against an in-memory database, right in your
browser — the same engine, no file. Create one by calling the unified create constructor with no path:
`Database::create(CreateOptions::default())` (Rust), `jed.CreateDatabase(jed.CreateOptions{})` (Go), or
`createDatabase({})` (TypeScript).

## The environment

Everything a database takes from outside the engine, except its storage, arrives in one immutable
**`Environment`** passed in the open/create options: the Unicode collation and casing data, the
time-zone data, [host functions](../host-functions/), and the random and clock sources behind
`uuidv4` (and its alias `gen_random_uuid`)/`uuidv7` and `now()`. Build it once with a builder and pass the same environment to every
database that should share those inputs; omit it and the database gets a fresh, empty one (`C`
collation and ASCII casing, `UTC` and fixed offsets, no host functions, OS randomness, the wall
clock).

```rust
let env = jed::Environment::builder()
    .load_unicode_data(&std::fs::read("unicode.jucd")?)?
    .load_time_zone_data(&std::fs::read("tzdata.jtz")?)?
    .build();
let db = Database::open_with_options(path, OpenOptions { environment: env.clone(), ..Default::default() })?;
```

An environment cannot change once built, so a database's inputs never shift while it is open, and two
libraries or tests in one process cannot interfere through it. To vary one input, derive a new
environment with `env.to_builder()` (Go `env.ToBuilder()`, TypeScript `env.toBuilder()`), which shares
everything you don't replace. For reproducible tests, install a seeded random source and a manual
clock, keep a reference to them, and reseed or set them between cases.

## Running untrusted queries

jed's built-in surface is pure: SQL cannot access the filesystem, network, processes, or
clock beyond the sanctioned host seam. Configure the session's work and input limits before
serving untrusted SQL. The scalar allocation budget below covers specific expanding kernels;
[resource limits](../resource-limits/) explains the remaining memory coverage.

- **Cost ceiling — `set_max_cost(limit)`** / `SetMaxCost` / `setMaxCost`. Bounds the deterministic
  *execution* cost; a query that reaches the ceiling aborts with `54P01`. `0` (the default) is
  unlimited.
- **Scalar allocation — `set_max_scalar_bytes(bytes)`** / `SetMaxScalarBytes` / `setMaxScalarBytes`.
  Limits cumulative repeat/padding output and decimal-transcendental scratch reservations per
  statement; default **64 MiB**, non-positive restores the default, over-budget fails `54P04`.
- **Query memory — `set_max_query_memory_bytes(bytes)`** / `SetMaxQueryMemoryBytes` /
  `setMaxQueryMemoryBytes`. Bounds the logical bytes of rows a statement holds at once (row
  buffers and result collectors); **unlimited** by default, non-positive restores unlimited,
  over-budget fails `54P05`.
- **Input size — `set_max_sql_length(bytes)`** / `SetMaxSQLLength` / `setMaxSqlLength`. Bounds the
  *input SQL length* (in bytes), rejecting an over-long statement with `54000` before it is parsed —
  so a giant query can't exhaust parse memory. The default is **1 MiB**; `0` is unlimited. Because
  jed parses one statement per call, this also bounds the parse tree's size (a million-column
  `SELECT` is just bytes).

Five further limits are fixed engine constants (no configuration): a statement may not nest
expressions/subqueries more than **256** deep (`54001`; a view's query counts as if it were written
as a subquery where the view is read), a single identifier may not exceed
**63 bytes** (`42622`), a composite type may not nest more than **32** composites deep
(`54001` at `CREATE TYPE` — a chain of small `CREATE TYPE`s that the input-size cap can't see),
and a `json`/`jsonb` document or `jsonpath` may not nest more than **256** levels deep (`54001`
when it is parsed or built — a short string such as `repeat('[', 100000)` can't overflow the
stack), and one view reference may not expand to more than **1024** views (`54001` — views that
each read the previous view twice would otherwise double at every level). Each limit is
deterministic and identical across the Rust, Go, and TypeScript cores.
