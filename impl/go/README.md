# impl/go/ — the Go core

The second native core (CLAUDE.md §2), built in genuine lockstep with the Rust core — the
divergence between the two is the project's honesty mechanism. The maintainer's
daily-driver language.

**Pure Go: no cgo, no FFI.** (A cgo wrapper around the Rust core would defeat the entire
point of an independent implementation.)

Style (CLAUDE.md §10): **boring, explicit code over clever abstraction.** Resist
over-interfacing. Flat, well-named, single-responsibility packages.

This core is a **consumer of [../../spec/](../../spec/)**, not an author of it. Type
definitions, comparison/coercion rules, and error codes come from the spec's data tables
(via build-time codegen, CLAUDE.md §5) — never hand-transcribed here.

## Running it

```sh
go run ./cmd/conformance   # run the shared conformance corpus
go test ./...              # unit tests
```

> Status: **built and at parity** with the Rust and TS cores — passes the shared
> conformance corpus and writes byte-identical on-disk files (`rust == go == ts == ruby`).
