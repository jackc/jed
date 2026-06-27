<svelte:head>
	<title>Preview status &amp; limitations — jed</title>
	<meta
		name="description"
		content="jed is a 0.x preview: no stability guarantees yet, the on-disk format may change, and it implements a curated, PostgreSQL-modeled subset. What to expect."
	/>
</svelte:head>

# Preview status & limitations

jed is in a **public preview**. It is feature-rich and extensively tested — three independent cores
(Rust, Go, and TypeScript) agree byte-for-byte on every query result and on the on-disk format, and
a large conformance corpus is checked against PostgreSQL as an oracle — but it has not yet reached a
stable release.

## What "0.x preview" means

- **No stability guarantee.** While the version is `0.x`, any release may change SQL behavior, the
  host API, or the **on-disk file format**. A database file is only guaranteed readable by the jed
  version that wrote it.
- **No format migration yet.** There is no automatic on-disk upgrade path between versions. Treat a
  preview database as reproducible or disposable, not as a system of record.
- **Limited distribution.** This release ships the **Go module** (`go get`) and the
  **website/playground**. The Rust crate, the `jed` CLI, the npm package, and the Ruby gem build
  from source but are not yet published. See [Installation](/docs/install/).

## Deliberate differences from PostgreSQL

jed's standing rule is to **match PostgreSQL unless there's an overriding reason** — but it is not a
drop-in PostgreSQL clone. It implements a **curated subset** of PostgreSQL's surface (no wire
protocol, no `pg_catalog`, no extensions, `DO` blocks, or `COPY … TO`/`pg_read_file` escape
hatches — that curation is part of what keeps untrusted SQL safe). A few behaviors differ on purpose:

- **No roles, users, or in-database `GRANT`.** Authorization is a per-session capability envelope the
  host configures (per-table privileges, an `allow_ddl` gate, cost budgets), enforced by the engine —
  not an in-database permission catalog.
- **`numeric` is always finite.** There is no `NaN` or `Infinity`; division by zero raises `22012`.
- **Sequences are transactional.** `nextval()` rolls back with its transaction — chosen for
  determinism, where PostgreSQL's sequences are deliberately non-transactional.
- **A single writer at a time.** Readers never block except during the brief commit. This is not MVCC:
  there is one committed version plus one writer's pending changes.
- **Stricter in places, by design.** jed uses its own number-literal grammar (hex, digit
  underscores, and `NaN` are rejected with `22P02`), and several conversions PostgreSQL does
  implicitly require an explicit `CAST` / `::`.

Each intentional divergence is recorded in the relevant design doc in the
[spec](https://github.com/jackc/jed/tree/master/spec).

## Not yet implemented

jed implements a broad surface, but notable gaps remain. As representative (not exhaustive) examples:

- Foreign-key referential **actions** (`ON DELETE CASCADE`, `SET NULL`, …) parse but are not yet
  enforced.
- Some date/time surface is pending — `to_char` / `to_timestamp`, `date_part`, `age`, and a separate
  `time` type.
- Spill-to-disk covers `ORDER BY`; the spilling hash join, aggregate, and `DISTINCT` are still in
  progress.
- Composite (row) types cannot yet be a `PRIMARY KEY` or index key.
- A file does not yet shrink back to the OS (dead space is reused internally, but not returned).

The authoritative, always-current picture lives in the project's
[spec](https://github.com/jackc/jed/tree/master/spec) and
[TODO.md](https://github.com/jackc/jed/blob/master/TODO.md).
