# jed-migrate — design

> A **simple, opt-in** schema-migration library for [jed](../CLAUDE.md). It is **not part of
> the engine core**: it links a jed core in and drives it through the public host API
> ([api.md](../spec/design/api.md), [session.md](../spec/design/session.md)). Modeled on
> [tern](https://github.com/jackc/tern) (same author), with three deliberate removals and a
> handful of jed-native simplifications. This doc is the **language-neutral contract** — the
> file format, the version-table semantics, and the migrate algorithm — that each per-language
> package (`go`, `rust`, `ts`) implements identically, so a project's migrations directory is
> portable across all three cores.

---

## 1. What this is, and what it is not

jed is an **embedded** database (CLAUDE.md §1): a library linked into a host, one database per
file. The overwhelmingly common way to evolve such a database's schema over time is a directory
of ordered SQL migration files applied at application startup. jed-migrate provides exactly that,
kept deliberately small.

It is **modeled on tern** — the file format, the single-integer version table, and the
`migrate` / `migrate --destination N` / `new` / `status` surface will be immediately familiar to
a tern user — with **three deliberate removals** (the requested scope cuts):

1. **jed, not PostgreSQL.** The engine underneath is a jed core, driven through the jed host API,
   not a `pgx` connection to a server. This is the load-bearing difference and it ripples through
   everything below (§5, §6, §9): no wire protocol, no connection string, no SSH tunnel, no
   `pg_advisory_lock`, no schema-qualified table names.
2. **No code-packages feature.** tern's separate `code` workflow for managing reloadable database
   code (functions, views) — `code install` / `compile` / `snapshot`, the `snapshots/` directory,
   `install_snapshot` — is **not** carried over. Migrations are the only concept.
3. **No templating.** tern runs every migration (and its config file) through Go `text/template` +
   Sprig, interpolating `{{.data}}` values and `{{ template "shared/..." }}` includes. jed-migrate
   does **none** of this: **a migration file's bytes are literal SQL**, handed to the engine
   verbatim. No `[data]` section, no `text/template`, no Sprig, no shared-file includes.

Two further simplifications fall out of jed's design (beyond the three requested cuts), and are
worth stating because they remove tern concepts a reader would otherwise look for:

- **No `disable-tx`.** tern's `---- tern: disable-tx ----` magic comment exists because some
  PostgreSQL statements (`CREATE INDEX CONCURRENTLY`, `ALTER TYPE ... ADD VALUE`) cannot run inside
  a transaction. **jed has no such statement** — every DDL and DML change accumulates in the
  single-writer staging buffer and lands atomically at commit (CLAUDE.md §3). So every migration is
  transactional, unconditionally, and there is no escape hatch to spec. (Revisit only if jed ever
  grows a non-transactional operation — none is planned.)
- **No `[ssh-tunnel]`, no connection config.** There is no server to reach. The host either passes
  an already-open `Database` handle (the embedded case) or the CLI opens a database *file* (§9).

**Non-goals for v1** (kept out on purpose, revisited in §11): a per-migration ledger /
checksums / drift detection, out-of-order migration application, migration squashing, and a
`renumber`-style collision helper.

---

## 2. Where it lives, and how it stays opt-in

```
/migrate/                  # THIS library — separate from the engine, opt-in
  design.md                # this doc: the language-neutral contract
  go/                      # the Go package   (depends on impl/go)
  rust/                    # the Rust crate    (depends on impl/rust)
  ts/                      # the TS package    (depends on impl/ts)
  testdata/                # shared example migrations, used by all three test suites
```

- **Separate from `/impl` and `/spec`.** A migration library is a *consumer* of jed, like the CLI
  (`/cli`), the website (`/web`), and the benchmark harnesses (`/bench`). It is **never linked into
  a core**, and **no core ever depends on it**. A jed core built without this directory is complete.
- **A package per implementation, independently versioned & published** — a Go module, a Rust
  crate, an npm package — each depending only on **its own jed core plus that language's standard
  library**. Because this is not an engine core, its dependencies never touch a core manifest (the
  `bench/` / `/cli` precedent, CLAUDE.md §14); the pure-Go-no-cgo rule that binds `impl/go` binds the
  Go package here too, for free (it only calls the Go core's API).
- **This doc is the shared contract, not the code.** jed's own honesty mechanism — one
  language-neutral spec, N downstream implementations (CLAUDE.md §2) — applies in miniature: the file
  format (§4), the version-table semantics (§5), and the algorithm (§6) are authored **once here**,
  and the Go/Rust/TS packages are three independent implementations that must agree. A migrations
  directory authored for the Go migrator must apply **byte-for-byte identically** through the Rust or
  TS migrator against the same starting database — verified by a shared `testdata/` corpus each
  package's tests run (§10). It is *not* part of the engine conformance corpus (that corpus fixes SQL
  semantics, not this library) — it is this library's own cross-package contract.

---

## 3. The two engine primitives it stands on

jed already ships the exact two host-API primitives a migrator needs
([session.md §4](../spec/design/session.md)); jed-migrate is a thin policy layer over them and
introduces **no new engine surface**.

- **`split_statements(sql)`** — a library-level, lexer-aware statement splitter (a top-level core
  export, no `Database`/`Session` needed). It respects string literals, dollar-quotes, and
  `--` / `/* */` comments, so a `;` inside them is never a boundary, and it yields one statement's
  source text at a time. This is how a multi-statement migration file is broken into statements the
  engine (which parses exactly one statement per call) can run.
- **`session.execute_script(sql)`** — split + run each statement in order, discarding result rows,
  returning an `O(1)` `ScriptSummary { statements_run, rows_affected_total, cost }`. Run on an
  **already-open** transaction it **joins** that transaction (no wrapper, no auto-commit — the caller
  owns the boundary); a mid-script error stops the run and leaves the block `Failed` for the caller to
  roll back. Crucially, an explicit `BEGIN`/`COMMIT`/`ROLLBACK` **inside** a script is rejected
  `0A000 feature_not_supported` — which is exactly the guard we want: a migration file must not manage
  its own transactions, because the migrator owns that boundary (§6).

Everything the engine already enforces per statement — `max_sql_length`, the parser depth limit
(`54001`), the cost ceiling (`54P01`), privilege checks (`42501`) — therefore applies to migration
statements **for free**, per statement, with no extra work in this library.

---

## 4. The migration file format (the shared contract)

Identical to tern's, minus templating. **A migrations directory is a flat set of files**:

```
migrations/
  001_create_users.sql
  002_add_posts.sql
  003_add_email_index.sql
```

- **Name:** `<sequence>_<name>.sql`. The `<sequence>` is a decimal integer prefix; `<name>` is a
  free-form human label. The canonical spelling zero-pads to three digits (`%03d`) but any number of
  digits is accepted. The match pattern is `^(\d+)_.+\.sql$`.
- **Sequence numbers are 1-based and contiguous.** The set must be exactly `1, 2, … , N` with no
  gaps and no duplicates. A gap or a duplicate is a **load-time error** (as in tern — a missing or
  duplicated sequence is refused before anything runs). The sequence *is* the version (§5).
- **Up and down in one file, split by a magic separator:**

  ```sql
  create table users (
    id   bigint primary key,
    name text   not null
  );

  ---- create above / drop below ----

  drop table users;
  ```

  The separator line is the exact string `---- create above / drop below ----` (kept verbatim from
  tern for muscle memory; it is itself a valid jed `--` line comment, so it is inert if a file is ever
  fed straight to the engine). Text **before** the separator is the *up* migration; text **after** is
  the *down* migration. The file is split on the **first** occurrence only.
- **Irreversible migration = omit the separator (and the down half).** A file with no separator is
  up-only; attempting to migrate *down* through it is an error (§6). This matches tern.
- **Multi-statement.** Either half may contain many statements separated by `;`; they are split with
  `split_statements` (§3) and run in order within the migration's transaction.
- **Literal SQL, no interpolation.** The bytes are passed to the engine unchanged. There is no
  `{{ }}`, no data section, no includes (removal #3). A `{` in a migration is just a `{`.
- **The up half must be non-empty.** A migration whose up half is only whitespace/comments is a
  load-time error (`no SQL in forward migration step`) — the tern rule, retained; it catches a file
  saved with the separator but no forward SQL.

There is **no** per-file config, no front-matter, and no naming convention beyond the prefix. The
directory *is* the migration set.

---

## 5. The version table

jed-migrate tracks schema state exactly as tern does: a **single-row, single-integer high-water
mark**.

- **Default table name: `schema_version`** (configurable). Note there is **no schema qualifier** —
  jed has no schema namespace (contrast tern's default `public.schema_version`), so the name is a
  bare table name. It may be qualified by an *attached database* name (`reports.schema_version`) if a
  host migrates an attachment, but that is the host's call.
- **Shape:** one table, one row, one column:

  ```sql
  create table schema_version (version integer not null);
  insert into schema_version (version) select 0 where not exists (select 1 from schema_version);
  ```

  `version = 0` means *no migrations applied*; `version = N` means *migrations 1 … N are applied*.
  `integer` (32-bit) is ample — a project will never author two billion migrations.
- **Created on first use.** The migrator ensures the table exists (and is seeded with `0`) before
  reading or writing it, idempotently, in its own committed transaction. Adopting an existing
  populated database is a `set-version` / baseline operation (§11).

**Why a high-water mark and not a per-migration ledger.** The single integer is tern's model and it
is the *simple* choice the brief asks for: it is trivial to read (`select version from
schema_version`), trivial to reason about, and it makes the up/down algorithm a plain integer walk
(§6). Its known limitation — it records only *how far*, not *which* files ran, and cannot detect a
migration file edited after it was applied — is accepted for v1. A richer per-migration ledger with
content checksums (drift detection, out-of-order support) is a deliberate future option (§11), not a
v1 feature; it would be an additive second table, not a change to this one.

---

## 6. The migrate algorithm

The public operation is **"bring the database to a target version,"** identical in shape to tern's
`MigrateTo`.

**Target resolution.** The library API takes an **absolute** target version (an integer in
`0 … N`). The *relative* spellings a user types — `+3`, `-2`, `-+1` (redo), `last` — are resolved to
an absolute target by the **caller** (the CLI, §9), exactly as tern splits this responsibility. `last`
= `N` (the highest sequence present). Migrating to `0` fully reverses every applied migration.

**The walk.** Given `current` (from the version table) and `target`:

1. If `current == target`, do nothing — return success. (This fast path is the common
   already-migrated startup case; it avoids opening a write transaction at all.)
2. Determine direction: `up` if `target > current`, else `down`.
3. Range-check both `current` and `target` against `0 … N`; an out-of-range value is a `BadVersion`
   error (a version table pointing outside the known set means the migrations directory and the
   database disagree — refuse rather than guess).
4. Step one migration at a time toward `target`. **Each step is its own write transaction** and
   commits independently, so an interrupted run leaves the database at a clean, known intermediate
   version (resumable) — the tern property. For each step:
   - **up** into version `v`: run migration `v`'s **up** half; then `update schema_version set
     version = v`.
   - **down** out of version `v` (to `v-1`): if migration `v` is irreversible (no down half), fail
     with `IrreversibleMigration`; otherwise run migration `v`'s **down** half; then `update
     schema_version set version = v-1`.
5. Repeat until `current == target`.

**How a step runs, on jed primitives** (§3). Per step, over a **read-write session**:

```text
session.begin(writable = true)                    # open one explicit transaction
session.execute_script(step.sql)                  # joins the tx; runs every statement in the half,
                                                  #   in order, discarding rows; stops at first error
session.execute("update schema_version set version = $1", [newVersion])
session.commit()                                  # DDL + version bump land atomically, or not at all
```

On any error the transaction is rolled back (the step made no change, the version is unmoved) and
the error is returned wrapped with the migration's name and the failing statement (§8). Because
`execute_script` **joins** the open transaction and rejects in-script transaction control (`0A000`),
the migration's schema change and its version bump are **one atomic unit** — the version table can
never disagree with the schema it describes. A closure wrapper (`db.update(fn)` / `db.view(fn)`,
[api.md §2.2](../spec/design/api.md)) is the idiomatic way to get the begin/commit-or-rollback
bracket per step.

**One transaction per step, not one per run.** This mirrors tern and suits jed: steps are small,
jed commits are cheap (incremental copy-on-write, only dirty pages — CLAUDE.md §9), and per-step
commit gives resumability. An *all-or-nothing across the entire run* mode (wrap every step in one
outer transaction) is a possible option flag (§11) but is **not** the default — a giant migration run
would otherwise hold the single writer and the whole staging buffer for its entire duration.

---

## 7. Loading migrations (the source seam)

The algorithm (§6) needs an **ordered list of `(sequence, name, up, down)`**. *How* that list is
produced is a seam, because embedded jed apps very commonly ship as a **single binary with
migrations compiled in** — not a directory read at runtime.

- **The default source is a filesystem directory** — the tern behavior, and what the CLI uses.
- **An embedded source is first-class**, per language, because it is the dominant embedded-app
  pattern: **Go** `embed.FS` (`//go:embed migrations/*.sql`), **Rust** a build-time include (e.g.
  `include_dir!` or a generated table), **TS** a bundler glob (`import.meta.glob`) or an object map
  of `name → contents`. Each package exposes a loader for its language's idiom; all produce the same
  `(sequence, name, up, down)` list, so the algorithm and the file format are source-agnostic.

Loading validates the contract of §4 (contiguous 1-based sequences, non-empty up half, at most one
separator) and fails before any statement runs.

---

## 8. Errors

Errors are structured and preserve the engine's `EngineError` / `SqlState`
([api.md §7](../spec/design/api.md)) underneath, so a caller can still branch on the SQLSTATE, while
adding migration context:

- **`MigrationError`** — a statement in a migration failed. Wraps the underlying `EngineError` and
  adds the **migration name** and the **failing statement text** (the tern `MigrationPgError`
  analogue, retargeted to jed's error type). Unwraps to the `EngineError`.
- **`IrreversibleMigration`** — a down-migration was requested through a migration that has no down
  half.
- **`BadVersion`** — the requested target, or the version read from the table, is outside `0 … N`.
- **load-time errors** — missing/duplicate sequence number, empty up half, unreadable source.

Under stop-on-first-error (the only mode), a failed step's transaction has already rolled back
(§6), so the database is left at the last cleanly-committed version — never half-applied.

---

## 9. CLI integration

The `jed` CLI ([cli.md](../spec/design/cli.md)) is a Rust host program that already links the Rust
core. It **bundles the Rust migrate crate** (`/migrate/rust`) so migrations are usable
stand-alone, with no separate binary to install — satisfying "the command line tool will bundle it
in as well."

**`migrate` is a subcommand.** Migration operations live under a single reserved verb —
`jed migrate …` — rather than as `--migrate`-style flags. A subcommand reads better for a
distinct mode of operation (it is not "run some SQL against a database" the way `--dump` /
`--import-csv` are; it is a self-contained workflow with its own arguments), and it namespaces the
sub-operations cleanly (§below) so generic words like `new` and `status` never leak to the top level.

**The one grammar change this forces.** The CLI today has **no** subcommands: the first positional is
the DBFILE (`jed app.jed`). Introducing `migrate` reserves **exactly one** first-token word. The rule
stays minimal and predictable:

- If the first token is the literal `migrate`, it is the migration subcommand; everything after it is
  parsed by that subcommand.
- **Any other first token is the DBFILE, exactly as today** — the existing `jed [OPTIONS] [DBFILE]`
  grammar is untouched for every non-`migrate` invocation.
- The only collision is a database file literally named `migrate` (no extension, in the cwd); it is
  reached by qualifying the path (`jed ./migrate`). This is the standard subcommand-vs-path tradeoff
  and the example dbfiles in cli.md already carry a `.jed` extension, so it is a non-issue in practice.

**The subcommand surface** (exact flags to be finalized in cli.md when this lands):

```
jed migrate [-d TARGET] [-m DIR] [--version-table NAME] DBFILE   # apply up/down to TARGET (default: last)
jed migrate status       [-m DIR] [--version-table NAME] DBFILE  # current version, target, pending count
jed migrate new NAME     [-m DIR]                                # scaffold NNN_NAME.sql (next sequence), no DB

  -d, --destination TARGET   integer | +N | -N | -+N | last   (tern's target grammar; default: last)
  -m, --migrations DIR       migrations directory              (default: ./migrations)
      --version-table NAME   override the default `schema_version`
```

- **Bare `jed migrate DBFILE` applies to `last`** — the tern-parity default (`tern migrate`), the
  dominant startup case. `-d`/`--destination` carries tern's relative grammar (`+N` / `-N` / `-+N` /
  `last` / an integer), resolved to an absolute target before calling the library (§6).
- **`status` and `new` are sub-sub-commands of `migrate`**, so the top level gains no generic verbs.
  The `migrate`-subverb vocabulary is fixed and small (`status`, `new`); a first argument after
  `migrate` that is not one of them is treated as the DBFILE for the default apply action.
- **`migrate new` needs no database** — it writes a stub file (the next sequence number, the `----
  create above / drop below ----` separator) into the migrations directory.

Migration output (which migration ran, which direction, the resulting version) prints through the
CLI's normal script-mode channel; errors carry the migration name and SQLSTATE (§8). Exit codes
follow cli.md (`0` success, `1` usage/open error, `2` a migration failed). The interactive TUI is out
of scope for migrations in v1.

---

## 10. Testing

- **Shared `testdata/` corpus.** A directory of example migrations (well-formed, irreversible,
  multi-statement, and deliberately malformed cases) lives once under `/migrate/testdata/` and is
  driven by **all three** packages' test suites — the cross-package contract of §2. The load-time
  validations and the up/down walk must produce identical outcomes in Go, Rust, and TS.
- **Portability check.** Apply the same well-formed set through each package against a fresh
  in-memory jed database and assert the resulting catalog + `schema_version` row match across cores
  (leaning on jed's own cross-core determinism, CLAUDE.md §8).
- **Per-package unit tests** cover the language-specific loaders (§7), the error wrapping (§8), and
  the target-resolution grammar (§9, the CLI's concern).

Because jed is deterministic and in-memory databases need no filesystem, these tests are fast and
hermetic — no live server, unlike tern's PostgreSQL-backed suite.

---

## 11. Open decisions & deferred features

Called out explicitly so they can be settled before or shortly after the first slice:

- **Version model — confirm the high-water mark (§5).** The tern single-integer model is the v1
  default (simple, matches the brief). The alternative — a per-migration **ledger** table recording
  each applied `(sequence, name, checksum, applied_at)` — buys drift detection, out-of-order
  application, and a truthful `status`. It would be an additive second table. Deferred unless wanted
  in v1.
- **Baseline / adopt-an-existing-database.** tern's `override-version` (set the version without
  running migrations, to adopt a database whose schema already matches migration N). Small, useful,
  likely v1.1. Named `set-version` here to avoid tern's wording.
- **All-or-nothing run mode.** An option to wrap the entire run in one transaction rather than one
  per step (§6). Deferred; the per-step default is safer for large runs.
- **A start/redo/`OnStart` hook.** tern exposes an `OnStart` callback (sequence, name, direction,
  SQL) for progress reporting. A minimal callback is cheap to add to the library API; deferred to
  keep the first slice minimal.
- **Renumbering helper.** tern's `renumber start/finish` resolves sequence collisions from parallel
  branches. Nice-to-have tooling; deferred.
- **CLI spelling.** §9 fixes the shape — a `migrate` subcommand with `status` / `new` sub-verbs — but
  the exact flag names and output formatting are settled in [cli.md](../spec/design/cli.md) when the
  CLI slice lands. Introducing the first reserved first-token verb (`migrate`) is itself a small
  cli.md change to record there.
- **Repo bookkeeping when greenlit.** Add a one-line pointer to `/migrate` from the CLAUDE.md §6
  repo-shape list and a TODO.md backlog entry, in the same change that lands the first package.
```
