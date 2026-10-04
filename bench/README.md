# bench/ — cross-core, cross-engine wall-clock benchmarks

Compares the three jed cores (Rust, Go, TS) against each other and against PostgreSQL
and SQLite on a shared, language-neutral benchmark corpus. **Canonical design:
[spec/design/benchmarks.md](../spec/design/benchmarks.md).**

`mise run bench:spill` runs the separate Linux spill workload from
`corpus/spill.toml`: JOIN, GROUP BY, DISTINCT and ordered-set aggregation over 32,768 wide rows with a
1 MiB `work_mem` and page cache. Each query runs in a fresh process, drains its
cursor, and records elapsed time, peak RSS, result checksum, and deterministic cost.
The driver compares unlimited, forced-spill and default-budget execution across all three cores
and fails if their results or costs differ. Setup is excluded from query memory
and timings. Input exceeds the operator budget; this does not assert that it
exceeds physical machine RAM or that `work_mem` caps process RSS.
Use `rake 'bench:spill[rows,payload_bytes,work_mem]'` to change scale; raw results
and workload metadata go into `bench/results/spill-<timestamp>/`.

```
rake bench:setup     # generate benchmark databases (once; fingerprint-gated)
rake bench:run       # run every harness binary, then print the comparison table + HTML
rake bench:report    # re-print the newest results
rake bench:html      # static HTML report (bars, multipliers, Δ vs the previous run)
rake bench:markdown  # the same report as Markdown (terminal / VS Code preview)
rake bench:diff      # machine-readable JSONL diff of two runs (newest vs previous)
```

- `corpus/` — the shared benchmark + dataset definitions (TOML).
- `go/`, `rust/`, `ts/` — per-language harnesses; **separate modules** with their own
  dependency manifests (PG/SQLite drivers live here, never in `impl/*`). One binary per
  engine/driver variant, all emitting identical JSONL.
- `ruby/` — the **Ruby-gem overhead** harness (`jed/ruby/wrap`): the same corpus through the
  gem, so its delta vs `jed/rust/core` is the binding tax (FFI + marshalling + coercion). Reuses
  the shared PRNG + checksum; no new dependency. See [benchmarks.md §7.1](../spec/design/benchmarks.md)
  and [ruby/README.md](ruby/README.md).
- `data/`, `results/` — generated; gitignored.

Wall-clock numbers are environment-relative and deliberately **not** part of `rake ci`
or the conformance contract — but every result carries an answer checksum and
`bench:report` fails on any cross-engine disagreement.
