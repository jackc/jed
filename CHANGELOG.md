# Changelog

All notable changes to jed are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

> **Versioning during 0.x.** jed is in a public-preview phase. While the major
> version is `0`, **any release may change behavior or the on-disk file format**.
> There is no stability or compatibility guarantee until `1.0`. Treat a database
> file as readable only by the jed version that wrote it.

## [0.3.0] — 2026-11-14

Third public preview, covering the changes since July's v0.2.0. This release breaks
embedding APIs and changes the on-disk format from **25 to 54**. The supported
Rust, Go, and TypeScript cores continue to share the same SQL, cost, and storage
contracts. jed remains a preview, without production or compatibility guarantees.

### Upgrading

- **Do not open a v0.2.0 database with v0.3.0.** There is no automatic file-format
  migration. Preserve the old file and matching binary, export schema/data using
  that version, recreate the database with v0.3.0, and validate the result before
  switching. Test any dump/reload path: the CLI dump is not a complete migration
  tool, including for standalone and serial-owned sequences.
- The browser playground uses the new format too. Export older OPFS files before
  switching versions; an export preserves their bytes, not an upgraded format.
- **Go requires Go 1.26 or later.** Use `CreateDatabase` / `OpenDatabase`,
  context-first `Exec` / `Query`, `Update(ctx, fn)` / `View(ctx, fn)`,
  `Begin(ctx, writable)`, and context-first script APIs. Configured session creation
  returns an error; duration options use `time.Duration`. Normal library builds
  exclude repository hooks; tools use the `jedtooling` build tag. `MapRows` provides
  lazy row mapping. Callback transactions clean up on panic, and cancellation
  retains SQLSTATE `57014` while unwrapping to the context error.
- **TypeScript's public Node and browser APIs are async-only**, with a dedicated
  execution worker, bounded binary result batches, prepared statements, sessions,
  cancellation, and worker-local host-function registration. Synchronous core APIs
  remain internal tooling interfaces. Node file locking needs the first-party
  native lock helper; the browser build does not.
- **Rust row cursors yield `Result` per row**, and database methods take `&self`.
  Check errors while consuming rows, not only when opening the cursor.
- Shared-file access now uses the `protocol-v2` page-service lock bundle. Stop all
  older processes before changing binaries; do not mix incompatible file formats
  or locking protocols on the same database.

### Added — SQL and embedding

- **Schema evolution:** `ALTER TABLE` column/constraint/key changes, identity
  columns, `CREATE TABLE AS`, richer sequence operations, foreign-key referential
  actions and `MATCH FULL`, expression/partial indexes, composite keys, expanded
  GIN indexing, and `REINDEX`.
- **Views:** read-only and automatically updatable views, check options, replacement
  and renames, column defaults, temporary and recursive views, security options,
  and dependency-aware schema changes.
- **Queries and DML:** `MERGE`, `DISTINCT ON`, named windows, broader subquery and CTE
  handling, `RETURNING` row-version aliases, parameter type inference, and
  parameterized `LIMIT` / `OFFSET`.
- **Expressions and types:** `COALESCE`, `GREATEST` / `LEAST`, named/default function
  arguments, PostgreSQL identifier folding and quoted identifiers, Unicode escapes,
  `SIMILAR TO`, richer regex, text, binary, array, JSON and date/time functions,
  datetime formatting/parsing, digests, casts, and temporal precision controls.
- **Host scalar functions**, including batched kernels and persisted index,
  `DEFAULT`, and `CHECK` dependencies, with explicit cost and determinism contracts.
- **Structured diagnostics** with constraint/table/column/type fields, detail,
  position, and hint; expanded SQL catalog introspection.
- **Maintenance APIs:** compaction, integrity checks (also
  `jed check`), and serialization/deserialization of plaintext databases.
- **Multi-process page service** across Rust, Go, and Node, including attachments,
  owner handoff, and remote access over SSH through the source-built CLI.
- **Encryption at rest:** XAES-256-GCM pages, key or Argon2id-passphrase credentials,
  credential changes, key rotation, encrypted scratch files, and browser
  WebCrypto/OPFS support. The security review remains outstanding.
- **Migration tooling:** baseline/set-version, progress callbacks, and migration
  renumbering support.

Snapshot backup machinery is implemented and tested, including encrypted and remote
files, but its public host API and CLI command are not yet defined.

### Changed — planning, resources, and storage

- A **deterministic cost-based planner** selects access paths and bounded join
  orders using exact row counts and explicit `ANALYZE` statistics. `EXPLAIN`
  includes estimates and execution attribution. Prepared plans use current schema,
  statistics, and bindings; predicate pushdown and contradiction detection avoid
  unnecessary work.
- **Resource budgets** now cover scalar amplification (`54P04`), retained query
  rows/operator state/pending writes (`54P05`), and committed storage (`54P06`).
  Native file hosts spill sort, hash join, aggregation, DISTINCT, and temporary
  tables. These remain logical guardrails, not whole-process memory caps; OPFS and
  in-memory hosts do not spill to disk.
- **Validated copy-on-write commits** use one steady-state durable barrier without
  a WAL. `synchronous = off` safely batches durability separately from commit
  visibility. Unchanged transactions avoid persistence, free trailing pages can
  be truncated, and reader-aware reclamation is incremental.
- **Compact storage and reads:** fixed-width stored decimals, compressed leaf
  regions and keys, front-coded GIN postings, deferred interior writes, smaller
  commit manifests, and patched base-plus-offset regions for outliers (format 54).
  Resident leaves are read in place and the cache charges their resident bytes.
- Streaming, columnar aggregates, prepared lookups, regex, JSON processing, and
  large-value handling have substantially less copying and allocation. Gains
  depend on query shape and host; no universal performance comparison is implied.

### Fixed and verified

- Numerous cross-core fixes for numeric/temporal boundaries, coercion, collation,
  Unicode, correlation, constraints, memory accounting, snapshot lifetime, crash
  recovery, and encrypted-page replay.
- Broader shared process, fault-simulation, metamorphic, and real-browser coverage.
- Rust/CLI database creation now accepts a bare relative filename such as `app.jed`.
- Website limitations, durability and memory descriptions, and the Go quickstart
  now reflect the current surface. Project-path links work on GitHub Pages, and
  the playground splits scripts correctly around quoted identifiers. Search links no
  longer duplicate the project prefix. Go/Rust documentation examples compile in CI;
  Rust snippets handle per-row errors and Go snippets release their cursors.

### Distribution

The **Go module** (`github.com/jackc/jed/impl/go`, tag `impl/go/v0.3.0`) and the
**website/playground** remain the shipped artifacts. The repository release tag is
`v0.3.0`. Rust, CLI, npm, Ruby and WASM artifacts remain source builds; this release
does not publish new registry packages. Frozen native-language experiments remain
opt-in and are not promoted to supported cores.

## [0.2.0] — 2026-07-07

Second public preview. Still 0.x, still no compatibility guarantee — and this release
**changes the on-disk file format** (`format_version` 21 → 25), so a database written by
v0.2.0 cannot be opened by v0.1.0, and vice versa. The three cores (Rust, Go, TypeScript)
remain in lockstep: byte-identical on-disk format and identical results, types, errors, and
execution cost.

### Added

- **`varchar(n)` / `string(n)` length limits** — parameterized `text` typmod; over-length
  input errors (`22001`), trailing spaces are truncated to fit.
- **Schema introspection** — the `jed_`-prefixed catalog relations `jed_tables`,
  `jed_columns`, `jed_indexes`, and `jed_constraints`, queryable as ordinary read-only tables.
- **`EXPLAIN` / `EXPLAIN ANALYZE`** — a deterministic plan dump; `ANALYZE` reports real
  execution cost.
- **Attached databases** — a host may `attach` other jed databases (on disk or in-memory)
  under a name and query across them by database-qualified name (`reports.sales`) in one
  consistent snapshot. Attaching is a host-API act, never SQL.
- **`jed-migrate`** — an opt-in, tern-modeled schema-migration library for all three cores,
  bundled by the CLI as the `jed migrate` subcommand.
- **Query planner** — index-nested-loop join; `OR` / `IN`-list merged point lookups;
  secondary-index range scans with multi-column prefix bounds.
- **`fsync = off`** — a create/open handle setting for development and testing.

### Changed — storage (`format_version` 21 → 25)

- PAX column-major leaf layout; a B+tree reshape (records live only in leaves, record-free
  routing interiors); on-disk free-list persistence with continuous within-session page
  reclamation (open no longer walks the whole file). Temporary tables now live on an
  in-memory block store, and in-memory databases are served through the same pager and
  packed-leaf read path as file-backed ones. **Files written by v0.2.0 are not readable by
  v0.1.0.**

### Changed — host / embedding API (breaking for embedders)

- Unified database constructors `create(...)` / `open`; the `exec` and `query` paths are
  unified onto a single `query → Rows` streaming seam; `SharedDb` was renamed to `Database`
  and the session handle unified; `NoFsync` was renamed to `SkipFsync`; the `table_names`
  convenience and the `SHARED` temporary-table surface were removed; the internal `Engine`
  and modules are now private behind a curated public API. Adds ergonomic per-language
  bindings, cross-core statement cancellation (`57014`), and a prepared-statement plan cache
  with streaming.

### Performance

- Packed (block-backed) PAX leaves with touched-column-only decode; a vectorized
  aggregate/filter executor; a true streaming result cursor with lazy record decode; open
  reads only the interior spine (the eager per-table row-count leaf walk is gone); geometric
  file preallocation; a smaller Go `Value`.

### Distribution

- Unchanged from v0.1.0: the **Go module** (`github.com/jackc/jed/impl/go`, pure Go) and the
  **website & playground** are the shipped artifacts. The Rust crate, the `jed` CLI, the npm
  package, and the Ruby gem are built in-repo but are **not yet published** to their registries.

## [0.1.0] — 2026-06-27

First public preview. The engine is implemented natively and in lockstep across
three cores — **Rust**, **Go**, and **TypeScript** — with **no reference
implementation**; a language-neutral spec and conformance corpus is the source of
truth (see [CLAUDE.md](CLAUDE.md) §2). All three cores agree byte-for-byte on the
on-disk format and on every query result, value, type, error, and execution cost.

### Engine

- **Strict, static type system** — `i16`/`i32`/`i64`, exact `decimal`/`numeric`,
  `f32`/`f64`, `text` (with linguistic collation), `boolean`, `bytea`, `uuid`,
  `date`, `timestamp`/`timestamptz`, `interval`, `json`/`jsonb`/`jsonpath`, plus the
  `array`, `range`, and composite (row) type containers. A value is never silently
  reinterpreted at runtime.
- **PostgreSQL behavior by default** — three-valued NULL logic, exact numerics,
  comparison/ordering semantics, and error conditions track PostgreSQL; deliberate
  divergences are documented in the spec.
- **Query surface** — joins, `GROUP BY`/`HAVING`, aggregates (incl. `DISTINCT`,
  `FILTER`, `WITHIN GROUP`, `GROUPING SETS`), window functions, set operations,
  subqueries (correlated, `LATERAL`), CTEs (`WITH RECURSIVE`, writable), set-returning
  functions, `LIKE`/`ILIKE` + linear-time regex, and a broad scalar/SQL-JSON function
  surface.
- **Schema & DML** — `CHECK`/`UNIQUE`/`NOT NULL`/`DEFAULT` (constant + expression)/
  composite `PRIMARY KEY`/`FOREIGN KEY`, secondary B-tree, GIN, and GiST indexes,
  `EXCLUDE` constraints, sequences (`serial`), `RETURNING`, `ON CONFLICT` upsert, and
  temporary tables.
- **Storage** — a single-file, page-backed copy-on-write B-tree with incremental
  commit, free-list page reclamation, a bounded buffer pool, large-value overflow
  chains with transparent LZ4 compression, per-page checksums, and an external
  merge sort that spills to disk (`format_version` 21).
- **Safe to run untrusted SQL** — every core is memory-safe, the built-in function
  surface is pure (no I/O, no host reach), and execution is bounded by a deterministic
  cost meter + ceiling (`54P01`), a per-session lifetime cost budget (`54P02`), a
  parser nesting-depth limit (`54001`), and a per-session capability envelope
  (per-table privileges, `42501`).

### Distribution

- **Go module** — importable as `github.com/jackc/jed/impl/go` (pure Go, no cgo).
- **Website & playground** — docs and a live in-browser SQL playground (the engine
  compiled to run client-side in a Web Worker).

The Rust crate, the `jed` CLI, the npm package, and the Ruby gem are built in-repo
but are **not yet published** to their registries.
