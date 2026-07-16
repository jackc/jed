# @jed/node-rust

Experimental native Node.js package wrapping the safe Rust core. It exists to compare the current
pure TypeScript Node experience with a Rust-backed package before the file-locking implementation
commits Node to native artifacts.

The database engine is `impl/rust`; this package only translates parameters and results over
Node-API. It is therefore a distribution artifact, not a fourth conformance voice. The public
prototype supports file create/open, execute/query, prepared statements, and bigint/string/null
values—the complete surface required by the shared benchmark corpus.

Build and test from the repository root:

```text
rake node:build
rake node:test
```

The experimental package uses exact-pinned `napi-rs` crates and builds `jed_node.node`; no addon is
downloaded or compiled during `npm install`.

Run the full pure-TypeScript versus Node/Rust comparison with:

```text
rake bench:node_compare
```

In the 2026-07-16 run, all 106 paired results agreed on checksums. The wrapper was 1.38× faster by
geometric mean over 49 single-process lanes and 3.76× over four concurrent lanes, but the cheap lanes
were effectively tied and pure TypeScript was 2.04× faster over five write lanes. A same-host native
Rust control brought the checksum-agreeing total to 159 and measured a 2.01× geometric-mean ordinary
Node boundary tax. See `spec/design/benchmarks.md` §7.3; this is deliberately evidence, not a production
package decision.
