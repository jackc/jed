<svelte:head>
	<title>Preview status &amp; limitations — jed</title>
	<meta
		name="description"
		content="jed is a 0.x preview: no stability guarantees yet, the on-disk format may change, and it implements a curated, PostgreSQL-modeled subset. What to expect."
	/>
</svelte:head>

# Preview status & limitations

jed is in a **public preview** and is **not ready for production use.** It is feature-rich and
heavily exercised by its **test suites**: three independent cores (Rust, Go, and TypeScript) agree
byte-for-byte on every query result and on the on-disk format, and a large conformance corpus is
checked against PostgreSQL as an oracle. But automated testing is not the same as real-world mileage
— jed has **not** been proven in production use, and bugs that only surface under real workloads
should be expected. **Do not use it as a system of record or for anything you can't afford to lose.**
It has not yet reached a stable release.

## What "0.x preview" means

- **No stability guarantee.** While the version is `0.x`, any release may change SQL behavior, the
  host API, or the **on-disk file format**. A database file is only guaranteed readable by the jed
  version that wrote it.
- **No format migration yet.** There is no automatic on-disk upgrade path between versions. Treat a
  preview database as reproducible or disposable, not as a system of record.
- **Limited distribution.** This release ships the **Go module** (`go get`) and the
  **website/playground**. The Rust crate, the `jed` CLI, the npm package, and the Ruby gem build
  from source but are not yet published. See [Installation](../install/).
- **Performance depends on the workload.** The cost-based planner, columnar aggregate paths,
  compact leaves, and bounded top-k sorts have received substantial optimization. Other query shapes
  still materialize data or fall back to scans; no universal performance parity with another database
  is claimed.

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
- **Stricter in places, by design.** `numeric` input rejects `NaN` and `Infinity` (`22P02`), and
  several conversions PostgreSQL does implicitly require an explicit `CAST` / `::`.

Each intentional divergence is recorded in the relevant design doc in the
[spec](https://github.com/jackc/jed/tree/master/spec).

## Not yet implemented

jed implements a broad surface, but notable gaps remain. As representative (not exhaustive) examples:

- A separate `time` type, enum types, and domains.
- Generated stored columns, materialized views, and views in or over attached databases.
- Deferred constraints, custom range types, and multiranges.
- Ordered `jsonb` primary/index keys and arrays of composites as keys; composite foreign keys.
- Browser/OPFS disk spill. Native file hosts spill sorts, hash joins, aggregates, and `DISTINCT`;
  browser and in-memory hosts keep that work resident. Resource budgets are guardrails, not a heap cap.
- A public online-backup API/CLI command, automatic file-format upgrades, replication, and a
  completed encryption security review.

Temporary and recursive views, keyable composites with keyable fields, `to_char` / `to_timestamp`,
file compaction, and automatic truncation of trailing free pages **are implemented**. See
[types](../sql/types/), [views](../sql/views/), [resource limits](../api/resource-limits/), and
[database maintenance](../api/opening-a-database/#reclaiming-space-compaction).

The authoritative, always-current picture lives in the project's
[spec](https://github.com/jackc/jed/tree/master/spec) and
[TODO.md](https://github.com/jackc/jed/blob/master/TODO.md).
