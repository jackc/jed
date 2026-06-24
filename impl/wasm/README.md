# jed-wasm — the Rust core as a WebAssembly module

A `wasm32-wasip1` build of the safe Rust core (`impl/rust`) behind a small **C ABI**. It is a
**host artifact, not a core** (CLAUDE.md §2; `spec/design/cores.md`): the same shape as the Ruby
gem's native extension (`impl/ruby/ext`) — a standalone `cdylib` that *wraps* the safe Rust core,
confines its FFI `unsafe` to pointer marshalling at the boundary, and pulls in **no third-party
dependency** (the only edge is the path-dep on `jed`). Because it *is* the Rust core recompiled, it
**conforms by construction**: every answer is byte-identical to the native cores (the benchmark
harness cross-checks this — see below).

This is the concrete form of the standing note in CLAUDE.md §2 that "a Rust→WASM wrap remains an
acceptable production fallback" for the JS/TS world, and a stepping stone for the Browser/OPFS host
story (`spec/design/hosts.md` §5).

## Why `wasm32-wasip1`

Not `wasm32-unknown-unknown`: WASI gives the core a working `std::fs` (so a `.jed` file opens
through a host **preopen**) and `getrandom` a backend (WASI `random_get`) with **no Cargo feature**,
and Node ships a built-in WASI host (`node:wasi`). The module is a **reactor** (exports
`_initialize`, no `_start`).

## Build

```
rustup target add wasm32-wasip1        # once
cargo build --release --target wasm32-wasip1 --manifest-path impl/wasm/Cargo.toml
# or: rake bench:build   (builds this alongside the other bench engines)
```

Output: `impl/wasm/target/wasm32-wasip1/release/jed_wasm.wasm`.

## The C ABI

`memory`, `jed_alloc`/`jed_dealloc` (host writes inputs into linear memory), and:

| export | purpose |
|---|---|
| `jed_abi_version` | ABI version check |
| `jed_open_memory` / `jed_create(path)` / `jed_open(path, ro)` / `jed_close(db)` | database lifecycle |
| `jed_execute(db, sql)` | one-shot parse+execute (DDL, `BEGIN`/`ROLLBACK`, `count(*)`) |
| `jed_prepare(db, sql)` → stmt | parse once |
| `jed_stmt_query` / `jed_stmt_execute(stmt, db, params, len)` | run the prepared statement (binding `$N`) |
| `jed_stmt_free(stmt)` / `jed_free(buf)` | release a statement / a result buffer |

Every fallible call returns a self-describing little-endian **result buffer** (length header + tag +
payload) the host reads back out of `memory`, then returns with `jed_free`. The wire format and the
bind-parameter encoding are documented at the top of [`src/lib.rs`](src/lib.rs). A query cell's text
is exactly `Value::render()` and a SQL NULL is a flag — the cross-core text contract — so a wasm
result reproduces the byte-identical cross-engine answer.

## Benchmark

The wasm wrap is a benchmark engine (`engine=jed, lang=wasm, variant=wrap`), driven from Node by
[`bench/ts/src/bench-wasm.ts`](../../bench/ts/src/bench-wasm.ts) over `node:wasi`. It runs the same
corpus as the native cores, so `jed/wasm/wrap − jed/ts/core` is the wasm-vs-native-JS comparison and
`jed/wasm/wrap − jed/rust/core` the wasm sandbox + marshalling tax. See
[`spec/design/benchmarks.md`](../../spec/design/benchmarks.md) §7.2.
