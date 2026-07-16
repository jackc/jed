# Benchmarks — cross-core and cross-engine wall-clock measurement

Status: v2 landed (corpus format, setup tool, harnesses in all three
cores). Grown since with `cte_materialized`, `lateral_top_n_per_group`, the GIN-bounded-scan
benchmarks (`gin_contains` / `gin_overlaps` / `gin_member` / `gin_array_eq` / `gin_delete`)
over a dedicated `gin` dataset (§4), the regex + window benchmarks, and the **concurrent-reader
throughput** benchmarks (the `concurrent_read` kind, §8.1). Rule-based access-path work is pinned by
scratch workloads including `composite_pk_lookup`, `interval_set_pk`, and
`bounded_index_limit`; P6b adds `order_only_index_limit` and `gist_range_select` while reusing
`gin_contains` and `interval_set_pk` for its cross-method matrix. `join_inl_topn` covers the
combined join rule, while P7 adds the reversed-source-order
`join_reverse_inl` / `join_reverse_nested_fallback` pair alongside `gin_inl` and the
`gist_inl` / `gist_inl_nested_fallback` pair for opclass sibling bounds, and the
`hash_join_equijoin` / `hash_join_nested_fallback` pair. `order_by_limit`
is the permanent blocking-sort top-k lane: one million fixed
rows, K=100, cross-engine checksum equality, and the scan-dominated timing/memory payoff. This document is the
canonical record for the `bench/` subsystem.

**Native Node wrapper experiment (2026-07-16).** `impl/node` and
`bench/ts/src/bench-node-rust.ts` provide a Node-API wrapper around the safe Rust core and run all 53
jed lanes against the pure TypeScript core. The same-host control run also included the native Rust
harness: all **159 results agreed on answer checksums**. Across the 49 single-process lanes, the
wrapper was 1.38× faster by geometric mean, but that aggregate hides the product-relevant split: the
13 sub-100 µs lanes were effectively tied (0.98× wrapper speedup; TypeScript won 8), the 36 heavier
lanes favored the wrapper by 1.55×, and pure TypeScript won the five write lanes by 2.04× geometric
mean. Rust-owned reader threads made the wrapper 3.76× faster across the four concurrent lanes. The
Node boundary itself cost 2.01× over native Rust by geometric mean on the single-process lanes. These
results do not select the production Node package; §7.3 records the experiment and its limits.

**Top-k result (2026-07-13).** On the permanent `order_by_limit` lane, every jed core and
PostgreSQL returned checksum `6350e1a54bbefa1d`. Median per-query time moved from the 2026-07-01
pre-top-k run to **145 ms Go** (2.07 s before, −93%), **140 ms Rust** (643 ms before, −78%), and
**246 ms TypeScript** (1.86 s before, −87%); PostgreSQL was 24 ms on the same host. The blocking scan
still visits one million rows, but only K=100 retained rows reach the final sort — a 10,000× bound on
sort candidates — and the per-core spill tests prove this fixed-width K creates no run while a
smaller `work_mem` falls back to the existing external sorter. Timings remain non-gating; checksum,
corpus results/costs, and the no-run/fallback invariants are the correctness proof.

**GiST sibling-INL result (2026-07-13).** The permanent `gist_inl` /
`gist_inl_nested_fallback` pair joins ten scalar probes to 50,000 inner rows and returns the same
500-row result per iteration (checksum `342dce43410acbad`). The bare equality uses jed's scalar GiST
sibling bound; equivalent paired inequalities force the nested-loop reference. Median per-query time
was **0.28 ms bounded vs 47.6 ms nested Go** (171×), **0.30 ms vs 53.0 ms Rust** (178×), and
**1.09 ms vs 137 ms TypeScript** (126×). The PostgreSQL lane creates an ordinary B-tree because
its scalar GiST opclass requires the optional `btree_gist` extension; its drivers returned the same
checksum.
The scratch setup begins with explicit drops, so the pair is repeatable across sequential language
harnesses. Timings remain non-gating; the checksum, shared GiST range/scalar corpus, and NoREC
optimized/fallback relations are the correctness proof.

**Hash-join result (2026-07-13).** The permanent `hash_join_equijoin` /
`hash_join_nested_fallback` pair joins two 2,500-row inputs into the same 12,500-row logical result
(checksum `bcb67a58737e9b73`), with the fallback's `l.k + 0 = r.k` deliberately defeating the hash
rule. Median per-query time was **3.0 ms hash vs 436 ms nested Go** (147×), **3.4 ms vs 464 ms Rust**
(136×), and **9.1 ms vs 852 ms TypeScript** (94×). PostgreSQL was 0.79/0.84 ms and chose its own
optimizer plan for both spellings. Timings are non-gating; cross-engine checksum equality, exact
costs, collision invariants, and the NoREC optimized/fallback relation are the correctness proof.

**P7 two-relation result (2026-07-13).** The new permanent `join_reverse_inl` /
`join_reverse_nested_fallback` pair puts the indexed 50,000-row relation first in SQL and the
250-row driver second. P7 reverses the physical order and performs 250 PK probes; the equivalent
`i.id + 0 = d.k` spelling makes INL ineligible and supplies the nested-loop reference. Both return
the same 250 logical matches and checksum `bfa69a736f5387bd`, including PostgreSQL. Median per-query
time was **12.1 ms INL vs 774 ms nested Go** (64×), **7.55 ms vs 1.04 s Rust** (138×), and
**0.995 ms vs 4.54 s TypeScript** (4,563×). The companion FROM-order `join_inl_topn` lane retained
checksum `836953ce88391bbd`, while the hash/nested pair retained checksum `bcb67a58737e9b73`.
Together those permanent lanes exercise both physical orientations and all three P7 algorithms;
the shared plan/cost corpus remains the deterministic selection proof and timings remain non-gating.

**P6b selector result (2026-07-13).** The native before/after run covered `gin_contains`,
`interval_set_pk`, `order_only_index_limit`, and `gist_range_select`. Every core retained the same
per-lane checksum (`1cee697762ea2e7f`, `e8e4cd343f1e418c`, `c1289e7ca1211ba5`, and
`9a99571dc60b90c5`, respectively). A reversed-order repeat of the longest GIN lane measured
**287 µs vs 280 µs Go (+2.3%)**, **226 µs vs 218 µs Rust (+3.5%)**, and **450 µs vs 449 µs
TypeScript (+0.1%)** after versus before. The other lanes retained their access shapes; their
single-run variation was non-gating. The shared plan/cost corpus is the selection proof, while these
checksums establish unchanged results across the newly cost-selected paths.

**Path-B final rerun (2026-07-13).** Fresh format-v29 jed datasets were generated from the committed
benchmark definitions, and all 20 access, join, and statistics lanes were rerun directly on the
native Go, Rust, and TypeScript harnesses. Every per-lane checksum agreed across the three cores.
Representative final medians were `gin_contains` **257/218/426 µs**, `join_reverse_inl`
**187/155/551 µs** versus its
results-identical nested reference **830 ms/1.35 s/4.59 s**, and the 3/5/9-relation lanes
**188/271/119 µs Go**, **95/208/78 µs Rust**, and **274/769/281 µs TypeScript**. The skew statistics
pair retained checksum `52e015d7a68673bd` and improved after ANALYZE by **18.7× Go, 8.2× Rust, and
5.7× TypeScript** in this rerun. These environment-relative timings do not replace the phase-local
before/after records above; they verify that the final integrated branch retains their plan shapes,
answers, and performance direction.

**Point-lookup ramp/hot split baseline (2026-07-14).** The former `point_lookup_pk` number mixed
steady prepared execution with the first fault of roughly 5,000 packed leaves: 2,000 random warmup
probes could not populate a roughly 6,900-leaf working set. It is now the explicitly named
`point_lookup_pk_ramp` lane, while `point_lookup_pk` warms with 50,000 probes before measuring. Both
retain the same SQL, million-row dataset, seed, generator, expected-row check, and cross-engine
checksum; because the longer warmup consumes more of the parameter stream, the measured checksums
are respectively `f82d3b99ddaff0fb` and `28f09c46d56e242a`. Result schema 2 adds p90/p99 so the
population tail is visible instead of being hidden by mean+p50. Before the packed-key/direct-point
follow-ons, the ramp mean / p50 / p90 was **9.25 / 2.38 / 54.1 µs Go**, **6.52 / 2.92 / 37.0 µs
Rust**, and **18.6 / 7.95 / 90.2 µs TypeScript**; the fully-hot values collapsed to **2.41 / 2.19 /
3.02 µs**, **2.83 / 2.82 / 3.36 µs**, and **7.77 / 7.19 / 8.81 µs** respectively. Same-language
SQLite hot means were **6.72 µs** (`mattn-cgo`), **2.78 µs** (`rusqlite`), and **6.47 µs**
(`node:sqlite`). The split proves the diagnosis directly: Go and Rust hot means are close to p50,
while the ramp p90 records the first-fault work the representation slice targets. Timings remain
non-gating; both lanes' checksum agreement is the correctness gate.

**Point-lookup optimization result (2026-07-14).** The completed follow-ons keep clean packed-leaf
keys block-backed with lazy weights, use one counted B+tree descent and one row reconstruction for a
complete primary-key equality, share cached bind labels/result metadata, validate cached estimator
inputs through normalized keys, and avoid constructing unused frozen-session state. The final shared
run produced:

| Lane / native core | Mean | p50 | p90 | p99 |
|---|---:|---:|---:|---:|
| ramp — Go | 8.326 µs | 1.786 µs | 51.161 µs | 66.532 µs |
| ramp — Rust | 5.632 µs | 2.162 µs | 34.828 µs | 37.017 µs |
| ramp — TypeScript | 13.884 µs | 6.781 µs | 62.523 µs | 72.699 µs |
| hot — Go | 2.015 µs | 1.905 µs | 2.442 µs | 7.381 µs |
| hot — Rust | 2.183 µs | 2.149 µs | 2.457 µs | 2.685 µs |
| hot — TypeScript | 6.659 µs | 6.098 µs | 7.822 µs | 12.000 µs |

Against the split baseline above, ramp mean improved by about **10% Go, 14% Rust, and 25%
TypeScript**; hot mean improved by **16%, 23%, and 14%**. Same-run SQLite hot means were **6.175
µs** (`mattn-cgo`), **2.775 µs** (`rusqlite`), and **6.714 µs** (`node:sqlite`), so every native jed
core is now at or below its same-language SQLite mean as well as its PostgreSQL client mean on this
machine. This is an environment-relative result, not a performance contract. Every implementation
retained checksum `f82d3b99ddaff0fb` for ramp and `28f09c46d56e242a` for hot. Follow-up runs also
covered `composite_pk_lookup`, `secondary_lookup`, expression/partial lookup, and primary/secondary
`in_list` lanes; their reporters found no cross-core/engine answer mismatch.

Temporary, non-checked-in allocation probes used identical before/after boundaries around a fully
drained prepared query. Rust counted allocator calls by harness phase (argument bind, cursor open,
drain/checksum); Go used `testing.AllocsPerRun` around the internal prepared cursor; TypeScript summed
V8 `--trace-gc-nvp` allocation bytes plus the final residual after a forced-GC start. They are review
evidence rather than a test gate:

| Core / probe | Constant | `id` point | Four-column point |
|---|---:|---:|---:|
| Rust allocations/op | 29 → 24 | 47 → 35 | ~62 → 41 |
| Rust bytes/op | 2,892 → 2,830 | 3,777 → 3,688 | ~4,676 → 3,906 |
| Go allocations/op | 11 → 8 | 22 → 18 | 26 → 22 |
| TypeScript allocated bytes/op | 9,423 → 5,025 | 15,421 → 10,893 | 18,118 → 13,535 |

The remaining allocations include owned public output values and the safe cursor lifetime; neither is
weakened for a benchmark. Allocation counts/bytes are diagnostic only and may move with compiler or
runtime versions.

**Cold page-checksum result (2026-07-15).** P0 kept the format-v29 CRC-32/IEEE byte contract and
replaced only its implementation machinery: Go uses the runtime-dispatched standard `hash/crc32`,
Node uses `node:zlib.crc32`, Rust uses safe slicing-by-8, and browser TypeScript retains the same safe
slicing backend without importing Node modules. The shared run in
`bench/results/20260715-021326` on an Intel Core Ultra 9 285K (Go 1.26.3, rustc 1.92.0, Node
24.16.0) produced:

| Lane / native core | Mean | p50 | p90 | p99 |
|---|---:|---:|---:|---:|
| ramp — Go | 2.953 µs | 2.208 µs | 5.543 µs | 9.215 µs |
| ramp — Rust | 2.926 µs | 2.103 µs | 7.566 µs | 10.882 µs |
| ramp — TypeScript | 8.548 µs | 6.885 µs | 11.931 µs | 20.521 µs |
| hot — Go | 1.965 µs | 1.950 µs | 2.376 µs | 3.550 µs |
| hot — Rust | 2.072 µs | 2.045 µs | 2.342 µs | 2.590 µs |
| hot — TypeScript | 6.845 µs | 6.287 µs | 8.285 µs | 12.772 µs |

Against the 2026-07-14 final point-lookup run above, ramp mean fell by **65% Go, 48% Rust, and
38% TypeScript**; ramp p90 fell by **89%, 78%, and 81%** respectively. The fully-hot lane stayed
within ordinary single-run variation. All native cores retained ramp checksum `f82d3b99ddaff0fb`
and hot checksum `28f09c46d56e242a`, and the existing golden/corruption suites remained green. The
timings are observational, while identical file bytes, answers, and corruption behavior are the
correctness gates.

**Zero-copy PAX directory result (2026-07-15).** Cold-fault P1 kept the complete fault-time scan of
every key and variable-value end-offset directory, but retained those directories as byte
ranges/payload offsets in the packed page rather than decoded `N`-entry integer arrays. That removes
`1 + V` per-leaf allocations for a leaf with `V` variable-width columns without changing the file,
cost, result, or corruption-timing contracts. The shared run in
`bench/results/20260715-042240` produced:

| Lane / native core | Mean | p50 | p90 | p99 |
|---|---:|---:|---:|---:|
| ramp — Go | 2.942 µs | 2.150 µs | 4.896 µs | 9.993 µs |
| ramp — Rust | 2.798 µs | 2.077 µs | 7.527 µs | 9.586 µs |
| ramp — TypeScript | 7.960 µs | 6.631 µs | 10.925 µs | 15.995 µs |
| hot — Go | 1.989 µs | 1.857 µs | 2.398 µs | 7.406 µs |
| hot — Rust | 2.000 µs | 1.977 µs | 2.198 µs | 2.433 µs |
| hot — TypeScript | 6.371 µs | 5.870 µs | 7.158 µs | 12.157 µs |

Against P0 above, ramp mean was effectively flat in Go, improved about **4% in Rust**, and improved
about **7% in TypeScript**; TypeScript ramp p99 improved about **22%**, while Rust ramp p99 improved
about **12%**. Hot medians/means remained within ordinary run variation; Go's isolated hot p99 is a
single-run tail outlier, not a claimed regression or contract. All native cores again retained ramp
checksum `f82d3b99ddaff0fb` and hot checksum `28f09c46d56e242a`. The allocation removal follows directly
from the retained structure (`1 + V` integer arrays no longer exist); timings remain observational.

**Bounded buffer-pool index reservation result (2026-07-15).** P2 improvement 1 gives the Rust and
Go page-id indexes a bounded initial capacity hint of `min(cache_leaves, 8192)`. The bound covers the
roughly 6,900 leaves populated by `point_lookup_pk_ramp`, but does not turn the default 32,768-leaf
ceiling—or a caller's much larger configured ceiling—into an unbounded eager allocation. Five fresh
opens before and after on the same Intel Core Ultra 9 285K host (Go 1.26.3, rustc 1.92.0) produced
these medians:

| Core / `point_lookup_pk_ramp` | Mean | p50 | p90 | p99 |
|---|---:|---:|---:|---:|
| Go before | 3.009 µs | 2.232 µs | 5.643 µs | 9.169 µs |
| Go after | 2.987 µs | 2.203 µs | 5.741 µs | 9.540 µs |
| Rust before | 2.896 µs | 2.131 µs | 7.595 µs | 10.139 µs |
| Rust after | 2.935 µs | 2.187 µs | 7.780 µs | 10.192 µs |

The end-to-end movements are all below the benchmark subsystem's 5% noise floor; the few eliminated
rehashes are diluted across 50,000 measured probes. A focused 6,900-page pool-population probe makes
the intended effect visible: Go's median moved from **361.5 to 200.2 µs**, **69 to 51 allocations**,
and **1,042,895 to 1,010,336 allocated bytes** per population; Rust moved from **239.6 to 133.8 µs**
and from **12 index growths to zero**. Rust's deterministic unit test pins the no-growth property and
Go retains the focused allocation benchmark. Both end-to-end lanes kept checksum `f82d3b99ddaff0fb`;
timings and runtime allocation counts remain observational.

**Rust positioned-read result (2026-07-15).** P2 improvement 2 replaces the Rust file host's
`seek` + `read_exact` page reads with safe standard-library positioned I/O: Unix uses
`FileExt::read_exact_at`, Windows uses an exact-read loop over `FileExt::seek_read`, and targets without
either trait keep the correct serialized cursor-based fallback. This changes no page buffer, file byte,
cost, result, or error contract. On the same Linux / Intel Core Ultra 9 285K host (rustc 1.92.0), five
runs of a temporary OS-cache-hot probe over random 8 KiB reads from `large.jed` produced:

| Focused file-host probe | Median time/read | Allocations/read | Allocated bytes/read |
|---|---:|---:|---:|
| `seek` + `read_exact` before | 872 ns | 1 | 8,192 |
| positioned read after | 781 ns | 1 | 8,192 |

The focused host operation improved **10.4%**; allocation count and bytes are intentionally unchanged
because `read_at` still returns one owned page buffer. A separate five-fresh-open
`point_lookup_pk_ramp` comparison showed that the small fault-only win is diluted below end-to-end
noise:

| Rust `point_lookup_pk_ramp` | Mean | p50 | p90 | p99 |
|---|---:|---:|---:|---:|
| before | 2.910 µs | 2.161 µs | 7.557 µs | 10.105 µs |
| after | 2.983 µs | 2.288 µs | 7.857 µs | 9.544 µs |

Every run retained checksum `f82d3b99ddaff0fb`. The focused allocation/time probe was temporary
instrumentation (the same review-evidence status as the prepared-lookup allocation probes above),
while the permanent unit test pins exact reads, short-read `58030`, and cursor preservation on the
positioned platforms.

**Concurrent cold-fault single-flight result (2026-07-15).** P2 improvement 3 removes the global
buffer-pool mutex from the physical-read + checksum + PAX-parse body in the threaded cores. Rust and
Go now register one in-flight load per page under a short pool critical section: distinct pages decode
concurrently, same-page callers share one result, and commit invalidation makes an in-flight old decode
non-cacheable and detaches it from post-invalidation callers, so page-id reuse cannot leave a stale
cache entry. The pager lock still serializes the physical read against commit writes; TypeScript
remains single-threaded. Permanent unit tests pin the overlap/single-flight/invalidation properties,
and the existing concurrent file reader/writer tests retain snapshot isolation (Go also passed under
`-race`).

The shared benchmark corpus gains `concurrent_read_pk_cold_r1` and `_r4`: 20,000 random lookups over
the million-row table after only 2,000 warmup probes. Its roughly 6,900-leaf working set fits in the
default pool, so the measured phase populates the cache without mixing in eviction thrash. The paired
baseline (`bench/results/20260715-133613`) and final-tree run
(`bench/results/20260715-135749`) on the Intel Core Ultra 9 285K host (Go 1.26.3, rustc 1.92.0)
produced:

| Core / cold lane | Before | After | Change |
|---|---:|---:|---:|
| Go r1 | 35.903 µs/op | 36.683 µs/op | +2.2% (noise) |
| Go r4 | 11.220 µs/op | 10.212 µs/op | -9.0% |
| Rust r1 | 31.015 µs/op | 31.273 µs/op | +0.8% (noise) |
| Rust r4 | 10.154 µs/op | 9.078 µs/op | -10.6% |
| TypeScript r1 | 64.754 µs/op | 64.634 µs/op | -0.2% (noise) |
| TypeScript r4 | 60.811 µs/op | 61.996 µs/op | +1.9% (noise) |

The r1 and r4 checksums were respectively `f086f7634186c3f4` and `c72e6d5fabdc504e` in every
native core. Wall-clock movement is observational and includes parse/plan work on every query; the
focused synchronization tests, rather than a timing threshold, are the regression gate.

**INSERT performance slice-0 baseline (2026-07-16).** Before changing INSERT execution or tree
ownership, commit `e5f8bed3` was measured on Linux 6.17 / glibc 2.39 on an Intel Core Ultra 9 285K
with rustc 1.92.0, Go 1.26.3, and Node 24.16.0. Each `insert_rollback` sample is one measured
transaction containing 1,000 executions of the prepared four-column INSERT into `small.orders`,
then rollback. Runs were pinned to CPU 2 and alternated by pair; one shared/exclusive warmup pair was
discarded, then five pairs were retained. Go and TypeScript used the same discarded-warmup + five-run
shape. Each cell below is the median of that statistic across the five retained runs:

| Core / locking | Mean | Minimum | p50 | p90 | p99 |
|---|---:|---:|---:|---:|---:|
| Rust / shared | 24.546 ms | 23.980 ms | 24.547 ms | 24.696 ms | 24.769 ms |
| Rust / exclusive | 19.376 ms | 19.143 ms | 19.353 ms | 19.537 ms | 19.566 ms |
| Go / auto (shared) | 12.174 ms | 11.888 ms | 12.119 ms | 12.418 ms | 12.535 ms |
| TypeScript / auto (shared) | 18.880 ms | 12.467 ms | 15.389 ms | 28.379 ms | 35.624 ms |

Every retained run produced checksum `ac02f0205c4f05c5`. Rust shared was **26.7% slower** than
exclusive by the median mean (26.8% by p50), establishing the glibc/background-thread attribution
before any INSERT optimization. The TypeScript tail is visibly GC-sensitive; timings remain
observational. The Rust harness accepts benchmark-only `JED_BENCH_LOCKING=auto|shared|exclusive|none`
so later paired runs do not require source patches; absence keeps the normal `auto` default.

A separate temporary Rust counting-allocator probe (removed after collection) bounded the same
1,000-row transaction and attributed approximately:

| Phase | Allocation/reallocation calls | Requested bytes |
|---|---:|---:|
| parameter binding and pre-`insert_rows` resolution | 38,000 | 3.3 MiB |
| INSERT validation and temporary structures | 370,000 | 50 MiB |
| table and secondary-index tree mutation | 306,000 | 39 MiB |
| total | 716,000 | 93 MiB |

About 75% of calls requested 16 bytes or less. Requested bytes are cumulative allocator traffic, not
retained memory, and the counting allocator perturbs timing, so these figures are attribution evidence
only and are not compared to Go or V8 object counts.

Slice 0 also pins the immutable-tree preconditions before optimization. The mirrored `split_shape`
tests cover table leaf/interior splits and secondary-index splits by exact costs, then overwrite and
append in a working root (costs 477/199), roll back, and require both the byte-exact committed image
and original costs (265/105) to return. The mirrored `PMap` tests retain the complete encoded-key/value
sequence across heavy clone mutation. Streaming tests pin a committed root through 64 same-leaf
inserts and pin a write-transaction cursor while later statements mutate its working root; attachment
tests do the same for main and attached roots simultaneously. The existing `pk_table.jed`,
`tall_tree.jed`, `index_table.jed`, and `max_sep_table.jed` fixtures independently pin the leaf,
interior, ordinary-index, and degenerate-index shapes through Rust, Go, TypeScript, and the Ruby
encoder/decoder. `cte/data_modifying_errors.test` already pins the phase-2 insert/insert collision
hidden by a writable-CTE read pin, while the mirrored writable-CTE tests pin same-leaf lexical
last-write behavior. As a mutation-sensitivity check, temporarily returning decoded TypeScript leaf
arrays without shallow copies made `pmap: clone is an independent snapshot` fail on the first
overwritten value; restoring the immutable copy made the suite pass. No experimental mutation remains.

**INSERT performance slice-1 result (2026-07-16).** Commit `e5f8bed3` remains the before control;
the slice-1 working tree specializes exactly one plain `INSERT ... VALUES` candidate and retains the
batch path for multi-row, `INSERT ... SELECT`, and `ON CONFLICT`. The same CPU-2 pin, discarded
process warmup, and five retained runs used by the slice-0 baseline produced the following medians of
per-run statistics. Every run retained checksum `ac02f0205c4f05c5`.

| Core / locking | Mean | Change | Minimum | p50 | p90 | p99 |
|---|---:|---:|---:|---:|---:|---:|
| Rust / shared | 23.928 ms | -2.5% | 23.681 ms | 23.910 ms | 24.162 ms | 24.184 ms |
| Rust / exclusive | 19.057 ms | -1.6% | 18.812 ms | 19.011 ms | 19.189 ms | 19.207 ms |
| Go / auto (shared) | 11.929 ms | -2.0% | 11.695 ms | 11.890 ms | 12.100 ms | 12.251 ms |
| TypeScript / auto (shared) | 16.706 ms | -11.5% | 10.549 ms | 14.001 ms | 25.296 ms | 31.379 ms |

The Rust shared/exclusive gap is still **25.6% by mean** (25.8% by p50), only slightly below the
slice-0 26.7%. Slice 1 therefore improves the common statement shape but does **not** solve the
glibc/background-thread regression; the Rust CoW entry-sharing and prepared-INSERT slices remain the
material follow-ons. A freshly rebuilt Node/Rust wrapper measured 29.489 ms mean (29.205 ms p50) on
the same lane and checksum, versus 16.706 ms pure TypeScript and 23.928 ms native Rust/shared; this is
the wrapper regression subset, not a claim about all Node workloads.

Temporary probes were run outside the timing samples, then removed. Go's `runtime.MemStats` boundary
covered the 1,000 inserts but excluded `BEGIN`/`ROLLBACK`; a forced batch-path control used the same
post-slice source. V8 `--trace-gc-nvp` covered the complete 3-warmup + 30-measured process and used
forced collections only to bound the first and residual allocation intervals. Rust used the same
counting-allocator boundary as slice 0.

| Core / probe | Forced batch path | Single-row path | Effect |
|---|---:|---:|---:|
| Rust calls / transaction | 707,057 | 707,057 | whole-lane counter indistinguishable |
| Rust requested bytes / transaction | 92,492,630 | 92,492,630 | whole-lane counter indistinguishable |
| Go mallocs / transaction | 85,144 | 80,144 | -5,000 (-5.9%; five per row) |
| Go allocated bytes / transaction | 44,866,920 | 44,722,872 | -144,048 (-0.3%) |
| Go GCs / transaction, median | 19 | 19 | unchanged |
| V8 allocated bytes / complete lane | 2,057,753,168 | 2,012,076,272 | -45,676,896 (-2.2%) |
| V8 natural GCs / complete lane | 51 (48 scavenges) | 50 (47 scavenges) | one fewer scavenge |

Rust's immutable B+tree rebuilding dominates the transaction counter; its reason to retain this
slice is the repeatable elapsed-time reduction and the explicit one-row control flow, not a claimed
allocator win. Go and TypeScript directly demonstrate the removed batch-temporary traffic.

A temporary scratch-only matrix (not added to the permanent cross-engine corpus) exercised the
fast path without secondary indexes; with ordinary, unique, expression, and partial indexes;
integer-only, variable-text, and 16 KiB long-value rows; transaction batches of 1/10/100/1,000; and
a true ten-candidate statement forced through the retained batch path. Times are mean transaction
latencies; every core returned the zero-row rollback checksum `4eb8c6181d9224ca` in every lane.

| Scratch lane | Go | Rust | TypeScript |
|---|---:|---:|---:|
| no secondary index, 1,000 rows | 3.232 ms | 10.534 ms | 5.973 ms |
| four mixed secondary indexes, 1,000 rows | 21.242 ms | 42.053 ms | 26.059 ms |
| integer-only, 1,000 rows | 3.195 ms | 8.739 ms | 4.765 ms |
| variable text, 1,000 rows | 2.557 ms | 5.682 ms | 12.892 ms |
| 16 KiB text, 100 rows | 10.860 ms | 10.956 ms | 271.393 ms |
| transaction batch 1 / 10 / 100 / 1,000 | 0.002 / 0.023 / 0.259 / 3.229 ms | 0.003 / 0.018 / 0.468 / 10.417 ms | 0.017 / 0.027 / 0.309 / 3.634 ms |
| ten-row statement × 100 | 3.599 ms | 13.249 ms | 3.316 ms |

Finally, three same-host process pairs compared the pre-slice engine with slice 1. The disposable
control worktree carried only the Go/Rust benchmark-handle lifetime fixes required by shared locking;
engine code stayed at `e5f8bed3`. Each cell is the delta between the median before and median after
process. All before/after checksums matched; the largest regression was +3.72%, inside the 5% gate.

| Regression lane | Go | Rust | TypeScript |
|---|---:|---:|---:|
| hot prepared PK lookup | -2.40% | +2.58% | -10.91% |
| cold PK ramp | +1.02% | +1.06% | -1.00% |
| full scan aggregate | -0.44% | -1.99% | -0.78% |
| resident concurrent reader | -0.70% | +1.35% | -0.49% |
| durable one-row commit | -1.80% | +3.72% | -6.70% |
| secondary-index UPDATE | -0.38% | +0.20% | -2.69% |
| secondary point-set DELETE | +0.62% | -0.37% | +0.77% |

The measurements also found and fixed a Go benchmark lifecycle issue exposed by shared locking:
each run now closes its owning `Database`, and concurrent readers are minted from that already-open
handle instead of attempting an illegal second in-process open.

**INSERT performance slice-2 result (2026-07-16).** Commit `606285f1` is the Slice-1 control: its
prepared statement reuses the parsed INSERT AST but deliberately has no DML-resolution cache. The
Slice-2 engine adds a separate immutable prepared-INSERT cache for plain `INSERT ... VALUES`, while
retaining fresh resolution for `INSERT ... SELECT`, `ON CONFLICT`, writable CTEs, subqueries, and
precompiled-regex shapes. The existing CPU-2-pinned `insert_rollback` lane used five retained
old/current process pairs for Go and TypeScript and six for Rust. Each cell is the median statistic
across those process results; every run retained checksum `ac02f0205c4f05c5`.

| Core / locking | Slice 1 mean | Slice 2 mean | Change | Minimum | p50 | p90 | p99 |
|---|---:|---:|---:|---:|---:|---:|---:|
| Rust / shared | 24.060 ms | 22.652 ms | -5.8% | 22.429 ms | 22.648 ms | 22.807 ms | 22.865 ms |
| Go / auto (shared) | 11.976 ms | 11.643 ms | -2.8% | 11.398 ms | 11.623 ms | 11.818 ms | 11.900 ms |
| TypeScript / auto (shared) | 17.020 ms | 15.879 ms | -6.7% | 10.713 ms | 13.221 ms | 23.631 ms | 29.152 ms |

The cache is exercised inside the lane's explicit transaction: the first INSERT may fill only when
the visible target schema exactly matches the committed schema, and the remaining 999 executions
hit. Working DDL cannot fill or overwrite an older committed entry. Estimator revisions are absent
from the DML signature because successful INSERTs advance them; database/attachment identity,
catalog generation, lowercased target name, and temp-shadow state remain validity inputs. Exact
cache-hit tests in all three cores cover DDL, drop/recreate, detach/reattach, collation upgrade,
another core, temp shadowing, rollback restoration, privileges, read-only state, result/error/cost,
and serialized tree bytes. Go additionally races concurrent sessions against the atomic cache slot.

One-shot execution was measured separately with temporary benchmark-only routing and then removed.
It averaged 13.759 ms Go, 25.284 ms Rust, and 18.623 ms TypeScript: respectively 18.2%, 11.6%, and
17.3% slower than the cached prepared lane. These one-shot values include parsing as well as DML
resolution and therefore are **not** used to attribute the cache's pure resolution savings; the
Slice-1 prepared control above is the attribution baseline.

Temporary allocation probes were also removed after collection:

| Core / probe | Slice 1 / fresh resolution | Slice 2 / cached | Effect |
|---|---:|---:|---:|
| Rust allocator calls / transaction | 644,625 | 619,625 | -25,000 (-3.9%; 25 per row) |
| Rust requested bytes / transaction | 91,781,589 | 88,895,589 | -2,886,000 (-3.1%; 2,886 per row) |
| Go allocations / transaction | 36,547 | 25,546 | -11,001 (-30.1%; 11 per row) |
| Go allocated bytes / transaction | 29,136,910 | 28,520,862 | -616,048 (-2.1%) |
| V8 allocated bytes / complete lane | 1,988,316,072 | 1,945,251,584 | -43,064,488 (-2.2%) |
| V8 scavenges / complete lane | 48 | 46 | two fewer |

The Go allocation comparison clears only the DML cache between executions, so both sides retain the
same prepared AST. The Rust counts bound the same 1,000-row transaction. V8 tracing covers one full
harness process and is allocation/GC evidence rather than object-count equivalence with native cores.

Three paired result processes sampled unrelated hot lookup, cold ramp, and full-scan controls; three
durable-write pairs were sufficient for Rust/TypeScript, while Go's noisy fsync lane was extended to
nine alternated pairs. All before/after checksums matched. Every median stayed within the 5% gate:

| Regression lane | Go | Rust | TypeScript |
|---|---:|---:|---:|
| hot prepared PK lookup | +3.33% | -1.06% | -0.41% |
| cold PK ramp | -2.96% | +0.40% | +0.99% |
| full scan aggregate + filter | +1.41% | -1.78% | -0.70% |
| durable one-row commit | +0.52% | +2.16% | -2.32% |

Finally, five exclusive Rust runs measured 18.006 ms mean (17.816 ms minimum, 18.003 ms p50,
18.118 ms p90, 18.173 ms p99). Slice 2's Rust/shared mean remains **25.8% slower** than exclusive.
The prepared cache materially reduces work but does not solve the glibc/background-thread gap; Rust
copy-on-write entry sharing remains the next material INSERT follow-on.

## 1. Purpose and non-goals

The benchmark suite answers two questions, continuously:

1. **Cross-core** — how do the three jed cores (Rust, Go, TS) compare against each other
   on the same workload?
2. **Cross-engine** — is jed's performance *at least tolerable* next to PostgreSQL and
   SQLite on the same workload?

This is **wall-clock** measurement and therefore deliberately **outside the conformance
contract and `rake ci`** (CLAUDE.md §10): timings are environment-relative and
nondeterministic, and must never gate a build. What *is* checked — loudly — is the
**answers**: every result carries a checksum of the returned rows, and the report fails
if any two engines/cores/drivers disagree for the same benchmark (§6). A benchmark run
is thus also a small differential test.

Non-goals: micro-benchmarks of internal functions (use `cargo bench`/`testing.B` ad hoc
if needed), benchmarking the deterministic *cost* units (cost is asserted exactly in the
conformance corpus — `spec/design/cost.md`), and adversarial true-parallelism *stress*
(random-schedule, invariant-checked — that is the Layer 3 stress runner below, not a
benchmark). *Wall-clock* concurrent-reader **throughput** — once a non-goal pending
file-backed concurrent sessions — **has since landed** as the `concurrent_read` kind (§8.1),
now that the slice-7 convergence (session.md §2.4/§10) gives a shared `Database` minting
concurrent reader `Session`s. **Correctness**-under-concurrency is *also* covered, by a
sibling bench-family harness: the Layer 3 stress runner (`spec/design/concurrency-testing.md`
§6) shares these modules' machinery (the splitmix64 PRNG, the FNV-1a answer checksum) via a
`stress` binary per core, run by `rake stress` — see §2.

## 2. Layout

```
bench/
  corpus/benchmarks.toml   # benchmark definitions (shared by every harness)
  corpus/datasets.toml     # dataset spec (shared by bench-setup and fingerprint checks)
  go/                      # Go harness module (jed-bench) — own go.mod, never impl/go's
  rust/                    # Rust harness package (jed-bench) — own Cargo.toml, never impl/rust's
  ts/                      # TS harness package — own package.json, never impl/ts's
  go/cmd/stress, rust/src/bin/stress.rs, ts/src/stress.ts   # Layer 3 concurrency-stress runner
                           #   (concurrency-testing.md §6); reuses the PRNG + checksum below
  data/                    # GITIGNORED: generated {small,large}.{jed,sqlite} + *.fingerprint
  results/                 # GITIGNORED: <UTC-stamp>/<lang>-<binary>.jsonl per run (+ stress/)
stress/*.stress.toml       # Layer 3 stress workloads (run by `rake stress`)
scripts/bench_report.rb    # aggregator (rake bench:report)
```

PostgreSQL benchmark data lives in the live `db` service (databases `jed_bench_small`,
`jed_bench_large`, `jed_bench_scratch`), reached over the Unix socket like the oracle
(`PGHOST=/var/run/postgresql`, trust auth).

The corpus is data, the harnesses are code: each harness parses the same two TOML files
and runs the same benchmarks; only the engine driver differs per binary (§7). New
benchmarks are added by editing `benchmarks.toml` (and, if they need new data,
`datasets.toml` + a `generator_version` bump — §5).

## 3. Benchmark corpus — `bench/corpus/benchmarks.toml`

```toml
schema_version = 1

[[bench]]
name        = "point_lookup_pk"     # fully-hot lane; point_lookup_pk_ramp keeps the short warmup
description = "Fully-hot PK point lookup on 1M rows after warming the leaf working set"
dataset     = "large"               # "small" | "large" | "scratch" (§8)
kind        = "query"               # "query" | "write_rollback" | "write_durable" | "concurrent_read" (§8.1)
sql         = "SELECT id, customer_id, amount, note FROM orders WHERE id = $1"
warmup      = 50000                 # enough random probes to touch essentially every leaf
iterations  = 50000                 # timed iterations
seed        = 4201                  # splitmix64 seed for this bench's param stream (§4)

[[bench.param]]                     # one entry per $N, ascending
gen = "int_uniform"                 # "int_uniform" | "serial" | "text" | "int_window"
min = 1                             # int_uniform: inclusive bounds
max = 1000000
```

Optional keys:

- `expect_rows_per_iter = N` — sanity gate: abort if any measured iteration returns a
  different row count.
- `engines = ["postgres", ...]` — allowlist; default is all three. The escape hatch for
  a benchmark only some engines can run.
- `batch = N` — write kinds only: statements per iteration (§8).
- `readers = N` — `concurrent_read` only: the number of reader `Session`s minted from the
  one shared `Database` (§8.1).
- `setup_sql = ["..."]` — write kinds only: statements run once before warmup.
- `[bench.sql_override]` / `[bench.setup_sql_override]` — per-engine SQL text keyed by
  `jed` / `postgres` / `sqlite`, used only where dialects genuinely diverge (v1 uses it
  once, for the scratch table's SQLite rowid-pk DDL — §8).

Param generators: `int_uniform` (`min`/`max`, drawn as `min + next() % (max-min+1)`),
`serial` (`start`; a monotonic counter that does **not** consume the PRNG — collision-free
ids), `text` (`min_len`/`max_len`; a length draw then per-char draws — §4), and `int_window`
(`base`/`off_min`/`off_max`; the value of an **earlier** param at index `base` plus
`int_uniform(off_min, off_max)` — a selective fixed-width range around a base param, e.g.
`col BETWEEN $1 AND $2` with `$2 = $1 + [off_min, off_max]`, both endpoints const-sources so
the range pushes down to an index bound).

**Placeholder policy.** SQL uses `$N` with first occurrences in ascending order. jed and
PostgreSQL bind `$N` natively; SQLite harnesses mechanically rewrite `$N` → `?N` at
prepare time (`?N` is SQLite's explicit-numbered positional form, unambiguous across
rusqlite, the Go drivers, and `node:sqlite`).

**Ordering rule.** Any benchmark query returning more than one row must carry a
**total-order `ORDER BY`** — ties broken explicitly (by `id`), because jed's implicit
primary-key tie-break (§8 of CLAUDE.md) is *not* shared by PostgreSQL or SQLite and the
checksum (§6) compares rendered rows in order.

**Type rule.** Stick to the common subset: jed's `smallint`/`integer`/`bigint` aliases
parse in all three engines, so one SQL text serves all of them. Aggregates were chosen so
result types match too (jed `SUM(i32) → i64` = PG `sum(integer) → bigint`).

## 4. Dataset spec and deterministic generation — `bench/corpus/datasets.toml`

```toml
schema_version    = 1
generator_version = 1     # BUMP whenever generation behavior changes (part of the fingerprint)

[[dataset]]
name = "small"            # → bench/data/small.{jed,sqlite}, PG database jed_bench_small

[[dataset.table]]
name = "orders"
rows = 10000
seed = 101                # one splitmix64 stream per table

  [[dataset.table.column]]
  name = "id"
  type = "i64"          # i64 | i32 | i16 | text | i64[] | i32[] | i16[]
  gen  = "serial"         # serial | int_uniform | text | int_array
  primary_key = true

  [[dataset.table.column]]
  name = "customer_id"
  type = "i32"
  gen  = "int_uniform"
  min  = 1
  max  = 1000

  [[dataset.table.index]]
  name    = "orders_customer_idx"
  columns = ["customer_id"]
  # optional method  = "gin"   # default "" = ordered btree; "gin" → CREATE INDEX ... USING gin
  # optional engines = [...]   # allowlist; default: every engine gets the index
```

A table may also carry a `engines = [...]` allowlist (same shape as an index's): an empty
list means every engine, a non-empty one restricts the table to those engines and
bench-setup skips it elsewhere. The `gin` dataset's `docs` table is `["jed", "postgres"]`
— SQLite has neither an array type nor GIN, so no `gin.sqlite` is produced at all.

**Generation order is the determinism contract:** for each table, seed one splitmix64
stream with `table.seed`; for each row 1..rows, draw each non-`serial` column's value in
declared column order. Any language reproduces the dataset exactly from the spec. (Today
bench-setup is Go-only; the data files it produces are read by all three language
harnesses, so the generators below live in `bench/go` alone.)

**DDL is derived** from the spec per engine — never written as literal SQL — with this
fixed type map:

| spec type | jed | PostgreSQL | SQLite |
|---|---|---|---|
| `i64` + `primary_key` | `bigint PRIMARY KEY` | `bigint PRIMARY KEY` | `INTEGER PRIMARY KEY` |
| `i64` / `i32` / `i16` | same name | `bigint` / `integer` / `smallint` | `INTEGER` |
| `text` | `text` | `text` | `TEXT` |
| `i64[]` / `i32[]` / `i16[]` | `bigint[]` / `integer[]` / `smallint[]` | same as jed | — (array table is allowlisted to jed+postgres) |

The SQLite pk maps to `INTEGER PRIMARY KEY` (the rowid alias) deliberately: it is
SQLite's idiomatic fast path, and `BIGINT PRIMARY KEY` would unfairly force a separate
index. Fairness notes like this live here so a surprising number has a written
explanation.

### The shared PRNG — splitmix64

State is one u64 `z`. One step, all arithmetic wrapping to 64 bits:

```
next():
  z += 0x9E3779B97F4A7C15
  x = z
  x = (x ^ (x >> 30)) * 0xBF58476D1CE4E5B9
  x = (x ^ (x >> 27)) * 0x94D049BB133111EB
  return x ^ (x >> 31)
```

Draws:

- bounded int in `[lo, hi]`: `lo + next() % (hi - lo + 1)` (modulo bias accepted — it is
  deterministic and identical everywhere, which is all that matters here);
- text of length `[min_len, max_len]`: one bounded draw for the length, then per
  character `'a' + next() % 26`.
- `int_array` of length `[min_len, max_len]` with elements in `[elem_min, elem_max]`: one
  bounded draw for the length, then that many bounded element draws — the same
  "length-then-contents" shape as `text`. Rendered as the array text literal `'{1,2,3}'`
  (`'{}'` when empty) for the jed/SQL load, and passed as a native `[]int64` to PostgreSQL's
  `CopyFrom`. The `gin` dataset's `docs.tags bigint[]` column is the only user today.

Ruby reference (the snippet that generated the pinned vectors below):

```ruby
MASK = 0xFFFFFFFFFFFFFFFF
def splitmix64_stream(seed, n)
  z = seed & MASK
  Array.new(n) do
    z = (z + 0x9E3779B97F4A7C15) & MASK
    x = z
    x = ((x ^ (x >> 30)) * 0xBF58476D1CE4E5B9) & MASK
    x = ((x ^ (x >> 27)) * 0x94D049BB133111EB) & MASK
    x ^ (x >> 31)
  end
end
```

Pinned vectors — asserted by a unit test in every harness (Go
`bench/go/internal/bench/prng_test.go`, Rust `bench/rust/src/lib.rs`, TS
`bench/ts/tests/prng.test.ts`; TS implements the PRNG over `BigInt` masked to 64 bits):

| seed | first five outputs (hex) |
|---|---|
| `1` | `910a2dec89025cc1`, `beeb8da1658eec67`, `f893a2eefb32555e`, `71c18690ee42c90b`, `71bb54d8d101b5b9` |
| `1234567` | `599ed017fb08fc85`, `2c73f08458540fa5`, `883ebce5a3f27c77`, `3fbef740e9177b3f`, `e3b8346708cb5ecd` |

## 5. Fingerprints — setup once, regenerate only when stale

`fingerprint = sha256_hex(bytes of bench/corpus/datasets.toml)`. The file embeds
`generator_version`, so a behavioral change in `bench-setup` that doesn't touch the
dataset shape is still a one-line bump that invalidates everything.

Stored per engine after a successful load:

- jed / SQLite: sidecar files `bench/data/<dataset>.jed.fingerprint` /
  `<dataset>.sqlite.fingerprint` (hex + `\n`);
- PostgreSQL: row `('fingerprint', <hex>)` in table `_bench_meta(key text PRIMARY KEY,
  value text)` inside each `jed_bench_<dataset>` database.

`bench-setup` (run via `rake bench:setup`) skips any engine/dataset pair whose stored
fingerprint matches; `--force` regenerates unconditionally. **Every harness binary
verifies the fingerprint before running** and aborts with `stale benchmark data: run
'rake bench:setup'` on mismatch or absence — a benchmark can never silently run against
wrong data.

The fingerprint covers `datasets.toml` but **not** jed's on-disk format version: a format
bump (`spec/fileformat/format.md`) leaves the dataset spec untouched, so the SQLite and PG
databases (stable formats) correctly stay valid, but the `.jed` files — written by the core
at the old version — are now stale and the current core rejects them as `XX001`. So the jed
skip carries one extra gate: `bench-setup` skips a `.jed` file only when its fingerprint
matches **and the file actually opens** with the current core; an unreadable file is
regenerated regardless of the fingerprint. (SQLite/PG keep the plain fingerprint check.)
Folding the format version into the fingerprint would couple the bench module to an
unexported core constant per core; the open-it-and-see gate auto-heals on any format bump,
partial write, or corruption without that coupling.

## 6. Harness contract

Every benchmark binary takes the same positional arguments:

```
bench-<engine> <corpus_dir> <data_dir> <out_path> [name_filter_substring]
bench-setup    <corpus_dir> <data_dir> [--engine jed|sqlite|pg|all] [--force]
```

The Rust jed harness additionally accepts `JED_BENCH_LOCKING=auto|shared|exclusive|none` for paired
coordination attribution. It is benchmark-only and absent by default, so normal runs exercise the
public API's `auto` default.

PG binaries use the standard `PG*` environment (the devcontainer points it at the Unix
socket). Human-readable progress goes to stderr; results go to `out_path` as JSONL,
truncated on open. One JSON object (single line, keys in this order) per completed
benchmark:

```json
{"schema":2,"bench":"point_lookup_pk","dataset":"large","engine":"jed","lang":"go",
 "variant":"core","iterations":50000,"warmup":50000,"readers":0,"total_ns":312000000,
 "ns_per_op":6240,"min_ns":4100,"p50_ns":5900,"p90_ns":6700,"p99_ns":9100,
 "rows_total":50000,"checksum":"9f86d081884c7d65",
 "fingerprint":"<sha256 hex>","started_at":"2026-06-12T14:03:11Z"}
```

`readers` is the concurrency level (`concurrent_read` only; `0` for the other kinds). For
`concurrent_read`, `total_ns` is the **wall clock of the timed phase** (so `ns_per_op =
wall / iterations` is the *throughput* latency that falls as readers scale), and `min_ns` /
`p50_ns` / `p90_ns` / `p99_ns` are the merged per-query latency distribution across readers (§8.1).

- `engine` ∈ `jed | postgres | sqlite`; `lang` ∈ `go | rust | ts`; `variant` names the
  driver: `core` (jed), `pgx`, `postgres-crate`, `porsager`, `modernc`, `mattn-cgo`,
  `rusqlite`, `node-sqlite`. The comparison key is `(engine, lang, variant)`.
- Timing: per-iteration elapsed via the language's monotonic clock (Go `time.Now`, Rust
  `Instant`, TS `process.hrtime.bigint`); `ns_per_op = total_ns / iterations` (integer
  division). `min_ns`, `p50_ns`, `p90_ns`, and `p99_ns` come from the sorted samples at
  index `floor((N - 1) * percentile / 100)`; this keeps p50's historical lower-median definition.
  Schema 2 adds p90/p99 so cache-fault and GC tails remain visible; reporters continue to read
  schema-1 runs and render their missing tail fields as an em dash.
- `rows_total`: rows returned across measured iterations (0 for write kinds — their
  verification lives in the checksum).

### The answer checksum

**FNV-1a 64-bit** (offset basis `0xcbf29ce484222325`, prime `0x100000001b3`), folded over
the **measured** iterations only, emitted as 16 lowercase hex chars:

- per result value: hash the canonical rendering's UTF-8 bytes, then one `0x1F` byte;
- after each row: one `0x1E` byte;
- canonical rendering: NULL → `NULL`, integers → decimal string, text → its raw bytes.

For write kinds the checksum is the hash of the post-run sanity `count(*)` rendered the
same way (one value, one row) — `insert_rollback` proves the rollbacks held,
`insert_commit_durable` proves every commit landed.

For `concurrent_read` (§8.1) the checksum is **partition-folded** so it is identical
regardless of thread scheduling or reader count's effect on timing: the measured param
stream is split into `readers` contiguous blocks (one per reader); each reader folds its
own block's rows in order into a per-block FNV hash; the runner then folds those per-block
hashes (as their hex text) in **reader-index order** into the one emitted checksum. Two cores
with different threading (Go goroutines, Rust threads, the single-threaded TS core running
the blocks sequentially) therefore produce the **same** checksum for a given `(bench,
readers)`, which is the cross-core answer-agreement gate. (It is partition-dependent — an
`r1` and an `r4` bench over the same rows hash differently — but each is only ever compared
within its own bench name, so that is immaterial.)

Identical checksums across all binaries simultaneously prove: the PRNG ports agree (same
param sequences), the engines agree (same answers), and write semantics agree (same
post-state). A mismatch fails the report.

Pinned vector, asserted by a unit test in every harness alongside the PRNG vectors:
folding the two rows `(1, NULL, 'abc')` and `(-7)` yields `dd6e60407d30d28b` (generated
by the independent Ruby reference, like the PRNG vectors).

## 7. Engines, variants, dependencies

| Module | Dependency | Why |
|---|---|---|
| `bench/go` | `jed` via `replace ../../impl/go` | system under test |
| | `github.com/jackc/pgx/v5` | native Go PG driver; `$N` native; `CopyFrom` for the 1M-row load |
| | `modernc.org/sqlite` | pure-Go SQLite — the no-cgo comparison point |
| | `github.com/mattn/go-sqlite3` | cgo SQLite — the C-speed baseline; **cgo confined to this module** |
| | `github.com/BurntSushi/toml` | corpus parsing |
| `bench/rust` | `jed = { path = "../../impl/rust" }` | system under test |
| | `postgres` (sync) | PG client; `$N` native |
| | `rusqlite` (`bundled`) | SQLite, self-contained build |
| | `toml` | corpus parsing (same crate family as the core's dev-dep) |
| `bench/ts` | jed via relative import of `impl/ts/src/lib.ts` | system under test |
| | `postgres` (porsager) | PG client; raw `$N` via `sql.unsafe` |
| | `node:sqlite` | built-in (Node ≥ 22), zero dep |
| | `smol-toml` | corpus parsing (Node has no built-in TOML) |
| `impl/node` + `bench/ts` | `jed` via `impl/rust` | experimental native Node reach artifact under test |
| | exact-pinned `napi` / `napi-derive` / build-only `napi-build` | stable Node-API binding; approved for this experiment |
| `bench/ruby` | jed via the **gem** (`require "jed"` ← `impl/ruby/lib`) | the wrapped core under test |
| | `toml-rb` | corpus parsing — **already a project dev dep** (root Gemfile), no new dep |
| | `bigdecimal` | transitively via the gem |

These are **harness dependencies, not engine dependencies** (CLAUDE.md §14): the bench
modules are separate packages and the cores' manifests are untouched. The Go core's
pure-Go/no-cgo rule binds the *core*; `bench-sqlite-cgo` exists precisely to get the
C-SQLite baseline and its cgo never leaks past `bench/go`. New bench dependencies still
require explicit human confirmation, like any dependency.

### 7.1 The Ruby-gem overhead variant (`jed/ruby/wrap`)

`bench/ruby/bench_jed.rb` runs the **same corpus** through the jed **Ruby gem**
(`engine=jed, lang=ruby, variant=wrap`), reusing the splitmix64 param stream + FNV-1a answer
checksum (ported in `bench/ruby/lib/bench.rb`, pinned to the shared vectors in
`bench/ruby/test/vectors_test.rb`). Its purpose is **not** engine comparison — it is the **gem's
binding overhead**: because `jed/ruby/wrap` and `jed/rust/core` drive the *identical* Rust engine
on the same data, the per-bench `ns_per_op` **delta** is the wrapper tax (FFI round-trip + result
marshalling + value coercion + Ruby object allocation). The answer checksum must match the core's,
which doubles as a correctness gate on the gem. It pulls in **no new dependency** (`toml-rb` is
already in the root Gemfile); it is spawned under `Bundler.with_unbundled_env` so the gem's
`bigdecimal` require resolves. **Caveat:** the gem has no prepared-statement API, so the bench
re-parses the SQL each call (the core's `prepare` parses once) — that per-call parse is *included*
in the measured delta; a gem prepared-statement API would isolate the pure FFI tax. The harness also
prints **allocations/op** to stderr (deterministic, unlike wall-clock) as a complementary metric.

### 7.2 The WebAssembly variant (`jed/wasm/wrap`)

`bench/ts/src/bench-wasm.ts` runs the **same corpus** through the Rust core compiled to
**`wasm32-wasip1`** (`impl/wasm`), driven from Node over `WebAssembly` + the `node:wasi` host
(`engine=jed, lang=wasm, variant=wrap`). It reuses the TS harness's param stream + FNV-1a checksum
(`bench/ts/src/lib.ts`), and its answer checksum must match the native cores' — the cross-engine
checksum gate in `scripts/bench_report.rb` doubles as a **conformance check on the wasm build**. It
needs Node's preview1 WASI: `node --experimental-wasi-unstable-preview1` (the Rakefile passes it);
the `.jed` data files open through a WASI preopen of `bench/data`. **No new dependency** — the wasm
artifact is loaded by Node's built-in `WebAssembly`/`node:wasi`. Two deltas are interesting:

- `jed/wasm/wrap − jed/ts/core` — the **wasm-vs-native-JS** comparison (the same Rust algorithms in
  a wasm sandbox vs. the hand-written TypeScript core). For cheap queries the per-call marshalling
  round-trip (param encode + result-buffer decode across linear memory) dominates and wasm can be
  *slower*; for scan/sort/aggregate-heavy queries the compiled-Rust execution dominates and wasm
  pulls ahead.
- `jed/wasm/wrap − jed/rust/core` — the **wasm sandbox + marshalling tax** over native Rust.

Unlike the Ruby gem wrap (§7.1), the wasm ABI exposes `jed_prepare`/`jed_stmt_query`, so the bench
mirrors the native cores' "parse once, run many" — the delta isolates execution, not parse overhead.
The artifact is an optimized release build (`opt-level=3`, full LTO, stripped); a size-first build
(`opt-level="z"`) trades speed for a smaller module.

### 7.3 The native Node/Rust variant (`jed/node/rust-wrap`)

`impl/node` is an experimental Node-API package that wraps the safe Rust core. Its TypeScript façade
exposes create/open, execute/query, and prepared statements; a compact little-endian buffer carries
`bigint`/`string`/`null` binds and results. The benchmark measures parameter encoding, the Node-API
crossing, Rust execution, result transfer, JavaScript decoding, and checksum folding. The prototype's
typed-value surface is deliberately only what the benchmark corpus needs, so it is not yet a
production replacement for the native TS API.

The addon uses exact pins `napi 3.10.5`, `napi-derive 3.5.10`, and build-only `napi-build 2.3.2` in a
separate host-artifact crate; neither core manifest changes. The local stripped Linux x64 artifact is
4.7 MiB (2.1 MiB gzip). A distributable package would still require the platform/prebuild/provenance
matrix in [locking.md §8](locking.md), complete value/error/session/host-function APIs, and worker-thread
guidance for long synchronous calls.

The full 2026-07-16 run is reproducible with `rake bench:node_compare`; a native Rust control was run
beside it to distinguish core differences from binding overhead. Selected means:

| Lane | Pure TS | Node/Rust wrap | Direction |
|---|---:|---:|---:|
| fully-hot four-column PK lookup | 6.8 µs | 8.4 µs | TS 1.23× faster |
| full scan `count + sum` | 65.0 ms | 16.5 ms | wrap 3.94× faster |
| non-indexed top-100 | 300 ms | 140 ms | wrap 2.14× faster |
| 1,000 inserts then rollback | 13.0 ms | 67.8 ms | TS 5.20× faster |
| one-row durable commit | 3.58 ms | 3.50 ms | within noise |
| hot PK reads, four readers | 18.1 µs/op | 3.3 µs/op | wrap 5.42× faster |
| cold-populating PK reads, four readers | 65.5 µs/op | 8.9 µs/op | wrap 7.38× faster |

The concurrent hook intentionally makes one Node call around the whole threaded phase; its near-zero
delta from the native Rust control is not the cost of ordinary per-query Node calls. Conversely, the
single-process wrapper/native-Rust comparison includes the ordinary boundary: 2.01× geometric-mean
tax overall and 2.77× on the 13 cheap lanes. The comparison therefore supports a workload split, not
the slogan that a native wrapper always runs at Rust speed.

## 8. Write benchmarks and the scratch database

Two write kinds:

- **`write_rollback`** — per iteration: open a transaction, run `batch` bound writes
  (`INSERT`, `UPDATE`, or `DELETE`),
  roll back (jed `begin(writable)` … `rollback()`; PG/SQLite `BEGIN` … `ROLLBACK`).
  Measures executor/binding throughput without growing the database. The post-run sanity
  `count(*)` must equal the dataset's committed row count.
- **`write_durable`** — per iteration: one statement as its own durable commit — the full
  fsync path. jed: autocommit (`synchronous=on` is the only mode today); PostgreSQL:
  default `synchronous_commit=on`; SQLite: `PRAGMA journal_mode=DELETE; PRAGMA
  synchronous=FULL`. The final `count(*)` must equal `warmup + iterations`.

`dataset = "scratch"` is reserved for `write_durable`: jed/SQLite harnesses create a
fresh file in a per-run temp dir under `bench/data/` (removed on exit); for PostgreSQL,
`bench-setup` creates an empty `jed_bench_scratch` database once and the harness runs
`DROP TABLE IF EXISTS` + the bench's `setup_sql` per run. No fingerprint applies to
scratch.

**Durability caveats.** On devcontainer filesystems (overlayfs / virtio volumes) fsync
can be artificially cheap or unevenly costly, and PostgreSQL's fsync happens server-side
on the PG service's own volume while jed/SQLite fsync the local one — durable-commit
numbers are indicative, compare with care. There is also a standing client/server
asymmetry: every PG number includes IPC over the Unix socket, which is PG's deployment
model but not jed's or SQLite's.

## 8.1 Concurrent-reader benchmarks (the `concurrent_read` kind)

`concurrent_read` measures the **throughput of concurrent reader Sessions on one shared
`Database`** — the slice-7 convergence (session.md §2.4/§10): `open`/`create` return a
`Database` that mints concurrently-usable reader `Session`s sharing one committed snapshot +
buffer pool, and the §3 read path is lock-free against everything but a commit. The corpus
ships two r1/r4 pairs: `concurrent_read_pk_r{1,4}` is fully resident, while
`concurrent_read_pk_cold_r{1,4}` starts measured work with most of the million-row leaf working set
absent. Within each pair the SQL and parameter stream are identical and only reader count changes,
so r1's `ns_per_op` over r4's is the realized speedup.

Per iteration of the run loop the kind does **not** apply (it is not a per-statement loop).
Instead the runner materializes the deterministic param stream, splits the measured params
into `readers` contiguous blocks, and hands them to the driver's concurrency hook, which:

1. opens **one** `Database`/`SharedCore` over the dataset file and mints one reader `Session`
   per block;
2. runs a **warmup** pass (untimed) so the shared buffer pool is populated before measuring;
3. runs a **measured** pass, each reader driving its block on its own thread (Go goroutines,
   Rust `std::thread` over the `Send + Sync` `SharedCore`; the single-threaded TS core runs
   the blocks sequentially), folding its block checksum and per-query latencies;
4. returns the per-block checksums (reader order), the merged latencies, and the **wall
   clock** of the timed phase.

`total_ns` is that wall clock, so `ns_per_op = wall / iterations` is throughput latency.
The checksum is partition-folded (§6). Readers re-parse the SQL each call — deliberately, not
via the session prepared-statement form — so a constant per-query parse cost is *included*
(uniform across the jed cores, so it does not distort the scaling).

**Dataset choices.** The resident pair uses the **`small`** dataset deliberately: with the whole
working set in the buffer pool after warmup, it isolates the concurrent *read path* (parse + plan +
a resident B-tree seek per reader) and shows near-linear scaling on a multi-core box. The cold pair
uses **`large`** with the same short 2,000-probe warmup as the point-lookup ramp: most of its roughly
6,900 leaves are absent when measurement starts, but all fit inside the default 32,768-leaf pool. It
therefore isolates concurrent page-cache population, including read/checksum/PAX parse, without
eviction. A truly larger-than-pool variant remains a separate **pager eviction/thrashing** follow-on.

**Scope — jed-only.** `concurrent_read` benches set `engines = ["jed"]`. This validates jed's
own concurrent sessions and keeps the three-way **cross-core** answer-agreement gate (Go ==
Rust == TS under three threading models — a new differential test of the concurrent read
path). The drivers that cannot model it opt out and the runner **skips** them: a driver
implements an optional capability (Go `ConcurrentEngine`, Rust `Engine::concurrent_read`
defaulting to `None`, TS optional `Engine.concurrentRead`); the Ruby gem wrap (autocommit, no
`Session` handle, GIL-bound) and the wasm wrap have no such capability, so they print a skip
line and emit no result. **Deferred follow-on:** a cross-*engine* concurrent comparison
(PostgreSQL connection pools, SQLite multi-connection readers) — a larger driver effort
(thread-per-connection pools across every binary) that is not the slice-7 feature under test.

## 9. Running and reporting

```
rake bench:setup        # build + run bench-setup (fingerprint-gated; [force] to override)
rake bench:run          # build all binaries, run them sequentially, results to
                        #   bench/results/<UTC-stamp>/, then report + HTML
rake "bench:run[point_lookup]"   # substring filter, passed through to every binary
rake bench:report       # re-aggregate the newest (or a given) results dir
rake bench:html         # static HTML report for the newest (or a given) run dir,
                        #   diffed against the previous run by default
rake bench:markdown     # the same report as Markdown, to stdout + <dir>/report.md
rake "bench:diff[a,b]"  # machine-readable JSONL diff of two runs (default: newest
                        #   vs previous)
```

Three reporters share one loader/verifier (`scripts/bench_results.rb`); each **exits 1**
if any two results in a run disagree on `checksum` (wrong answer somewhere — treat it
like a failing conformance test) or on `fingerprint` (mixed-vintage data):

- `scripts/bench_report.rb` — the terminal matrix: groups results by `(bench, dataset)`
  and prints fixed-width `ns_per_op` (humanized ns/µs/ms) with one column per
  `engine/lang/variant`; `-v` adds min/p50.
- `scripts/bench_html.rb [run] [baseline] [--no-baseline]` — writes a self-contained
  `<run_dir>/report.html` (stdlib ERB, inline CSS, zero JS): per-benchmark bar charts
  sorted fastest-first, multipliers vs the fastest, min/p50 tooltips, and — against the
  baseline run (default: the one before it) — per-pair Δ% colored with a 5% noise floor.
  On verification failure the page is still written, failures in a red banner. A
  baseline with a *different* fingerprint is a warning in the page (the runs measured
  different data), not a failure.
- `scripts/bench_markdown.rb [run] [baseline] [--no-baseline]` — the same report (the
  two renderers share `BenchResults.report_model`, so they cannot drift) as Markdown:
  printed to stdout for reading at the terminal and written to `<run_dir>/report.md`
  for the VS Code markdown preview. GFM tables with block-character bars; cells are
  space-padded so the raw text aligns at the terminal. Same defaults, failure handling,
  and fingerprint warning as the HTML report.
- `scripts/bench_diff.rb [run] [baseline] [--json] [--fail-over=PCT]` — the machine
  surface (built for tooling and AI agents; this is the one-command form of the
  CLAUDE.md §10 before/after obligation). Emits JSONL: one object per joined
  `(bench, dataset, engine, lang, variant)` with `before_ns_per_op`/`after_ns_per_op`/
  `delta_pct`/`checksum_match`, with `before_only`/`after_only` flags making
  partial/filtered runs explicit, plus a trailing `{"summary":…}` line (fingerprints,
  improved/regressed/noise counts at the same 5% floor). `--json` emits one pretty
  document instead; `--fail-over=PCT` exits 2 if any matched pair regressed by more
  than PCT% — an operator-side regression gate, never part of `rake ci`.

`rake ci` does **not** run benchmarks, and never will (§1).

## 10. Methodology caveats

Single-threaded, one binary at a time, no process pinning in v1 — run on an otherwise
idle machine; `taskset`/`nice` are at the operator's discretion. The one exception is the
`concurrent_read` kind (§8.1), which spawns `readers` threads *within* one binary by design;
its numbers are most meaningful on an otherwise-idle multi-core box (the speedup tops out at
the available cores). Wall-clock numbers are
relative to a machine and a moment: compare *within* a run (and trends across runs on
the same box), not absolute values across machines. Warmup iterations exist to populate
caches (jed's buffer pool, PG's shared buffers, SQLite's page cache) and JIT-warm the TS
core; they consume the same param stream so the measured window is identically
distributed across engines.

## 11. Backfill and the growth obligation

v1 began with six benchmarks (point lookup, secondary-index lookup, full-scan aggregate,
ORDER BY + LIMIT, insert+rollback throughput, durable single-row commits) over two
datasets (10k / 1M rows). Known gaps, tracked in TODO.md Phase 8:

- a join benchmark (needs a second dataset table → `generator_version` bump);
- GROUP BY aggregate; UPDATE / DELETE throughput; miss-heavy point lookups;
- text-heavy / large-value rows (exercise the overflow + LZ4 path);
- ✅ **`Database` concurrent-reader throughput** — landed as the `concurrent_read` kind
  (§8.1): `concurrent_read_pk_r{1,4}` over the resident `small` dataset plus
  `concurrent_read_pk_cold_r{1,4}` over cold population of the cache-fitting `large` dataset,
  jed-only. Remaining concurrency follow-ons: a truly larger-than-pool eviction/thrashing variant,
  and a cross-*engine* comparison (PG/SQLite connection pools);
- cold-open time;
- durable-commit batch-size sweep (1 vs 100 vs 1000 rows per commit).

**Standing obligation** (CLAUDE.md §10): a perf-relevant feature lands with a benchmark
the same way an optimization lands with a NoREC relation; a perf-sensitive change runs
the affected benchmarks before and after, and both numbers go in the change description.
P6a adds `index_range_nonselective`: a one-sided predicate over the 1M-row secondary index whose
table-fetch storm the deterministic estimator rejects in favor of a full scan. Together with
`point_lookup_pk`, `secondary_lookup`, and `index_range`, it pins the point/range and
selective/nonselective access-path performance matrix without making wall-clock timing part of
conformance.
P6b adds `order_only_index_limit` and `gist_range_select`, and treats `gin_contains` plus
`interval_set_pk` as affected selector lanes. Together they cover order-only B-tree, GiST, GIN, and
interval-set choices across every native core; timings remain observational and checksum equality is
the benchmark gate.
