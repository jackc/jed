# bench/ruby — Ruby-gem binding overhead

Measures the **overhead of the jed Ruby gem** by running the shared benchmark corpus through the
gem (`engine=jed, lang=ruby, variant=wrap`) and comparing against the Rust core (`jed/rust/core`).
Both drive the **identical Rust engine** on the same data, so the per-bench `ns_per_op` **delta**
is the binding tax — the FFI round-trip, result marshalling, value coercion, and Ruby object
allocation. Canonical design: [spec/design/benchmarks.md §7.1](../../spec/design/benchmarks.md).

```sh
rake bench:setup     # generate the databases once (heavy: 1M rows) — same datasets the core uses
rake bench:run       # runs every harness incl. jed/ruby/wrap; bench:report shows the columns
rake "bench:run[point_lookup]"   # filter to the cheap, overhead-revealing benches
```

In the report, read the overhead off the `jed/ruby/wrap` column minus the `jed/rust/core` column,
row by row. The **cheap, hot, small-result** benches (`point_lookup_pk`, `secondary_lookup`) are
where the wrapper tax is a meaningful fraction; big scans/aggregates are dominated by engine work
and hide it.

## What's measured (and a caveat)

- The **answer checksum** must match the core's — a correctness gate proving the gem returns
  byte-identical rows (the same engine).
- The harness also prints **allocations/op** to stderr — a *deterministic* overhead metric (unlike
  wall-clock), so it pins exactly how many Ruby objects each call costs.
- **Caveat — per-call parse is included.** The gem has no prepared-statement API, so the bench
  re-parses the SQL on every call (`db.query(sql, …)`), whereas the core's `prepare` parses once.
  That per-call parse is part of the measured delta, so this is "cost of using the gem as it exists
  today," not the pure FFI tax. A gem prepared-statement API (a follow-on) would isolate it.

## Files

- `lib/bench.rb` — the shared harness (splitmix64 PRNG, FNV-1a checksum, corpus/dataset parsing,
  fingerprint gate, run loop), a faithful port of `bench/rust/src/lib.rs`.
- `bench_jed.rb` — the gem driver + entrypoint.
- `test/vectors_test.rb` — pins the PRNG + checksum to the shared cross-language vectors (the
  agreement contract). Run: `mise exec -- ruby bench/ruby/test/vectors_test.rb`.

No build step beyond the gem's native extension (`rake ruby:build`, done by `rake bench:build`).
Deliberately **outside `rake ci`** — wall-clock is nondeterministic (CLAUDE.md §10).
