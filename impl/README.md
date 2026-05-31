# impl/ — the native cores

One natively-implemented engine per language, built from scratch in lockstep. **None is a
reference implementation** (CLAUDE.md §2) — each is a downstream consumer of
[../spec/](../spec/) and ships a thin harness that runs the shared conformance corpus.

## Implementation priority (CLAUDE.md §2)

| Dir | Language | Notes |
|---|---|---|
| [rust/](rust/) | **Rust** | First. Manual ownership, no GC, no runtime. |
| [go/](go/) | **Go** | Second, built in genuine lockstep with Rust. Pure Go — **no cgo, no FFI**. |
| ts/ | JS/TypeScript | Later. Must run in the browser. Native TS default; Rust→WASM is a fallback. **TBD.** |

Rust and Go are about as far apart as two systems languages get, so this pair does the
bulk of the cross-implementation honesty work. Java, C#, and Swift come later (Swift may
wrap the Rust core as a deliberate exception).

## Toolchain

Tool versions are pinned in the repo-root [`mise.toml`](../mise.toml) (currently a
placeholder — filled in when the cores are initialized).

> Status: cores are not yet initialized. Project setup (Cargo, Go module) and the first
> code land at CLAUDE.md §11 step 5, the "it's alive" vertical slice.
