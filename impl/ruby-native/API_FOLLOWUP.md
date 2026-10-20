# Ordinary Ruby API and host-function follow-up

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](../../bench/ruby-native/ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

Measured 2026-10-19. This is an explicitly requested optimization of the frozen
Ruby experiment. Its support status and exclusion from normal tests, benchmarks,
CI, and future feature work are unchanged. No dependency was added.

## Changes

- Ordinary `query(sql)` and `execute(sql)` reuse parsed statements from a
  database-owned LRU cache: at most 64 entries, 64 KiB of retained SQL, and
  16 KiB per statement. Encoding is part of the key. Failed parses, oversized
  statements, and `EXPLAIN` trees are not retained. The cache stores no bindings,
  results, or session state. Public `prepare` returns an independent statement.
- The lexer groups operators by their first byte and avoids temporary strings
  when scanning punctuation and comments. Parameter discovery uses an explicit
  stack instead of recursively allocating arrays of child maxima.
- Host overload lookup uses an immutable environment-specific name index.
  Eligible read plans reuse resolved host expressions without folding callback
  results. Scalar lazy host queries can use the lightweight cursor; callbacks
  still execute on pull, with the original strict NULL, privilege, cost, error,
  ownership, and captured-context rules.
- Eligible eager scalar and PK queries drain lightweight cursors into ordinary
  owned `Result` objects on repeated use. A bounded session-local admission
  history avoids paying for cursor metadata on one-off SQL. Host callbacks keep the original eager executor so
  callback-visible meters and live setting mutations retain their behavior.
  Transactions, finite query-memory budgets, and unsupported shapes also keep
  their existing paths.
- Batch execution uses internal iteration instead of switching an Enumerator
  Fiber for every source row. Outcomes use positions within the bounded chunk,
  with a separate failing-row slot. The common single source charge is stored
  directly, while multiple charge events retain their individual boundaries.
  One-argument callbacks avoid temporary argument arrays, and integer result
  validation uses precomputed bounds. Profiling identified 2,000 avoidable
  charge/outcome objects per 1,000-row batch before this pass.

The existing deferred-value guard also treated a zero bit as truthy Ruby.
Checking that bit explicitly now leaves potentially failing large-value reads
at their normal evaluation site, preserving error order during batch prefetch.

## Measurement method

The [comparison harness](../../bench/ruby-native/binding_compare.rb) uses the
same Ruby executable/JIT for native Ruby and the release Rust-backed Ruby
package. Each lane runs in a separate bounded process, sequentially. GC stays
enabled. There were no overlapping tests, builds, or other benchmarks from this
task. The machine is Linux x86-64, Intel Core Ultra 9 285K; results remain
shared-machine microbenchmarks.

`native-query` uses an explicit long-lived session and fully drains `query(sql)`.
`native-eager` calls `execute(sql)` and iterates its result arrays.
`native-prepared` prepares outside timing and fully drains `query_prepared`.
The binding reparses, executes, marshals, and constructs Ruby rows on each call;
it currently has no prepared-statement or lazy API. Eager native rows are plain
arrays, so these compare the available APIs, not isolated language or FFI costs.
Database convenience methods still create fresh sessions and are not the timed
native API here.

The seven original queries use the shared 1,000-row plan. Cheap queries have
300 warmups followed by five batches of 2,000 calls; larger queries have 100
warmups and five batches of 50. Tables report median batch time per query.
Host kernels run identical Ruby arithmetic. Instrumented validation is outside
timing and checks values, names, types, costs, consumed rows, callback counts,
and actual batch sizes. Kernels used for timing do not count or hash results.

File comparisons use private copies of the million-row canonical fixture,
read-only opens with auto locking in both packages, 50,000 warmup probes, and
five batches of 5,000 hits returning four columns. These are warm reads, not
cold-open or write measurements.

Fresh-SQL controls append a unique SQL comment to every warmup and measured
call. They include parsing and force cache misses; the same modified SQL goes
to both packages. Extra mixed-expression and join shapes prevent a result based
only on the simplest scalar. Cold probes use the same five-batch schedule as
the cheap repeated queries.

Before measurements use the unchanged scalar-follow-up source at
`e5dbbe514e41822793e92c996fffbba5e5419e96`, SHA-256
`7d1197be345d44a7c389a66ec0a18d038282d26d59fad97ab08f8bdd6f8a7a28`.
Every report records native/wrapper/Rust source, executable, harness, extension,
and fixture hashes as applicable, plus actual JIT state and source stability.
The [raw results](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/api-followup`) retain each
sample and validation digest. The earlier
[binding comparison](BINDING_COMPARISON.md) remains the historical baseline.

## Results: stable Ruby 4.0.7 + YJIT

Microseconds per complete query, lower is better. These ordinary SQL calls
reuse their SQL text while varying parameters; they include the implicit cache
lookup and all execution work. The binding column is from the after run.

| Query/API | Before | After | Speedup | Rust binding |
|---|---:|---:|---:|---:|
| Scalar, query | 102.57 | 5.46 | 18.8× | 8.10 |
| Scalar, eager | 106.27 | 5.82 | 18.2× | 8.10 |
| Memory PK hit, query | 132.34 | 4.55 | 29.1× | 10.62 |
| Memory PK hit, eager | 93.62 | 4.04 | 23.2× | 10.62 |
| Memory PK miss, query | 129.76 | 3.89 | 33.3× | 9.62 |
| Million-row file PK, query | 136.50 | 18.97 | 7.2× | 17.61 |
| Million-row file PK, eager | 131.93 | 18.27 | 7.2× | 17.61 |
| Host scalar, prepared | 40.62 | 4.98 | 8.2× | 8.82 |
| Host scalar, query | 77.45 | 5.43 | 14.3× | 8.82 |
| Host scalar, eager | 63.95 | 29.57 | 2.2× | 8.82 |

Repeated ordinary scalar and memory PK queries now compete with prepared
execution and beat this binding's available API. Warm ordinary file reads are
close to the binding. Eager host calls improve but retain the general executor
for callback-state compatibility, explaining their larger remaining setup cost.

Larger ordinary `query` shapes, also in microseconds:

| Query | Before | After | Rust binding |
|---|---:|---:|---:|
| filtered scan | 668.72 | 322.32 | 131.08 |
| aggregate | 444.53 | 287.57 | 62.06 |
| group | 1010.36 | 669.66 | 224.67 |
| sort limit | 1036.69 | 907.60 | 160.03 |

Removing setup overhead helps, but the binding still wins these heavier
queries. This pass does not establish parity for scans, aggregates, or sorting.

## Host batching

Milliseconds per whole query. Unless labeled otherwise, native uses eager
execution. Both packages run the same Ruby kernel.

| Shape/API | Before | After | Rust binding |
|---|---:|---:|---:|
| 500 rows, individual callbacks | 1.552 | 1.353 | 0.790 |
| 500 rows, batch callbacks | 1.848 | 1.500 | 0.556 |
| 1,000 rows, individual callbacks | 1.720 | 1.490 | 1.409 |
| 1,000 rows, batch callbacks | 2.142 | 1.681 | 0.853 |
| 1,000 rows, batch callbacks, lazy query | 2.646 | 1.874 | 0.853 |
| 1,000 rows, batch callbacks, prepared | 2.194 | 1.870 | 0.853 |

Native batching is cheaper than before, but still slower than individual
native callbacks for this cheap arithmetic kernel. Its gather, ownership,
declared-cost replay, and error-boundary work outweigh the saved Ruby calls.
The binding benefits more from amortizing FFI upcalls. This is not evidence
that every host kernel should use batches.

The secondary predicate returns 500 rows: native uses eight chunks of up to
64, while the binding uses one batch. The PK-range query returns 1,000 rows:
both use 16 chunks of up to 64. These are whole-plan comparisons; the first
pair also has different plan/batching choices. Validation checks each callback
and batch size independently of the timed kernels.

## YJIT and ZJIT

Microseconds, **native / binding**. The first four columns use ordinary native
`query`; the last uses eager `execute`. Ruby 4.1 is preview1, not a new runtime
support promise. Reports verify the requested JIT was actually enabled.

| Runtime | Scalar | Memory PK | File PK | Host scalar | Eager host scalar |
|---|---:|---:|---:|---:|---:|
| 4.0-yjit | 5.46 / 8.10 | 4.55 / 10.62 | 18.97 / 17.61 | 5.43 / 8.82 | 29.57 / 8.82 |
| 4.1-yjit | 6.05 / 7.76 | 4.86 / 10.33 | 17.74 / 18.84 | 5.55 / 8.32 | 19.14 / 8.32 |
| 4.1-zjit | 6.85 / 7.87 | 4.59 / 10.07 | 20.71 / 17.29 | 6.25 / 8.34 | 19.26 / 8.34 |

The setup improvements work on both JITs. YJIT remains the better default for
the measured mix; ZJIT does not consistently win and some larger host shapes
remain substantially slower. Full matrices are retained in the JSON reports.
Against the historical preview-YJIT baseline, a few already-prepared queries
and buffered callbacks measured about 5–6% slower; some binding controls also
shifted similarly. Small cross-run differences do not establish universal gains.

## Fresh SQL

Microseconds for ordinary native `query`, with a different SQL string on every
invocation, including warmup. These numbers include parsing and plan admission.

| Query | 4.0 YJIT before | 4.0 YJIT after | 4.1 YJIT before | 4.1 YJIT after |
|---|---:|---:|---:|---:|
| scalar | 100.89 | 94.64 | 91.81 | 83.83 |
| pk hit | 93.02 | 88.89 | 81.86 | 72.56 |
| host scalar | 79.41 | 43.67 | 66.17 | 33.65 |
| cold mixed | 410.71 | 306.31 | 329.39 | 291.59 |
| cold join | 803.40 | 740.42 | 623.01 | 571.08 |

Cache misses remain much more expensive than reuse. The lexer and parameter
walk help some shapes, but there is no blanket cold-query speedup. Ruby 4.0
cold PK timings varied substantially with workload history, including isolated
probes; the reports retain those results rather than averaging them into the
repeated-query gains. An intermediate eager route added cursor planning even
on first use; the final version defers that work until reuse. Its isolated cold
eager PK probe was 80.41 µs versus the original 82.54 µs.

The Rust binding remains substantially faster for fresh SQL. Applications
issuing one-off complex SQL still have a reason to prefer it. Native batching
and heavy execution also leave meaningful gaps; experimental status remains
unchanged.

## Verification and resource bounds

Final native source SHA-256:
`d3db0d7784b23d9d9aa6da1f3f45045b298eef7279dc7e7987b81052ee2032fc`.

The full opt-in host/storage suite passes, including cold and warmed eager
comparisons, actual cursor admission, cache eviction/ownership, schema changes,
private namespaces, unconsumed snapshots, reentrant host context changes,
callback-visible eager meters, strict NULL/false/nil batch outcomes, nested
source charges, error prefixes, malformed kernels, and deferred file values.

| Corpus run | Passed / records | Corpus-marked skipped files |
|---|---:|---:|
| disk, execute, yjit  | 18250 / 18250 | 77 |
| memory, execute, zjit  | 20267 / 20267 | 2 |
| memory, query, yjit /cast/ | 269 / 269 | 0 |
| memory, query, yjit /expr/ | 2877 / 2877 | 0 |
| memory, query, yjit /params/ | 228 / 228 | 0 |

No failures, unsupported files, timeouts, or memory-limit failures occurred.
Memory/disk totals exclude only the corpus's existing mode-specific skips.
Lazy coverage explicitly runs expression, cast, and parameter suites; it is
not presented as a full lazy-corpus pass.

Every benchmark worker had a 512 MiB sampled process-group RSS ceiling, a hard
2 GiB address-space limit, and a 300-second deadline. Conformance additionally
isolates each file with a 10-second deadline and runs under a bounded outer
supervisor. Final peak RSS was 133.0 MiB for the host suite,
188.1 MiB for a corpus worker, and 351.3 MiB for a benchmark worker.
These harness limits are process safeguards, not whole-engine memory promises.

`intermediate/` retains earlier diagnostic runs; the top-level `after-*`
matrix and `final-*` verification reports describe the final source.

## Reproduction

Run only when explicitly opting into this experimental core:

```sh
mise run ruby-native:test
mise run ruby-native:binding-compare -- --output /tmp/ruby-api-memory.json
mise run ruby-native:binding-compare -- --file .dev/ruby-native-bench-data/large.jed --output /tmp/ruby-api-file.json
mise run ruby-native:binding-compare -- --fresh-sql --lanes wrapper,native-query,native-eager --workloads scalar,pk_hit,host_scalar,cold_mixed,cold_join --output /tmp/ruby-api-cold.json
```

Use `--ruby .dev/ruby-native-runtimes/ruby-4.1.0-preview1/bin/ruby` for the
separately built preview runtime, and add `--jit zjit` for ZJIT. The default
comparison JIT is YJIT. `--native-source PATH` selects an immutable source
snapshot for before/after work. See [runtime instructions](../../bench/ruby-native/RUNTIMES.md).
