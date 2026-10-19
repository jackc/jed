# Compiled Ruby ORDER BY comparisons

The cursor failures reported below are historical. The
[cursor conformance correction](CURSOR_CONFORMANCE_FIX.md) fixes them and
supersedes the mixed-routing interpretation. Original measurements are retained.

Measured 2026-10-19. This explicitly requested extension adds typed comparisons
to the experimental runtime compiler after the
[cursor and scan correctness fixes](CORRECTNESS_FOLLOWUP.md), committed at
`e8ec94e5`. The Ruby core remains frozen and opt-in. No dependency or public API
changes, and no supported-core implementation changes.

## Scope

A reused blocking single-table sort can compile its complete integer/boolean
key vector into one Ruby method. Direction, NULL placement, and comparison type
become fixed branches; the original input ordinal still breaks complete ties.
The method receives two entries containing already evaluated keys. It does not
retain a session, engine, parameters, rows, or resolved expressions.

Key evaluation, callbacks, projection, cost checkpoints, memory reservations,
the bounded LIMIT heap, full sorting, and external spill/merge algorithms retain
their existing order. A typed key produced by a host callback may use the new
comparator without compiling or moving that callback. Unsupported types,
collations, and named types use the original comparator. The first query uses
that comparator; the second may generate code.

Each method is limited to 192 keys and 32 KiB source. Ordering is the fifth slot
in the existing bounded plan cache and shares its 64 KiB total source cap.
Source consists only of compiler templates and numeric key indexes. SQL text
and values never enter generated source. The cached method Proc retains only
its kernel; churn measurements below distinguish collectible instances from
Ruby's separate JIT code/class retention policy.

## Measurements

Compiled comparisons substantially improve these sorting workloads. Stable
Ruby 4.0.7 YJIT gains **1.69–2.63×**, depending on shape and API. Both enabled
medians are faster than both disabled medians for every sorting/API pair.
The scalar and plain-scan controls establish no general improvement to queries
that do not use the comparator.

This table reports mean paired medians in microseconds per complete query, on
the same final source. Prepared uses `query_prepared`; eager uses `execute`.

| Query | Prepared disabled | Prepared compiled | Speedup | Eager disabled | Eager compiled | Speedup |
|---|---:|---:|---:|---:|---:|---:|
| Sort/LIMIT 20 | 882.15 | 355.90 | 2.48× | 849.59 | 323.20 | 2.63× |
| Computed sort/LIMIT | 962.45 | 456.99 | 2.11× | 1004.59 | 408.04 | 2.46× |
| Full sort | 1592.67 | 803.67 | 1.98× | 1363.57 | 586.61 | 2.32× |
| Nullable integer/boolean sort | 1049.60 | 607.94 | 1.73× | 1076.23 | 635.27 | 1.69× |

Ordinary `query` improves 1.79–2.49×. Unchanged scan controls contain visible
noise: one prepared enabled run takes 633 µs while its other runs take 474–499 µs;
one ordinary-query disabled run takes 603 µs while its others take 473–477 µs.
Those shifts cannot be attributed to comparator execution, which that query
never uses. Scalar medians vary little, but slow first samples remain in the raw
results. The sorting gains are well separated despite these observations.

The improvement also holds without a JIT and with preview ZJIT:

| Runtime | Prepared sorting gains | Ordinary query gains | Eager sorting gains |
|---|---:|---:|---:|
| Ruby 4.0.7 interpreter | 1.68–1.83× | 1.67–1.84× | 1.71–2.00× |
| Ruby 4.0.7 YJIT | 1.73–2.48× | 1.79–2.49× | 1.69–2.63× |
| Ruby 4.1.0-preview1 ZJIT | 1.77–2.14× | 1.76–2.10× | 1.76–2.12× |

All 36 sorting/API/runtime comparisons have both enabled medians below both
disabled medians. ZJIT's eager scalar control moves about 5.6% slower, while its
plain-scan controls stay within 2.5%; this is not evidence of a scalar benefit.
The runtimes are different Ruby versions, so their absolute timings do not
isolate YJIT versus ZJIT implementation differences.

The six workloads use the unchanged 1,000-row fixture: scalar/plain-scan controls,
the existing sort/LIMIT and computed-sort queries, a full sort, and a sort with
nullable integer keys plus boolean keys. The original workloads retain their existing
warmup and iteration schedules. Every timed run retains five samples; GC stays
enabled. All results are fully consumed.

Pre-change measurements use a source snapshot from `e8ec94e5` with SHA-256
`e717a4cc8d51ac2d139bde06ab6f58c288aaf8f5510ea112eb4317cef83ab97e`.
The new implementation is
`ac5c78008942148b55e7f13f4a9c133d4bc1bc730741999a147e5d0dd9e4040d`.
The controlled runs use that same new source in fresh processes, toggling only
the internal ordering switch: disabled, enabled, enabled, disabled. Expression
and pipeline compilation remain enabled. Mean paired medians summarize this
small alternating experiment; they are not confidence intervals.

The [raw evidence](../../bench/ruby-native/results/ordering-followup/) retains
source/runtime/harness hashes, commands, schedules, samples, answer and cost
digests, and process limits. No timed job overlaps tests, builds, or another
benchmark job.

Pre-change source measurements are also retained, rather than substituted by
the controlled disabled runs. These are original prepared-query medians in µs:

| Runtime | Sort/LIMIT | Computed sort | Full sort | Nullable integer/boolean sort |
|---|---:|---:|---:|---:|
| 4.0.7 interpreter | 1610.23 | 1861.12 | 3141.76 | 2353.14 |
| 4.0.7 YJIT | 900.34 | 992.26 | 1651.49 | 1193.17 |
| 4.1 preview ZJIT | 1010.61 | 1125.80 | 1839.05 | 1191.31 |

The controlled comparisons isolate the new tier more directly than those
earlier observations. `summary.json` contains all per-runtime/API values;
`timing-summary.json` additionally retains pooled-sample statistics, explicitly
distinguished from the paired-median statistic used here.

## Contemporary core and Ruby-binding comparisons

The shared comparison adapter measures complete prepared queries with identical
SQL, data, parameter schedules, warmups, and result consumption across languages.
All values/checksums and row counts agree. This adapter does not compare costs;
the separate actual-binding comparison below verifies exact costs too.
These are independent measurements in µs, not the ABBA medians above.

| Query | Ruby 4.0.7 YJIT | Go | Rust | TypeScript |
|---|---:|---:|---:|---:|
| Scalar control | 4.69 | 0.86 | 1.38 | 2.30 |
| Sort/LIMIT 20 | 361.02 | 230.19 | 148.10 | 453.23 |
| Computed sort/LIMIT | 431.93 | 317.73 | 208.00 | 521.74 |
| Plain-scan control | 504.11 | 167.33 | 109.67 | 339.08 |
| Full sort | 872.97 | 371.09 | 175.94 | 414.78 |
| Nullable integer/boolean sort | 535.03 | 364.95 | 243.28 | 545.34 |

Ruby is **1.36–2.35× Go** and **2.08–4.96× Rust** on these four sorting shapes.
It is faster than TypeScript on the simple and computed bounded sorts and is
within 2% on the mixed-key sort; TypeScript remains faster on the full sort.
These measurements do not establish general Go/Rust parity.

The actual Ruby Rust binding still has an advantage on bounded sorts. Its
materialized API is compared here with native `execute`, both on Ruby 4.0.7
YJIT. All six workloads match in values, metadata, and exact costs across the
binding and all three measured native APIs.

| Query | Native eager | Rust binding |
|---|---:|---:|
| Scalar control | 5.54 | 7.83 |
| Sort/LIMIT 20 | 336.41 | 156.64 |
| Computed sort/LIMIT | 423.72 | 261.62 |
| Plain-scan control | 259.46 | 682.54 |
| Full sort | 606.80 | 757.65 |
| Nullable integer/boolean sort | 556.54 | 309.01 |

Native eager full sorting is about 1.25× faster than the binding in this run;
native prepared full sorting is 783.02 µs versus the binding's 757.65 µs.
The collector/API choice therefore still matters. Whole-query timing here is
in-memory; file and spill behavior are covered by correctness tests, not by a
claim of equivalent gains on large durable databases.

## Direct generated-code evidence and overhead

The direct diagnostic compares the exact former comparator lambda with generated
methods over precomputed keys. It includes integer and nullable integer/boolean
vectors, boundary values, complete ties, and stable ordinals. Every boundary pair
and full sorted result is checked before and after timing, with zero added cost.

Direct comparison gains are 4.50–4.89× in the interpreter, 6.15–8.02× with YJIT,
and 2.41–4.71× with ZJIT. Sorting 1,000 precomputed entries improves 5.38–5.54×,
7.12–8.11×, and 2.95–5.36× respectively. These exclude scanning, key evaluation,
projection, and public API overhead; they are not whole-query speedups.
Each 100,000-comparison sample allocates zero objects in the generated path
versus 175,000 in the original lambda. A complete direct sort drops from
16,811–17,834 objects to its two Array allocations.

YJIT reports 178 and 560 compiled instructions for the two generated methods.
ZJIT's per-job compilation log proves both generated methods compiled. This is
direct evidence for those methods, beyond a global JIT-enabled flag.

Repeated compilation medians are 20.6–35.1 µs. First compilation is 57.7–419.5 µs,
and the first comparison is 0.7–1.4 µs. These are distinct from warmed execution
and support the existing reuse-based admission policy.

Churn creates 256 kernels, invokes each 200 times, retains at most 64, then
releases them. Live kernels return from 64 to zero in every runtime, including
their cached Proc cycles. After GC, YJIT retains 26 anonymous generated classes
and a 2.70 MB process code region; ZJIT retains 22 classes, a 1.81 MB code region,
and 4.05 MB total JIT memory. These are whole-process counters. Cache eviction
does not promise immediate VM code/class reclamation. The largest timed control
worker uses 83.85 MiB RSS; direct diagnostics stay below 52 MiB.

## Correctness and resource checks

All final validation runs verified the implementation hash above unchanged.
Corpus admission was forced so one-use statements could exercise generation.

| Audit | Passed / attempted | Generated ordering kernels |
|---|---:|---:|
| Memory execute, Ruby 4.1 preview ZJIT | 20,267 / 20,267 | 168 |
| File execute, Ruby 4.1 preview YJIT | 18,250 / 18,250 | 161 |
| Memory mixed query/execute, Ruby 4.0.7 YJIT | 20,267 / 20,267 | 168 |
| Memory pure query, Ruby 4.0.7 YJIT | 20,221 / 20,267 | 168 |

Every run examines all 679 files and preserves the corpus's existing storage-mode
skips: two memory files and 77 disk files. There are no unsupported files,
timeouts, memory-limit hits, or new failures. The pure-query diagnostic retains
exactly the same 46 collector-threshold mismatches as the corrected baseline;
its raw failures are identical. The mixed audit routes 159 finite-memory
invocations to their authored materialized API and 19,928 others to `query`.
It is not a full pure-query pass; the
[correctness report](CORRECTNESS_FOLLOWUP.md) explains the two collector contracts.

All 52 host-test files pass. The new internal test independently checks generated
dispatch, NULL/direction/type combinations, ordinal ties, template injection
rejection, source/cache limits, all four Ruby APIs, reused bindings, cost/lifetime
failure prefixes, finite query memory, callback/error order, snapshots,
permissions, schema replacement, and actual external spills with cleanup after
drain, early close, and projection errors.

Corpus workers retain a 512 MiB RSS guard, 2 GiB address-space limit, and
10-second per-file deadline. The largest worker used 181.2 MiB RSS. These process
guards bound the audit; they are not a whole-engine memory guarantee for hosts.

## Reproduction

```sh
# All Ruby work remains explicitly selected.
mise run ruby-native:test

mise run ruby-native:binding-compare -- \
  --workloads scalar,sort_limit,computed_sort,plain_scan,full_sort,sort_mixed_nulls \
  --lanes native-prepared,native-query,native-eager \
  --jit yjit --output /tmp/ordering-enabled.json
# Repeat with --disable-ordering for the control.

JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 mise run ruby-native:bounded -- \
  ruby --yjit bench/ruby-native/ordering_diagnostics.rb \
  --jit yjit --report /tmp/ordering-diagnostic.json

RUBY_NATIVE_COMPARE_PLAN=bench/ruby-native/compare/ordering-plan.json \
  RUBY_NATIVE_PHASE=ordering-comparison RUBY_NATIVE_LANES=ruby-yjit,go,rust,ts \
  RUBY_NATIVE_COMPARE_RUBY_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:compare
```

Use `--jit none` for the interpreter. For ZJIT, select the separately built
Ruby 4.1.0-preview1 executable with `--ruby` in the binding comparison, or invoke
it with `--zjit` in the bounded diagnostic. The retained commands include the
direct ZJIT compilation log flags. Runtime installation remains separate from
the normal repository configuration.
