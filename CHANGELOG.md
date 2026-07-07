# Changelog

All notable changes to jed are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

> **Versioning during 0.x.** jed is in a public-preview phase. While the major
> version is `0`, **any release may change behavior or the on-disk file format**.
> There is no stability or compatibility guarantee until `1.0`. Treat a database
> file as readable only by the jed version that wrote it.

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
