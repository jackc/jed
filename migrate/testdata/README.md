# jed-migrate shared test corpus

These migration directories are the **cross-package contract** ([../design.md](../design.md) §10):
the Go, Rust, and TS packages all drive them and must produce identical outcomes. A directory
authored for one migrator must apply byte-for-byte identically through the others against the same
starting database.

| Directory | What it exercises |
|---|---|
| `blog/` | The well-formed **portability set** — a reversible, multi-statement set (`001` inserts rows, `003` is a `CREATE INDEX` / `DROP INDEX`). Applying `0 → 3 → 0` must round-trip, and the resulting catalog + `schema_version` row must match across cores. |
| `irreversible/` | `001` is reversible; `002` has **no separator** (up-only). Migrating up to `2` works; migrating down through `2` is an `IrreversibleMigration` error. |
| `ignored/` | One real migration (`001_only.sql`) plus files that do **not** match `^(\d+)_.+\.sql$` (`README.md`, `notes.txt`, `001_backup.sql.bak`, `draft_add_thing.sql`). The loader must skip the non-matching files and load exactly one migration. |
| `malformed/gap/` | Sequences `1, 3` — a **gap** (missing `2`). Load-time error. |
| `malformed/duplicate/` | Two files at sequence `1`. Load-time error. |
| `malformed/missing_one/` | Starts at `2` (no `1`). Sequences must be `1 … N`. Load-time error. |
| `malformed/empty_up/` | Separator present but the up half is only whitespace/comments. Load-time error (`no SQL in forward migration step`). |

The malformed cases each live in their own directory because a malformed set is refused at load
time, before anything runs.
