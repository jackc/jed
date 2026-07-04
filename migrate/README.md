# jed-migrate

A **small, opt-in schema-migration library** for [jed](../CLAUDE.md), modeled on
[tern](https://github.com/jackc/tern). It is **not part of the engine core** — it links a jed
core in and drives it through the public host API. A jed core built without this directory is
complete, and no core depends on it (the `cli/` + `bench/` precedent, CLAUDE.md §14).

The **language-neutral contract** — the file format, the version-table semantics, and the
migrate algorithm — is [`design.md`](design.md). The Go, Rust, and TS packages are three
independent implementations of it, verified against the shared [`testdata/`](testdata) corpus, so
a migrations directory is portable across all three cores.

## Layout

| Path | What |
|---|---|
| [`design.md`](design.md) | The shared contract (the "why" + the algorithm). Read this first. |
| [`go/`](go) | The Go package — `github.com/jackc/jed/migrate/go` (depends on `impl/go`). |
| [`rust/`](rust) | The Rust crate — `jed-migrate` (depends on `impl/rust`; bundled by the CLI). |
| [`ts/`](ts) | The TS package — runs on bare Node via type-stripping (depends on `impl/ts`). |
| [`testdata/`](testdata) | Shared example migrations (well-formed, irreversible, malformed) driven by all three packages' tests. |

## Migration files

A migrations directory is a flat set of `<sequence>_<name>.sql` files. Each holds an up
migration and an optional down migration, split by a magic line:

```sql
create table users (
  id   bigint primary key,
  name text   not null
);

---- create above / drop below ----

drop table users;
```

Sequence numbers are 1-based and contiguous (`1 … N`); the sequence *is* the version. Omit the
separator (and the down half) for an irreversible migration. Schema state is tracked as a
single-integer high-water mark in a version table (default `schema_version`).

## Quick start

**Go**

```go
db, _ := jed.OpenDatabase("app.jed")
migrations, _ := migrate.LoadMigrations("migrations")
m, _ := migrate.NewMigrator(db, migrations, migrate.Options{})
defer m.Close()
m.Migrate() // bring the database up to the latest version
```

**Rust**

```rust
let db = jed::Database::open("app.jed")?;
let migrations = jed_migrate::load_migrations("migrations".as_ref())?;
let mut m = jed_migrate::Migrator::new(&db, migrations, jed_migrate::Options::default())?;
m.migrate()?;
```

**TypeScript**

```ts
const db = createDatabase({ path: "app.jed" });
const m = new Migrator(db, loadMigrations("migrations"));
try { m.migrate(); } finally { m.close(); }
```

## CLI

The `jed` CLI bundles the Rust crate as the `jed migrate` subcommand:

```
jed migrate app.jed                 # apply every migration (creates app.jed if absent)
jed migrate -d 2 app.jed            # migrate up/down to version 2
jed migrate status app.jed          # current version, target, pending count
jed migrate new add_posts           # scaffold migrations/NNN_add_posts.sql
```

See [`design.md`](design.md) §9 and [`spec/design/cli.md`](../spec/design/cli.md) §3.1.

## Tests

```
rake migrate:test        # Go + Rust + TS package suites (also part of `rake test` / `rake ci`)
```
