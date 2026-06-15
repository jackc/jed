# impl/ — the native cores

One natively-implemented engine per language, built from scratch in lockstep. **None is a
reference implementation** (CLAUDE.md §2) — each is a downstream consumer of
[../spec/](../spec/) and ships a thin harness that runs the shared conformance corpus.

## Implementation priority (CLAUDE.md §2)

| Dir | Language | Notes |
|---|---|---|
| [rust/](rust/) | **Rust** | First. Manual ownership, no GC, no runtime. |
| [go/](go/) | **Go** | Second, built in genuine lockstep with Rust. Pure Go — **no cgo, no FFI**. |
| [ts/](ts/) | **JS/TypeScript** | Third — a **native** core (not a Rust→WASM wrapper), runs on modern Node by type-stripping (no build step). Browser/OPFS host comes later. |

Rust, Go, and TS are the differential set: Rust and Go are about as far apart as two
systems languages get, and the native TS core closes the two axes they agreed on by
construction (exact int64 via `bigint`, UTF-8 names, big-endian bytes — CLAUDE.md §2).
Java, C#, and Swift come later, chosen **native or wrapped per language** on
best-experience grounds (CLAUDE.md §2, [../spec/design/cores.md](../spec/design/cores.md)).

The independent **Ruby** reference (`spec/**/verify.rb`) is not a SQL-engine core; it
verifies the byte-exact on-disk format and the spec fixtures, so the round-trip is
`rust == go == ts == ruby` (CLAUDE.md §8).

## Toolchain

Tool versions are pinned in the repo-root [`mise.toml`](../mise.toml): `rust 1.92`,
`go 1.26`, `node 24`, `ruby 4.0`.

> Status: all three cores are **built and at parity** — they pass the shared conformance
> corpus and write byte-identical on-disk files. See each core's README for how to run its
> conformance harness and unit tests.
