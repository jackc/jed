# jed

An **embeddable, strictly-typed SQL database** — one file, no server, runs anywhere. The
product is a strict, static type system: a value is never silently reinterpreted at runtime.
Storage is a single in-process file (in the spirit of SQLite), and the observable semantics
(NULL logic, comparisons, ordering, exact numerics, errors) follow PostgreSQL closely — the
standing rule is **match PostgreSQL unless there's an overriding reason** ([CLAUDE.md §1](CLAUDE.md)).

> ⚠️ **Status: 0.x public preview.** jed is pre-1.0. Any release may change behavior or
> the **on-disk file format** — there are **no stability or compatibility guarantees yet**,
> and a database file is only guaranteed readable by the jed version that wrote it. See
> [CHANGELOG.md](CHANGELOG.md).

## Try it

A live, in-browser SQL playground (the engine runs entirely client-side in a Web Worker —
nothing is sent to a server) and the docs are at **<https://jackc.github.io/jed/>**.

## Use it from Go

The Go core is pure Go — no cgo, no FFI — so it installs with no native toolchain:

```sh
go get github.com/jackc/jed/impl/go@latest
```

```go
package main

import (
	"fmt"
	"log"

	jed "github.com/jackc/jed/impl/go"
)

func main() {
	// A path creates a single-file database on disk; jed.NewDatabase() is a transient
	// in-memory one. Writes accumulate until an explicit Commit (Close discards uncommitted
	// changes).
	db, err := jed.Create("people.jed", jed.DatabaseOptions{PageSize: jed.DefaultPageSize})
	if err != nil {
		log.Fatal(err)
	}
	defer db.Close()

	if _, err := db.ExecuteSQL("CREATE TABLE person (id i32 PRIMARY KEY, name text NOT NULL)", nil); err != nil {
		log.Fatal(err)
	}
	if _, err := db.ExecuteSQL("INSERT INTO person VALUES (1, 'Ada'), (2, 'Grace')", nil); err != nil {
		log.Fatal(err)
	}
	if err := db.Commit(); err != nil {
		log.Fatal(err)
	}

	rows, err := db.QuerySQL("SELECT name FROM person ORDER BY id", nil)
	if err != nil {
		log.Fatal(err)
	}
	for rows.Next() {
		fmt.Println(rows.Row()[0].Render())
	}
}
```

(The import path's last element is `go`, so Go imports it under an alias — `jed` above. The
Rust core, the `jed` CLI, the npm package, and the Ruby gem are built in this repository but
are **not yet published** to their registries.)

## What makes jed different

- **SQLite's deployment model, PostgreSQL's behavior, a real type system.** Embeddable
  single-file storage like SQLite, observable semantics like PostgreSQL, and a deliberate
  strict, static type system that is stricter than either.
- **Untrusted SQL is safe to run** ([CLAUDE.md §13](CLAUDE.md)). A query supplied by an
  adversary cannot corrupt memory (every core is memory-safe), cannot reach the host (no
  built-in does I/O or escapes the engine), and cannot exhaust resources (a deterministic
  cost meter + ceiling, a per-session cost budget, and a parser depth limit bound the work).
- **No reference implementation.** jed is implemented natively in multiple languages **in
  lockstep**, so every spec ambiguity becomes a failing cross-core test the day it is
  written. The honesty mechanism is divergence under a shared contract, not implementation
  count.

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
