<svelte:head>
  <title>v0.3.0 release &amp; upgrade notes — jed</title>
  <meta name="description" content="What's new in jed v0.3.0, breaking API changes, and upgrading from the v0.2.0 preview." />
</svelte:head>

# v0.3.0 release & upgrade notes

**November 14, 2026.** This public preview collects the changes since v0.2.0 in July.
The Go module and this website are the released distribution; other language packages and the CLI
are built from source. jed is still [not ready for production use](../status/).

## What's new

- **More SQL:** `ALTER TABLE`, identity columns, `CREATE TABLE AS`, `MERGE`, `DISTINCT ON`,
  named windows, and richer expression, cast, text, JSON, binary, regex and date/time support.
  [Tables & constraints](../sql/tables/) and [querying](../sql/select/) have runnable examples.
- **Views and indexes:** updatable, temporary and recursive views; check options and view defaults;
  expression/partial indexes, keyable composites, broader GIN indexing and `REINDEX`.
  See [views](../sql/views/) and [indexes](../sql/indexes/).
- **Planning:** cost-based access and join selection, transactional `ANALYZE` statistics, parameter-aware
  prepared plans, predicate pushdown, and richer [`EXPLAIN`](../sql/explain/).
- **Embedding:** host scalar functions and batched kernels, structured errors, an async TypeScript
  worker runtime, and context-aware Go APIs with lazy row mapping.
- **Storage:** multi-process sharing and SSH access, compaction, integrity checking,
  database byte export/import, encrypted files (security review pending), and relaxed durability.
  See [opening and maintaining a database](../api/opening-a-database/).
- **Resource controls:** scalar, query-memory, pending-write and storage budgets, plus spilling sorts,
  joins, aggregates, `DISTINCT` and temporary tables on native file hosts. Browser/OPFS and in-memory
  hosts stay resident. See [resource limits](../api/resource-limits/) for coverage.
- **Performance:** compact leaves, in-place column reads, smaller commits, incremental reclamation,
  and less copying in streaming and aggregate paths. Improvements depend on workload and host.

The [repository changelog](https://github.com/jackc/jed/blob/v0.3.0/CHANGELOG.md) has the fuller release
summary. Rust, Go and TypeScript remain the supported independent cores.

<span id="upgrading-from-v020"></span>

## Upgrading from v0.2.0

**The file format changes from 25 to 54. The two releases cannot open each other's files.** There is
no automatic upgrade or compatibility promise during 0.x. `jed-migrate` manages application schema
changes; it does not convert engine file formats.

Keep your old database and the matching executable. Export the schema and data with that version,
create a new database with v0.3.0, and validate the reconstructed schema, data, and queries before
switching. A binary copy or backup preserves the old format. Test the export/import procedure for
your schema: the CLI dump is not a complete migration tool, including for standalone and
serial-owned sequences. Recreating disposable databases from their source data is often simpler.

The same change affects files retained by the **browser playground**. Export older OPFS files before
moving to the new site, and keep those copies with the matching jed version. Importing an older
file into the new playground does not convert it. The tool does not overwrite a file just because
opening it fails. The playground's file UI currently opens plaintext databases; encrypted OPFS
files require an application using the credential options in the browser API.

### Go

Use **Go 1.26 or later** and pin the preview when upgrading:

```sh
go get github.com/jackc/jed/impl/go@v0.3.0
```

Use `CreateDatabase` / `OpenDatabase` and context-first `Exec`, `Query`, transaction helpers, and
script APIs. Bare database statements autocommit; use `Update(ctx, fn)` for a group of writes.
`Begin(ctx, writable)` takes a context for transaction admission. Configured session constructors
return errors, and duration options use `time.Duration`. `MapRows` maps rows lazily, reporting
iteration errors. See the current [quickstart](../api/opening-a-database/) and
[transactions](../api/transactions/) examples.

### TypeScript and Rust source users

TypeScript's `jed-ts` and `jed-ts/browser` entry points are **async-only**. Each database owns a
worker; await operations and consume async row cursors. Register host functions inside the worker.
Transport byte limits are separate from SQL resource limits, so large collected results may require
streaming or explicit runtime settings. The Node file host requires the first-party lock helper.
These packages remain unpublished to npm.

Rust cursors yield a `Result` for each row; handle errors during iteration. Database methods take
`&self`. The Rust crate remains unpublished to crates.io.

### Shared files

Stop every older process before upgrading. Shared-file access uses the `protocol-v2` lock bundle and
requires compatible binaries. With default synchronous durability, commits flush before returning;
`synchronous = off` batches that flush and permits loss of recent commits after a power failure.
