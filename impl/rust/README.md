# impl/rust/ — the Rust core

The first native core (CLAUDE.md §2). Manual ownership, no GC, no runtime.

Style (CLAUDE.md §10): **boring, explicit code over clever abstraction.** Resist deep
generics and macro magic. Flat, well-named, single-responsibility modules with small
context footprints.

This core is a **consumer of [../../spec/](../../spec/)**, not an author of it. Type
definitions, comparison/coercion rules, and error codes come from the spec's data tables
(via build-time codegen, CLAUDE.md §5) — never hand-transcribed here.

## Running it

```sh
cargo run --release --bin conformance   # run the shared conformance corpus
cargo test                              # unit + integration tests (tests/)
```

The `conformance` binary is also the corpus **cost re-baseliner** (`--rebaseline`
rewrites each `# cost:` directive to this core's accrued cost); the Go and TS harnesses
stay pure verifiers, so re-running them cross-checks the new costs (CLAUDE.md §8).

> Status: **built and at parity** with the Go and TS cores — passes the shared conformance
> corpus and writes byte-identical on-disk files (`rust == go == ts == ruby`).
