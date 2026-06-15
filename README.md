# jed

An **embedded SQL database** — *SQLite's footprint, PostgreSQL's behavior, and a real
(strict, static) type system.* Single-file storage and in-process embedding from SQLite;
observable semantics (NULL logic, comparisons, ordering, exact numerics, errors) from
PostgreSQL — the standing rule is **match PostgreSQL unless there's an overriding reason**
([CLAUDE.md §1](CLAUDE.md)). Implemented natively in multiple languages in lockstep with
**no reference implementation**.

## Read this first

- **[CLAUDE.md](CLAUDE.md)** — the Project Design Brief. The standing, load-bearing
  record of every architectural decision. Read it before making changes; when a decision
  changes, update it in the same change.
- **[spec/](spec/)** — the **canonical** language-neutral specification and conformance
  corpus. This, not any implementation, is the source of truth (CLAUDE.md §2).

## Repository shape

```
spec/        CANONICAL source of truth — design docs + data tables + conformance corpus
impl/        native cores, one per language, each a downstream consumer of spec/
  rust/      first core — manual ownership, no GC
  go/        second core — pure Go, no cgo/FFI
  ts/        third core — native TypeScript on modern Node (type-stripping, no build step)
```

All three cores agree byte-for-byte (CLAUDE.md §8): the on-disk format round-trip is
`rust == go == ts == ruby`. The TS core is native (not a Rust→WASM wrapper) precisely to
stress the spec on dimensions the systems cores hide — exact int64 (`bigint`), UTF-8 names,
big-endian bytes.

## Build order & current status (CLAUDE.md §11)

1. ✅ **Scaffold** the repo around `spec/`.
2. ✅ **Type-system spec** — scalar set + comparison/coercion matrix as data. *Step-1
   scope: signed integers only* (`int16`/`int32`/`int64`). See [spec/types/](spec/types/)
   and [spec/design/types.md](spec/design/types.md).
3. ✅ **Conformance harness format + first corpus** — sqllogictest-style format, three-axis
   taxonomy (suites / capabilities / profiles), integer corpus. See
   [spec/conformance/](spec/conformance/) and [spec/design/conformance.md](spec/design/conformance.md).
4. ✅ **Storage seam + key-encoding fixtures** — the block-device seam + root-swap commit
   model ([spec/design/storage.md](spec/design/storage.md)); byte-exact integer key-encoding
   vectors ([spec/encoding/](spec/encoding/)). On-disk byte *format* is authored with step 5.
5. ✅ **First vertical slice — "it's alive"** — `CREATE TABLE` / `INSERT` /
   `SELECT ... WHERE pk =` (+ `ORDER BY`, `IS [NOT] NULL`, three-valued logic, `CAST`,
   overflow trap), integer columns only, driven through **both** the Rust
   ([impl/rust/](impl/rust/)) and Go ([impl/go/](impl/go/)) cores against the shared
   corpus — `core`/`casts`/`comparison` profiles green in both.
5b. ✅ **On-disk format + Rust↔Go round-trip** — the single-file byte format
   ([spec/fileformat/format.md](spec/fileformat/format.md)) with byte-exact golden fixtures.
   Both cores read every golden and write byte-identical output — the load-bearing
   cross-core honesty test ([CLAUDE.md §8](CLAUDE.md)), with an independent Ruby reference
   ([spec/fileformat/verify.rb](spec/fileformat/verify.rb)) pinning the goldens. Whole-image
   form for now; incremental commit deferred.
6. ✅ **Row mutation — `UPDATE` / `DELETE`** — integer columns, both cores, against a
   `mutation` conformance profile. `UPDATE` is in-place and two-phase (all-or-nothing:
   every matching row is type-checked before any write); assigning a `PRIMARY KEY` column
   is rejected (`0A000`) so the storage key never changes. No-PK rows use a monotonic
   rowid (reconstructed on load), so `DELETE` then `INSERT` never collides.

### Beyond the first six steps

The six steps above are CLAUDE.md §11's "it's alive" bootstrap. Substantial work has landed
since across **all three cores** (Rust, Go, TS) — forward work is tracked in
**[TODO.md](TODO.md)**. Highlights:

- **Type system** — the full scalar set: `decimal`/`numeric` (exact), `float32`/`float64`,
  `text`, `boolean`, `bytea`, `uuid`, `timestamp`/`timestamptz`, `interval`; arithmetic, `CAST`
  and `::`, typed string literals.
- **Query surface** — `JOIN` (inner/cross/left/right/full), `GROUP BY`/`HAVING`, aggregates,
  `UNION`/`INTERSECT`/`EXCEPT`, subqueries (scalar/`IN`/`EXISTS`, correlated), `IN`/`BETWEEN`/
  `LIKE`/`CASE`, `LIMIT`/`OFFSET`/`DISTINCT`, set-returning `generate_series`, scalar functions.
- **Schema/DML** — `NOT NULL`/`DEFAULT` (constant + expression)/`CHECK`/`UNIQUE`/composite PK,
  secondary indexes (`CREATE`/`DROP INDEX`), `INSERT` column list / multi-row / `INSERT … SELECT`,
  `RETURNING`, explicit transactions (`BEGIN`/`COMMIT`/`ROLLBACK`).
- **Storage** — a page-backed copy-on-write B-tree with incremental commit, page reclamation,
  a bounded buffer pool, large values (overflow chains + LZ4), and per-page checksums
  (`format_version` 8). Plus deterministic cost metering + a `max_cost` ceiling, a `jed` CLI,
  and a benchmark harness vs. PostgreSQL and SQLite.
