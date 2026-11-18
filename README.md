# jed

**Embeddable. Capable. Safe for untrusted SQL.**
One file, no server — types and behavior modeled on PostgreSQL.

jed is an in-process SQL database you link into your program — one database is one file on
disk, in the spirit of SQLite. It pairs a broad SQL feature set and a rich type system with
observable semantics (NULL logic, comparisons, ordering, exact numerics, errors) modeled on
PostgreSQL — the standing rule is **match PostgreSQL unless there's an overriding reason**
([CLAUDE.md §1](CLAUDE.md)).

> ⚠️ **Status: 0.x public preview.** jed is pre-1.0. Any release may change behavior or
> the **on-disk file format** — there are **no stability or compatibility guarantees yet**,
> and a database file is only guaranteed readable by the jed version that wrote it. See
> [CHANGELOG.md](CHANGELOG.md).

## Try it

A live, in-browser SQL playground (the engine runs entirely client-side in a Web Worker —
nothing is sent to a server) and the docs are at **<https://jackc.github.io/jed/>**.

## Use it from Go

Go 1.26 or later is required. The Go core is pure Go — no cgo, no FFI — so it installs with no native toolchain:

```sh
go get github.com/jackc/jed/impl/go@latest
```

```go
package main

import (
	"context"
	"fmt"
	"log"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	// Create and Open return a *Database — the handle you run SQL through. A path gives a
	// single-file database on disk; jed.Create(jed.CreateOptions{}) (no path) is a transient
	// in-memory one. Each bare Exec autocommits durably (it runs on a fresh session); for a
	// multi-statement transaction use db.BeginFunc(ctx, jed.TxOptions{}, ...) or mint a Session.
	db, err := jed.Create(jed.CreateOptions{Path: "people.jed"})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	ctx := context.Background()
	mustExec(db, "CREATE TABLE person (id i32 PRIMARY KEY, name text NOT NULL)")
	mustExec(db, "INSERT INTO person VALUES (1, 'Ada'), (2, 'Grace')")

	// Query returns a row cursor; Exec is for statements that produce no rows.
	rows, err := db.Query(ctx, "SELECT name FROM person ORDER BY id")
	if err != nil {
		log.Fatal(err)
	}
	defer rows.Close()
	for rows.Next() {
		fmt.Println(rows.Row()[0].String())
	}
	if err := rows.Err(); err != nil {
		log.Fatal(err)
	}
}

func mustExec(db *jed.Database, sql string) {
	if _, err := db.Exec(context.Background(), sql); err != nil {
		log.Fatal(err)
	}
}
```

(The import path's last element is `go`, so Go imports it under an alias — `jed` above. The
Rust core, the `jed` CLI, the npm package, and the Ruby gem are built in this repository but
are **not yet published** to their registries.)

## What makes jed different

- **SQLite's deployment model, PostgreSQL's behavior, a full SQL engine.** Embeddable
  single-file storage like SQLite, observable semantics modeled on PostgreSQL, and a broad
  SQL feature set with a rich, strict type system.
- **Resource controls for untrusted SQL** ([CLAUDE.md §13](CLAUDE.md)). Memory-safe cores
  and pure built-ins prevent memory corruption and host access. Deterministic cost budgets,
  parser limits, a finite scalar-allocation allowance, and an opt-in live query-memory budget
  over row buffers, operator state, and pending writes bound their documented surfaces. Committed
  storage and file-page caches have separate limits. These are guardrails, not a process-memory
  cap; see [memory coverage](spec/design/memory.md).
- **No reference implementation.** jed is implemented natively in multiple languages **in
  lockstep**, so every spec ambiguity becomes a failing cross-core test the day it is
  written. The honesty mechanism is divergence under a shared contract, not implementation
  count.

## Development

Building on jed itself: [DEVELOPMENT.md](DEVELOPMENT.md) — prerequisites for native macOS and the
devcontainer, the per-checkout PostgreSQL oracle, and the command set (`mise run ci` and friends).

## Design & internals

- **[CLAUDE.md](CLAUDE.md)** — the Project Design Brief: the standing, load-bearing record
  of every architectural decision.
- **[spec/](spec/)** — the **canonical** language-neutral specification and conformance
  corpus. This, not any implementation, is the source of truth (CLAUDE.md §2).
- **[TODO.md](TODO.md)** — the forward-work backlog.

```
spec/        CANONICAL source of truth — design docs + data tables + conformance corpus
impl/        native cores, one per language, each a downstream consumer of spec/
  rust/      first core — manual ownership, no GC
  go/        second core — pure Go, no cgo/FFI
  ts/        third core — native TypeScript on modern Node (type-stripping, no build step)
web/         the website: static docs + the live in-browser playground
```

All three cores agree byte-for-byte (CLAUDE.md §8): the on-disk format round-trip is
`rust == go == ts == ruby`, and every query result, value, type, error, and execution cost is
identical across cores. The TS core is native (not a Rust→WASM wrapper) precisely to stress
the spec on dimensions the systems cores hide — exact i64 (`bigint`), UTF-8 names, big-endian
bytes.

## License

[MIT](LICENSE) © 2026 Jack Christensen.
