# Ruby cursor and scan correctness follow-up

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](../../bench/ruby-native/ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

The remaining 46 failures in this historical report were subsequently addressed
by the [cursor conformance correction](CURSOR_CONFORMANCE_FIX.md). The explanation
below that the shared corpus was authored for materialized execution was wrong:
the supported runners drain cursors externally. Correcting the Ruby adapter also
exposed three real accounting failures that its extra collector had masked.
The original measurements and raw failure reports remain historical evidence.

Updated 2026-10-19. This explicitly requested pass fixes defects found during the
[pipeline audit](PIPELINE_FOLLOWUP.md) at commit
`39f804e416fd264a315b98183ef38354e5105d15`. The Ruby core remains frozen and opt-in;
this work does not change the shared SQL, memory, cost, or file-format contracts.

## Corrected behavior

Cursor admission now resolves the same JOIN layout and names as execution,
including merged USING/NATURAL columns, qualified original keys, nested join
groups, and aliases. It validates recursive CTE shape before attempting cursor
metadata and preserves invalid RIGHT/FULL LATERAL reference errors. JSON deletion
and contextual JSON pretty flags retain their proper result types and errors.
The headers are correct before the first pull, including for an empty result;
draining a cursor no longer has to repair them.

Legal deep expressions, query sources, JSON containment, and JSONPath
parsing/rendering no longer depend on the smaller stack available inside a Ruby
cursor Fiber. Bounded stack slices preserve recursive evaluation order and the
SQL/JSON nesting limits. The root SELECT stays on its owning cursor Fiber, so
pulling one row and closing still leave later projections unevaluated. JSONPath
regex precharging and window-shape inspection use explicit stacks. Parameter
metadata reuses resolved subtree types for the same scope, hint, and binding
revision instead of repeatedly revisiting nested array constructors.

Ordered contiguous scans now charge every structural page and touched payload
in the bound before row work. LIMIT and OFFSET reduce row work without reducing
that logical page block. A disjoint interval set charges each complete interval
when first started; LIMIT 0 starts no interval. Secondary intervals retain the
per-row primary-key lookup charge. Fixed-width touched columns need no payload
inspection, preserving lazy access to later leaves.

Budgeted correlated projections keep the ordinary executor's output checkpoint
order when cursor execution alone would otherwise choose streaming. The shared
`resource/dos_amplification.test` example completes at its exact 21-unit boundary
and fails at earlier disallowed checkpoints. Inner unguarded output increments
also update the session's shared lifetime meter, so later cursor charges cannot
overwrite them with a stale total.

## Exact Rust-binding cost comparison

The pipeline report's 1,000-row fixture was replayed against the actual Ruby Rust
binding and all four native APIs: `execute`, `query`, `execute_prepared`, and
`query_prepared`. For bindings 0 through 4, values and exact costs now agree:

| Query shape | Native before correction | Corrected native and Rust binding |
|---|---|---|
| LIMIT 20 | 102, 108, 114, 120, 134 | 106, 112, 118, 124, 138 |
| OFFSET 500 / LIMIT 20 | 1104, 1220, 1364, 1555, 1801 | 1106, 1222, 1366, 1556, 1802 |

The SQL is `SELECT id, score * 3 + bucket FROM items WHERE bucket >= $1
ORDER BY id LIMIT 20`, with `OFFSET 500` for the second row. The
[portable reproducer](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/correctness-followup/limit-cost-probe.rb`)
includes fixture construction. The
[focused result](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/correctness-followup/limit-cost-focused.json`)
retains the original command, implementation hash, full API cost vectors, and
52,166,656-byte peak RSS. This is correctness validation, not a timing benchmark.

## Query-memory audits and host collectors

The original pure-query audit remains historical evidence: 71 failures included
actual lazy execution defects and expectations authored for the materialized
executor's memory thresholds. `Rows#rows` owns a result collector; external
`Rows#each` collection belongs to the host. Their retained buffers differ under
[memory.md §5.1 and §5.4](../../spec/design/memory.md), so their exact memory
thresholds need not equal `execute` thresholds.

The default harness still applies the selected `JED_NATIVE_QUERY_API` to every
record. An explicit mixed audit can set `JED_NATIVE_QUERY_MEMORY_API=execute`:
records with a finite `max_query_memory_bytes` use `execute`, and other records
use `query`. It changes no expected value, error, cost, or memory allowance and
skips no corpus records. Reports record the selected APIs, public API invocation
counts, and `materialized_memory_records`. A passing mixed audit is **not a full
pure-query pass**. Write statements selected through `query` still use its normal
internal dispatch to `execute`.

The separate [cursor resource tests](test/cursor_resources_test.rb) pin the lazy
host contracts directly: a 100-row i32 collector needs exactly 6,400 logical
bytes, while an externally drained streaming scan needs no output collector.
A three-row materialized function input plus an owned collector needs 384 bytes;
external collection retains only its 192-byte input. Tests cover exact allowance
boundaries, captured budgets, failed-collector pin cleanup, transaction failure,
and deep-query suspension/early close.

## Verification

All final runs use implementation SHA-256
`e717a4cc8d51ac2d139bde06ab6f58c288aaf8f5510ea112eb4317cef83ab97e`,
verified unchanged before and after each run. Compilation admission was forced
for the corpus. Every run examined all 679 files; the corpus's existing mode
directives skip two memory files and 77 disk files. No new skips were added.

| Audit | Passed / attempted | Peak worker RSS |
|---|---:|---:|
| Memory, execute, Ruby 4.1 preview ZJIT | 20,267 / 20,267 | 151.4 MiB |
| File, execute, Ruby 4.1 preview YJIT | 18,250 / 18,250 | 178.3 MiB |
| Memory, mixed query/execute, Ruby 4.0.7 YJIT | 20,267 / 20,267 | 161.4 MiB |
| Memory, pure query, Ruby 4.0.7 YJIT | 20,221 / 20,267 | 167.8 MiB |

The mixed run records 19,928 `query` invocations and 159 finite-memory `execute`
invocations. The pure-query run retains exactly the 46 prior memory-threshold
mismatches, including their transaction-abort cascades, across four resource
files. It has no new failures. All 25 prior implementation failures are resolved,
and all 97 records previously left unattempted by stack crashes now execute.
There are no timeouts, memory-limit hits, or unsupported files in any final run.
Each worker retained the 512 MiB RSS guard, 2 GiB address-space limit, and
10-second file deadline.

All 51 Ruby host-test files pass on the same source, at 107.7 MiB peak RSS.
The [retained reports](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/correctness-followup`)
include every raw failure, API selection, source hash, runtime, and bounded-run
command. `verification-manifest.json` records artifact hashes and the exact
old/new failure comparison.

The new [scan-cost host test](test/scan_cost_test.rb) passes on memory and file
hosts. It checks all four APIs, multi-leaf forward/reverse bounds, OFFSET/LIMIT 0,
compressed payload blocks, lazy interval admission, row-fault ordering, and every
statement/lifetime budget prefix from 1 through 22 for the correlated query.
Its [focused report](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/correctness-followup/scan-cost-focused.json`)
records 43,327,488-byte peak RSS. The existing compressed-cost, compiled-pipeline,
and window-prefix host tests also pass. Focused lazy shared-corpus runs passed
`resource/dos_amplification.test` (24/24) and
`query/bounded_limit_streaming.test` (30/30).

Focused reports retain their development-source hashes; the final audit table
above identifies the complete corrected implementation. Original pipeline
measurements and failure reports remain unchanged. No new PostgreSQL behavior is
introduced: these corrections implement existing shared expectations and host
contracts.

## Reproduction

All runs remain explicit. The RSS ceiling below is sampled for the process group;
the address-space ceiling is a hard limit. Neither is a whole-engine memory
guarantee for applications embedding the library.

```sh
JED_NATIVE_MEMORY_BYTES=536870912 JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:bounded -- ruby -Iimpl/ruby-native/lib \
  impl/ruby-native/test/scan_cost_test.rb

JED_NATIVE_MEMORY_BYTES=536870912 JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:bounded -- ruby \
  bench/ruby-native/results/correctness-followup/limit-cost-probe.rb

# Pure query audit: keeps every materialized-memory expectation unchanged.
JED_NATIVE_QUERY_API=query JED_NATIVE_COMPILATION=immediate \
  JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 mise run ruby-native:conformance

# Mixed audit: materialized-memory fixtures use execute; other records use query.
JED_NATIVE_QUERY_API=query JED_NATIVE_QUERY_MEMORY_API=execute \
  JED_NATIVE_COMPILATION=immediate JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:conformance
```
