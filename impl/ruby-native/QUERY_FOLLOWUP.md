# Ruby query-shape optimization pass

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](../../bench/ruby-native/ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

The subsequent [scalar pass](SCALAR_FOLLOWUP.md) addresses the cheap-query
overhead measured here. These tables retain this pass's original measurements.

This explicitly requested continuation makes the canonical memory filtered scan
and aggregate about **3.1× faster**, grouping **2.8× faster**, and sort-and-limit
**1.9× faster** on Ruby 4.0.7 YJIT. Across the tested YJIT/ZJIT modes, all six
non-scalar memory workloads are now **within 8× Go**; file PK lookup remains
**7.7–8.4× Go**. Cheap scalar queries remain much slower, at 26–31× Go.
This builds on the [second lookup pass](LOOKUP_SECOND_PASS.md).
The experiment remains frozen and opt-in; normal
tests, benchmarks, CI, and future feature updates still exclude it. No new
dependency or supported-core promotion is part of this change.

## Implementation

Canonical integer casts now perform their existing range check directly, avoiding
four keyword-forwarding wrappers for temporal, composite, and collated values.
Nominal types and other casts retain their complete dispatch. This removes about
8,000 temporary hashes from the canonical 1,000-row filtered scan.

Resident scans with no overflow/decompression payload units avoid allocating
per-row ordinal and accounting arrays. They still execute the zero-cost check
before each row, including after a suspended cursor resumes: another query can
have exhausted the session's shared lifetime budget in the meantime. Paged reads
retain their demand-fault accounting.

Grouping resolves grouping-set key positions once and stores evaluated keys in
arrays rather than AST-keyed hashes. Unordered aggregate inputs retain their
values directly instead of decorating every input with empty ordering keys and
an ordinal. Ordered aggregates retain stable ordering, and argument evaluation,
fold order, errors, cost charges, and logical memory reservations are preserved.
Numeric sorting avoids temporary collation arrays and repeated direction-string
conversions; collated values retain their compatibility checks.
Integer addition retains the same overflow check while bypassing generic operator
normalization, and integer comparisons bypass inapplicable container/collation
dispatch. Decimal, floating-point, NULL, and open-type behavior retain their paths.

Prepared binding and cursor metadata caches validate existing schema identities
without rebuilding inventories on cache hits. General cursor cloning uses a hash
for omission checks and shares audited immutable inputs and construction caches;
mutable regex VM state remains private. Eager clean reads avoid rebuilding
unchanged index metadata after a successful refresh for the same catalog and
host environment. Writes, transactions, and changed catalogs retain refreshes.
Durable publication avoids a second refresh already performed when adopting the
new storage roots.

The new eager prepared-statement invalidation tests also exposed a pre-existing
disk constraint-key bug: the default `C` collation was passed as a custom
collation name. The constraint encoder now uses the same nil-for-C convention
as the index writers. This is a correctness fix, not a benchmark shortcut.

## Measurement contract

Baseline commit: `81dfcbb0d0de239b62d7fa454bc8c467d24d776f`.
Baseline Ruby library SHA-256:
`00fa6ca66444430c9715160c1a59c6d00c6432a4f83a810fb2b8e0b3a1041ae3`.
Final Ruby library SHA-256:
`a4e732343b161afcab199bf9b5a4bde5927c657e0721caa21726bb99a14ef0a7`.
Both baseline and final timing workers load immutable source copies and record
the hash before and after execution. The existing benchmark SQL, data,
parameters, warmup counts, adapters, checksums, and supported cores are unchanged.

The host is an Intel Core Ultra 9 285K on x86-64 Linux/ext4. Ruby modes are 4.0.7
interpreter/YJIT and the previously built 4.1.0-preview1
interpreter/YJIT/ZJIT. The runtime checks require the requested JIT to be active.
The other cores use Go 1.27.1, Rust 1.99.0, and Node 26.10.0. Timing runs are
sequential, without concurrent tests or builds from this task. This is a shared
host; small timing differences should not be treated as established improvements.

The new [query diagnostic](../../bench/ruby-native/query_profile.rb) exercises
the same seven memory workloads through both lazy `query_prepared` and eager
`execute_prepared`. It finishes all timing before enabling TracePoint or
ObjectSpace tracing. Allocation tracing disables GC for one query at a time;
traced time is never reported. Diagnostic timings describe those two Ruby API
paths and are separate from canonical cross-core comparisons.

## Canonical memory results

Median microseconds per query across five batches on the unchanged 1,000-row
plan. Each lane also verifies 32 answer-checksum cases per query. Canonical
warmups remain unchanged and are short; these results do not represent uniform
steady-state JIT compilation across query shapes.

| Query | 4.0 interpreter, before → after | 4.0 YJIT, before → after | 4.1 YJIT, before → after | 4.1 ZJIT, before → after |
|---|---:|---:|---:|---:|
| Scalar | 63.94 → 56.17 | 25.79 → 22.08 | 24.13 → 19.18 | 26.60 → 22.86 |
| PK hit | 8.13 → 8.44 | 3.96 → 4.13 | 4.35 → 4.34 | 3.98 → 3.96 |
| PK miss | 6.71 → 6.83 | 3.51 → 3.30 | 3.34 → 3.29 | 3.50 → 3.57 |
| Filtered scan | 1912.20 → 878.12 | 982.85 → 322.05 | 966.62 → 292.59 | 1169.31 → 511.25 |
| Aggregate | 1624.31 → 740.84 | 942.77 → 303.02 | 924.94 → 297.01 | 1150.33 → 499.77 |
| Group | 2948.69 → 1356.68 | 1832.48 → 652.91 | 2275.48 → 653.13 | 2328.02 → 883.78 |
| Sort and limit | 2818.92 → 1594.55 | 1670.75 → 867.20 | 1651.96 → 929.32 | 1671.86 → 1147.10 |

Final cross-core comparison:

| Query | 4.0 YJIT | 4.1 YJIT | 4.1 ZJIT | Go | Rust | TypeScript |
|---|---:|---:|---:|---:|---:|---:|
| Scalar | 22.08 | 19.18 | 22.86 | 0.74 | 1.41 | 1.74 |
| PK hit | 4.13 | 4.34 | 3.96 | 1.34 | 2.23 | 4.11 |
| PK miss | 3.30 | 3.29 | 3.57 | 0.98 | 1.79 | 2.48 |
| Filtered scan | 322.05 | 292.59 | 511.25 | 162.93 | 106.13 | 322.62 |
| Aggregate | 303.02 | 297.01 | 499.77 | 65.16 | 51.57 | 169.47 |
| Group | 652.91 | 653.13 | 883.78 | 250.42 | 172.22 | 442.03 |
| Sort and limit | 867.20 | 929.32 | 1147.10 | 208.21 | 136.41 | 318.87 |

YJIT non-scalar queries range from 1.8× to 4.7× Go. ZJIT also benefits, but its
aggregate is 7.7× Go and its scan/group/sort results trail YJIT in this plan.
Cheap scalar queries still pay general cursor setup and remain 26–31× Go.
Memory PK timings are broadly unchanged, with small regressions as well as
improvements. No new specialized scalar executor or benchmark-specific result
cache was added.

Ruby 4.1 interpreter results are also recorded: filtered scan 867.95 µs,
aggregate 744.20 µs, group 1321.65 µs, and sort-and-limit 1613.55 µs. Full samples,
answers, runtime checks, and resource peaks are in the [4.0 baseline](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-before-4_0_7.json`),
[4.1 baseline](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-before-4_1_preview1.json`),
[4.0 final and supported cores](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-after-4_0_7.json`),
and [4.1 final modes](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-after-4_1_preview1.json`).

## Canonical file results

Mean microseconds per operation. The unchanged file workloads use a million-row
lookup dataset (50,000 warmup and 50,000 measured queries), 10/200 cold-open
warmup/iterations, and 10/300 durable-commit warmup/iterations. Cold open does not
flush the operating-system cache. Workers use private copies on ext4, exclusive
locking, normal synchronous durability, and the same 512 MiB RSS limit.

| Runtime/core | PK lookup | Cold open + lookup | Insert + durable commit |
|---|---:|---:|---:|
| Ruby 4.0.7 interpreter | 34.91 | 26811.75 | 6890.68 |
| Ruby 4.0.7 YJIT | 16.27 | 4024.31 | 5263.34 |
| Ruby 4.1 preview interpreter | 35.32 | 25234.32 | 6315.04 |
| Ruby 4.1 preview YJIT | 17.61 | 3920.13 | 5045.73 |
| Ruby 4.1 preview ZJIT | 17.49 | 7712.66 | 5785.52 |
| Go | 2.10 | 492.14 | 1975.42 |
| Rust | 2.37 | 454.11 | 1696.19 |
| TypeScript | 7.76 | 1150.14 | 2085.55 |

The JIT file lookup ratios are **7.7×, 8.4×, and 8.3× Go**, respectively, still
within the requested target. The fresh 4.0 baseline is 35.74 µs interpreter and
16.80 µs YJIT, so the lookup improvements are small compared with the general
query gains. This is one complete run per mode; the prior pass contains the
repeated lookup evidence. These samples do not establish a general JIT winner.

The fresh baseline/final durable controls give **6530.36 → 6890.68 µs** for the
4.0 interpreter (+5.5%) and **5368.46 → 5263.34 µs** for 4.0 YJIT (−2.0%). This
does not establish a write-speed improvement. ZJIT cold-open latency also remains
substantially higher than YJIT. Go still generates parameters inside timed file
operations while the other adapters prepare them immediately before timing;
that existing harness difference is unchanged.

Raw reports: [4.0 baseline](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-followup/before-file-4.0.7.json`),
[4.0 final and supported cores](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-followup/final-file-4.0.7.json`),
[4.1 final modes](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-followup/final-file-4.1-preview1.json`).

## Lazy and eager API diagnostics

Ruby 4.1 YJIT, median microseconds across five timed batches. Allocation figures
are GC allocation-counter deltas per query from the same batches, rounded to
whole objects. All 14 before/after first-answer digests match.

| API / query | Before, µs | After, µs | Allocations before → after |
|---|---:|---:|---:|
| query scalar | 23.03 | 20.22 | 168 → 147 |
| query PK hit | 3.90 | 4.26 | 29 → 28 |
| query PK miss | 3.59 | 3.79 | 24 → 24 |
| query filtered scan | 928.92 | 296.38 | 10,533 → 503 |
| query aggregate | 980.51 | 377.47 | 10,500 → 1,128 |
| query group | 1887.64 | 701.52 | 19,443 → 5,293 |
| query sort and limit | 1609.55 | 926.50 | 18,639 → 9,696 |
| execute scalar | 13.14 | 13.07 | 181 → 140 |
| execute PK hit | 16.48 | 12.73 | 192 → 134 |
| execute PK miss | 14.65 | 11.31 | 180 → 131 |
| execute filtered scan | 1029.79 | 276.93 | 10,527 → 478 |
| execute aggregate | 954.53 | 289.05 | 10,271 → 904 |
| execute group | 1796.89 | 633.04 | 19,377 → 5,208 |
| execute sort and limit | 1628.30 | 904.32 | 18,625 → 9,663 |

Eager reads benefit from removing redundant index metadata work. The already
specialized lazy PK path receives little benefit; its small diagnostic slowdown
is reported rather than treated as an improvement. The final canonical results
are the appropriate cross-core comparison.

[Baseline profile](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-followup/before-profile.json`)
and [final profile](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-followup/final-profile.json`)
retain all samples, per-method call counts, and allocation sites. The
[intermediate profile](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-followup/candidate-profile.json`)
isolates the stage before the integer addition/comparison kernels: adding those
kernels reduces sort-and-limit from 1416 to 927 µs and grouped aggregation from
805 to 702 µs in this diagnostic. Scalar queries still pay the general cursor
setup cost; a substantial further reduction would require a broader execution
change rather than another allocation cleanup.

## Validation

| Final-source audit | Result | Peak sampled worker RSS |
|---|---:|---:|
| Memory SQL/concurrency corpus, Ruby 4.1 ZJIT | 20,267 / 20,267 | 138.1 MiB |
| Disk SQL/concurrency corpus, Ruby 4.1 YJIT | 18,250 / 18,250 | 188.0 MiB |
| Host/storage suite, Ruby 4.0.7 | 40 test files pass | 109.0 MiB |

Both corpus audits cover 679 files with zero failures, unsupported files,
timeouts, or memory-limit failures. Existing storage-mode rules skip two files
in memory and 77 on disk. The shared corpus primarily covers eager execution;
host cursor tests and benchmark answer checks cover the lazy paths as well.
No PostgreSQL oracle run is needed for these implementation-only changes; SQL
semantics and the shared corpus are unchanged.

[Memory audit](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-followup/final-conformance-memory-4.1-zjit.json`),
[disk audit](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-followup/final-conformance-disk-4.1-yjit.json`),
and [host evidence](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-followup/final-host-4.0.7.json`)
record the final source hash before and after execution. New host cases retain
prepared statements across writes, savepoint rollback, table/view/type
replacement, and file publication. The C-collation regression covers text and
text-array primary/unique keys, duplicate errors, canonical key bytes, and reopen.
It belongs in host tests because the disk corpus reopens between autocommit
records, which had hidden the bug in live descriptors.

All **112 before/after memory cells, 30 file cells, and 14 before/after diagnostic
answers** pass their cross-report checks. The [summary](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/query-followup/final-summary.json`)
records source hashes, active JIT modes, checksums, resource bounds, and artifact
digests. Intermediate artifacts retain their own source hashes, including the
host failure that exposed the constraint-key bug; they are separate from the
successful final-source evidence.

## Reproduction

Use the existing opt-in [runtime commands](../../bench/ruby-native/RUNTIMES.md)
and bounded runners. The diagnostic can be run explicitly with:

```sh
JED_NATIVE_TIMEOUT=180 JED_NATIVE_ADDRESS_SPACE_BYTES=2147483648 \
  mise run ruby-native:bounded -- ruby --yjit \
  bench/ruby-native/query_profile.rb --report /tmp/ruby-query-profile.json
```

Memory comparison phases for this pass are `query-before-4_0_7`,
`query-before-4_1_preview1`, `query-after-4_0_7`, and `query-after-4_1_preview1`.
Additional artifacts live under `bench/ruby-native/results/query-followup/`.
All workload workers retain the 512 MiB sampled RSS limit and deadlines.
These are resource guardrails, not a claim of a whole-engine heap ceiling.
