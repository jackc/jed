# bench/ — cross-core, cross-engine wall-clock benchmarks

Compares the three jed cores (Rust, Go, TS) against each other and against PostgreSQL
and SQLite on a shared, language-neutral benchmark corpus. **Canonical design:
[spec/design/benchmarks.md](../spec/design/benchmarks.md).**

```
rake bench:setup     # generate benchmark databases (once; fingerprint-gated)
rake bench:run       # run every harness binary, then print the comparison table
rake bench:report    # re-print the newest results
```

- `corpus/` — the shared benchmark + dataset definitions (TOML).
- `go/`, `rust/`, `ts/` — per-language harnesses; **separate modules** with their own
  dependency manifests (PG/SQLite drivers live here, never in `impl/*`). One binary per
  engine/driver variant, all emitting identical JSONL.
- `data/`, `results/` — generated; gitignored.

Wall-clock numbers are environment-relative and deliberately **not** part of `rake ci`
or the conformance contract — but every result carries an answer checksum and
`bench:report` fails on any cross-engine disagreement.
