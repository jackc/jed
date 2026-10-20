# Final Ruby experiment benchmarks

These are the last measurements of the original experiment at
`48fd21cd34b1001b23aa2b8c04c4d53dd591fbee`, library SHA-256
`4d67d30ffbd9163214d1164c1c9c11dcdf5ca63e260a2b502402cc741314bf91`.
Rebasing and removing archived results did not change the Ruby library.
The other cores were measured at the recorded research revisions, before the
cleanup rebase; these are not new measurements against the newer master.

Measurement date: 2026-10-20 UTC. Linux x86-64, Intel Core Ultra 9 285K;
Ruby 4.0.7 with YJIT, Go 1.27.1, Rust 1.99.0 and Node 26.10.0.
Times are microseconds per fully consumed query, medians of five samples after
warmup, using the same 1,000-row fixture and parameter schedules from
[`nway-frontier-plan.json`](../../bench/ruby-native/compare/nway-frontier-plan.json).

The direct-core adapters prepare once. Ruby's adapter collects `Rows#rows`;
other core adapters drain their cursors. The separate actual Ruby-package run
uses the Rust binding's public `query` API, which reparses and materializes each
call; native Ruby uses a persistent session and the prepared, query or eager
path shown. These are API comparisons, not measurements of isolated FFI cost.
They must not be combined into one universal engine speed ratio.

## Direct-core comparison

| Workload | Ruby YJIT | Go | Rust | TypeScript | Ruby/Go | Ruby/Rust |
|---|---:|---:|---:|---:|---:|---:|
| scalar | 4.843 | 0.762 | 1.398 | 2.193 | 6.355× | 3.464× |
| pk_hit | 4.032 | 1.629 | 2.207 | 3.782 | 2.475× | 1.827× |
| indexed_join | 152.935 | 54.292 | 2.494 | 122.496 | 2.817× | 61.329× |
| three_table_join | 226.430 | 3.511 | 3.157 | 12.469 | 64.499× | 71.734× |
| three_table_join_miss | 209.152 | 1.094 | 1.514 | 3.668 | 191.137× | 138.177× |
| three_table_join_range | 1290.264 | 77.893 | 59.767 | 241.079 | 16.565× | 21.588× |
| three_table_computed_key | 250.967 | 6.980 | 6.125 | 16.492 | 35.955× | 40.972× |
| four_table_join | 313.553 | 4.287 | 3.992 | 13.437 | 73.138× | 78.541× |
| three_table_left_fence | 458.093 | 3.442 | 3.158 | 12.347 | 133.099× | 145.065× |
| join_residual | 1037.864 | 393.572 | 772.915 | 1568.760 | 2.637× | 1.343× |
| join_text_predicate | 179.598 | 70.031 | 2.579 | 147.701 | 2.565× | 69.627× |
| join_text_projection | 148.015 | 77.792 | 2.459 | 145.548 | 1.903× | 60.203× |
| join_integer_outputs | 171.854 | 113.005 | 2.694 | 199.030 | 1.521× | 63.789× |
| five_table_bounded | 5598.349 | 21.249 | 17.979 | 59.062 | 263.460× | 311.384× |

## Actual Ruby Rust binding

| Workload | Wrapper | Native prepared | Native query | Native eager | Prepared/wrapper |
|---|---:|---:|---:|---:|---:|
| scalar | 8.113 | 5.039 | 5.355 | 5.837 | 0.621× |
| pk_hit | 10.706 | 4.133 | 4.478 | 4.188 | 0.386× |
| indexed_join | 41.163 | 154.854 | 151.746 | 147.935 | 3.762× |
| three_table_join | 189.431 | 229.118 | 220.966 | 223.911 | 1.210× |
| three_table_join_miss | 188.018 | 201.296 | 205.994 | 193.844 | 1.071× |
| three_table_join_range | 239.818 | 1369.629 | 1293.154 | 1363.082 | 5.711× |
| three_table_computed_key | 146.815 | 257.569 | 246.946 | 245.880 | 1.754× |
| four_table_join | 421.355 | 326.982 | 308.610 | 318.757 | 0.776× |
| three_table_left_fence | 50.936 | 457.341 | 443.028 | 448.735 | 8.979× |
| join_residual | 899.522 | 1096.197 | 1034.693 | 913.837 | 1.219× |
| join_text_predicate | 45.978 | 184.257 | 185.068 | 177.199 | 4.008× |
| join_text_projection | 51.556 | 159.966 | 159.474 | 152.604 | 3.103× |
| join_integer_outputs | 63.703 | 188.256 | 183.117 | 178.105 | 2.955× |
| five_table_bounded | 1587.905 | 5715.771 | 5965.733 | 6390.929 | 3.600× |


## JIT observations and limits

Separate repeated prepared-query runs measured the final five-table query at
9,418.179 µs interpreted, 5,369.847 µs with YJIT and 5,436.050 µs with ZJIT.
Those values average two process medians. Interpreter/YJIT use Ruby 4.0.7;
ZJIT uses Ruby 4.1.0-preview1, so the comparison changes runtime version too.

The final metadata-sharing change reduced five-table elapsed time by 15.8–34.3%
across nine API/runtime cells and warmed allocations by 30.9%. Other results
were mixed: the ZJIT ordinary-query four-table control was 15.4% slower.
See [the final optimization report](JOIN_KEY_FOLLOWUP.md) for the limitations.

This last suite is JOIN-heavy. The earlier [broad performance checkpoint](PERFORMANCE_CHECKPOINT.md)
covers scans, aggregates, sorting, host callbacks and file operations at its
own older source revision. Those timings are not final-revision measurements.

Raw samples, exact values/metadata/cost preflights, source/runtime hashes and
all slower observations remain in the [original evidence archive](../../bench/ruby-native/ARCHIVE.md),
under `bench/ruby-native/results/join-key-followup/`.
