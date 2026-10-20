# Prepared JOIN key storage

> Historical report: raw evidence and run-specific drivers are on the preserved
> experiment branch. See the [archive and retrieval instructions](../../bench/ruby-native/ARCHIVE.md).
> Reproduction commands below refer to that original checkout.

This explicitly requested investigation follows the rejected
[N-way candidate-allocation experiment](NWAY_FRONTIER_FOLLOWUP.md). The Ruby
core remains experimental, frozen and opt-in. Normal tests, benchmarks, CI
and future feature updates do not include it.

**Decision: retain the change.** The five-table workload is 15.8–34.3% faster
across all nine runtime/API combinations, with 30.9% fewer warmed allocations.
Other query shapes show mixed timings, including a 15.4% ZJIT regression;
this is a targeted improvement, not a general throughput claim.

## Baseline

The baseline is pushed commit
`e9c8cabc284335f9c36f581a682fdc15e3d7c38d`, library SHA-256
`66afc525fcc2f4d0290ef70d03cc2f95e7521bd467029ecc653fe7b418f55341`.
Its private archive is `/tmp/jed-join-key-before/impl/ruby-native`.
New evidence belongs in
[`join-key-followup`](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/join-key-followup`).
Earlier experiment reports and measurements remain unchanged.

The preceding five-table profile suggested that the prepared-resolution key
budget was preventing repeated expression roots from being retained. This
pass first measured cache occupancy and admission failures on the unchanged
baseline. It preserves the existing 4,096-unit/64-KiB key allowance,
128-root limit or 64-statement limit.

The bounded baseline probe confirms this bottleneck. It compares 54
cold/warm/changed-binding observations against resolution-disabled controls,
with equal complete outcomes and current plans. All fourteen jobs use ordinary
reuse; range, LEFT, four-table and five-table also exercise disabled physical
plan reuse. The probe uses TracePoint and independently recounts owned keys;
it is not a timing result.

| Warm case | Retained roots / scopes | Key units / bytes | Unit-limit declines | Base expression compilations |
| --- | ---: | ---: | ---: | ---: |
| Five-table, ordinary policy | 51 / 27 | 4,085 / 2,103 | 149 | 668 |
| Four-table, physical reuse disabled | 53 / 24 | 4,077 / — | 25 | 119 |

The five-table call makes 283 root lookups: 128 hits, 149 unit-limit declines
and six further uncached resolutions. No size decline
is caused by the string-byte allowance. Its scopes consume 2,783 key units,
although they contain only twenty distinct column descriptions. One missing
twelve-column scope requires another 157 units plus five token/wrapper units.
The range and LEFT controls already hit all warmed roots and have no size
declines; they are controls for added lookup/publication overhead.

The first probe completed its observations but failed during helper cleanup
because a local was first assigned inside a block. Its exact helper, report
and failed guard record remain under `failed-cleanup`. Initializing cleanup
locals corrected the harness; the repeated probe passed with unchanged core
source and 67.8 MiB peak sampled RSS.

Later observer review found that the probe's cost labels used the session's
engine counter even when a cursor planned on a duplicated engine. Its six
additional misses are therefore not classified from that label here. The
149 unit-limit declines are independently established by exact proposal-size
recounts. Formal direct diagnostics use the actual traced engine's counter;
the original probe evidence remains unchanged.

## Change

Each prepared entry now interns complete immutable column-description hashes
across its retained scopes. Scope keys remain full ordered arrays, including
duplicate columns. Existing structural scope/AST lookups stay unchanged;
warm hits do not tokenize metadata or walk an additional validation tree.
No parameter values, execution owners or physical winners enter this pool.

Publication stages the complete root key, any new scope and AST, and new
column descriptions before changing the entry. A failed size/admission check
leaves all maps and counters untouched. Every scope occurrence charges a
reference; each distinct description charges its complete owned tree once
plus two pool references. New scope containers and ordinary root/AST keys
remain charged. This changes representation, not the existing limits.

New owned snapshots freeze hash keys as well as values and admit only plain
hashes, arrays, strings and scalar primitive metadata. Unusual mutable objects,
container subclasses and hashes with defaults or identity comparison cannot
be newly copied into the pool; existing structural hits keep their ordinary
equality checks. Full structural keys preserve scope order, aliases, types,
nullability and all additional metadata. Existing contextual-parameter checks
still replay at the original resolution site against the current binding.

Final library SHA-256:
`4d67d30ffbd9163214d1164c1c9c11dcdf5ca63e260a2b502402cc741314bf91`.

## Verification and measurement

The unchanged fourteen-job
[plan](../../bench/ruby-native/compare/nway-frontier-plan.json) covers scalar,
PK, two- through five-table JOINs, range/miss/computed keys, LEFT boundaries,
and scalar built-ins. SQL, fixture and schedules remain unchanged. Every timed
query is fully drained.

Interpreter and YJIT use Ruby 4.0.7; ZJIT uses Ruby 4.1.0-preview1, so the JIT
comparison also changes Ruby version. Each runtime uses serial
before/final/final/before processes across prepared cursors, ordinary query
cursors and eager execute. Source times average two process medians; each
process retains five raw samples. All slower observations remain available.
Process-range separation is descriptive, not a significance test.

Fresh supported-core adapters and the actual Ruby Rust binding use their own
public APIs and collection paths; those comparisons are separate from the
controlled source-change measurements. Exact binding preflights precede
timing. Profiles run last using reused `execute_prepared`; the timed eager
lane repeats `execute(SQL)`.

Workload processes run serially under a 512-MiB sampled RSS guard. Focused
checks use 1 GiB of address space, host/corpus/Ruby workers 2 GiB, and the
supported-core comparison retains its established 16-GiB virtual reserve.
These process limits are not whole-engine heap guarantees.

## Correctness

The new focused test independently recounts retained units and bytes and
checks full-descriptor sharing, ordered/duplicate columns, mutation isolation,
entry-local ownership, exact allowance boundaries and atomic refusal after
staging descriptors or AST data. Failed, metered and unsupported resolution
cannot publish partial metadata. It also exercises the 128-root cap,
contextual checks, unusual metadata, detached ownership, all four query APIs
in memory/file modes, runtime errors and statement/lifetime cost prefixes.

Development preflights passed. Review strengthened two tests before the
formal freeze: late AST overflow now passes syntax admission before reaching
the key-budget refusal, and a compound mutable Hash key proves key ownership
beyond Ruby's automatic freezing of ordinary String keys. The revised focused
test passed at 47.1 MiB peak sampled RSS.

All seven focused suites passed under the interpreter, YJIT and ZJIT, with
88.3 MiB largest sampled RSS. All 71 host-test files passed; that worker
peaked at 189.1 MiB. The source, focused tests and correctness driver remained
frozen throughout these checks.

All four complete corpus lanes passed **77,154/77,154 records**:

| Storage / API | Runtime | Passed records |
| --- | --- | ---: |
| Memory / query cursor | ZJIT | 20,297 |
| Memory / execute collector | ZJIT | 20,297 |
| File / query cursor | YJIT | 18,280 |
| File / execute collector | YJIT | 18,280 |

There are no failures, unsupported records, timeouts, memory-limit exits or
additional skips. Existing authored storage-mode skips remain unchanged.
The largest corpus worker used 183.5 MiB sampled RSS, distinct from the
smaller conformance driver process. Corpus lanes force immediate compiler
admission; focused and direct diagnostics exercise ordinary cache policy as
well as explicit controls. Every correctness worker exited before diagnostic
smokes or formal measurement work began.

## Results

The five-table workload improves with both final process medians below both
baseline medians in every measured API/runtime combination:

| Runtime | API | Before (µs) | Final (µs) | Elapsed change |
| --- | --- | ---: | ---: | ---: |
| Interpreter | Prepared cursor | 12,582.047 | 9,418.179 | −25.146% |
| Interpreter | Ordinary query cursor | 12,561.112 | 9,482.100 | −24.512% |
| Interpreter | Eager execute | 12,216.466 | 9,309.922 | −23.792% |
| YJIT | Prepared cursor | 7,214.083 | 5,369.847 | −25.564% |
| YJIT | Ordinary query cursor | 7,706.258 | 5,061.429 | −34.321% |
| YJIT | Eager execute | 6,917.583 | 5,335.292 | −22.873% |
| ZJIT | Prepared cursor | 6,456.033 | 5,436.050 | −15.799% |
| ZJIT | Ordinary query cursor | 6,603.002 | 5,361.390 | −18.804% |
| ZJIT | Eager execute | 6,583.176 | 5,231.155 | −20.538% |

All **126 cells** remain in the
[complete tables](../../bench/ruby-native/ARCHIVE.md#retrieve-evidence) (archived `bench/ruby-native/results/join-key-followup/tables.md`):
24 have separated faster process medians, 21 have separated slower medians,
and 81 overlap. Fifty-three paired means are slower. The largest observed
regression is ZJIT ordinary-query four-table JOIN: **309.256 → 356.742 µs,
+15.355%**. ZJIT prepared text projection is +9.128%, ordinary-query
three-table miss +8.435%, and prepared computed-key +6.098%, also with
separated slower medians. YJIT prepared computed-key is +5.44%; interpreter
eager three-table is +3.21%. No replacement runs discard slower results.

The YJIT reused-`execute_prepared` profile records a five-table allocation
median of **50,981.550 → 35,220.800 objects/query, −30.915%**. All thirteen
other workloads have unchanged warmed allocation medians. The separate
three-execution call trace can include a cache rebuild and is not a warmed
CPU-time profile. It cannot explain or rule out the ZJIT regressions. The
consistent target gains and independently verified reduction in repeated
resolution justify retention; the slower controls remain a limitation.

The paired YJIT diagnostic smokes passed with identical complete outcomes,
current plans and private file bytes across all 112 API/storage/policy groups.
Their largest sampled RSS was 79.2 MiB. In warm in-memory five-table prepared
queries with physical reuse disabled, base expression compilation fell from 668 to
244 calls and root hits rose from 128 to 230. Key storage fell from
4,085 units / 2,103 bytes to 3,347 units / 495 bytes while retained roots grew
from 51 to the unchanged ceiling of 128. Twenty shared descriptions serve
736 scope references across 67 scopes.

Using the actual executing engine's counter resolves the historical probe's
six ambiguous misses: all 155 baseline misses are publication refusals.
The final implementation has zero publication refusals in this observation, but 53 root
ceiling fallbacks remain. All six formal direct reports agree on complete
outcomes, plans, costs and private file bytes across 112 API/storage/policy
groups each. All 96 paired warm five-table observations across runtimes,
storage modes, APIs and policies show fewer compilation calls and publication
refusals, and more root hits. Independent counters verify the shared key units
and bytes. The remaining root cap is unchanged.

In the fresh supported-core adapter, final Ruby YJIT five-table execution is
5,598.349 µs versus Go 21.249 µs, Rust 17.979 µs and TypeScript 59.062 µs:
263.460× Go and 311.384× Rust. The separate actual Ruby-package comparison
measures native prepared five-table at 5,715.771 µs versus the Rust binding's
1,587.905 µs, or **3.600×** its elapsed time. Other native-prepared/binding
ratios include PK 0.386×, four-table 0.776×, range 5.711× and LEFT 8.979×.
Different public APIs and collectors make these separate observations, not
source-change effects or isolated language/FFI costs. The native core remains
far behind supported cores on the five-table shape despite this improvement.

All 24 formal commands and the evidence verifier completed successfully
within their guards. There were no measured-source changes, overlapping
workload processes or adaptive replacement timing runs. Ruby comparison
workers peaked at 98.5 MiB sampled RSS; the largest supported-core comparison
worker was TypeScript at 227.6 MiB.

## Reproduction

The evidence directory contains explicit `check.rb` and `run.rb` drivers.
Invoke them through `mise run ruby-native:bounded`; every workload has its
own resource guard. Use at least 16 GiB of outer address space for
supported-core comparisons. Existing file-host tests also require local
Unix-socket access.

Run `check.rb --phase PHASE --source-sha256 FINAL_HASH` for `focus`, `host`
and `corpus`. After correctness passes and measured source/helpers are frozen,
run `run.rb --phase PHASE --final-source-sha256 FINAL_HASH` in this order:
`pins`, `verify`, `diagnostics`, `controls`, `cross`, `binding`, `profiles`.
The drivers refuse to replace existing reports; preserve previous artifacts
before an intentional rerun.

After every worker exits, `summarize.rb` verifies source/runtime/helper hashes,
unchanged schedules, exact signatures, raw-sample arithmetic and serial
command intervals. It writes the summary, complete tables and manifest while
preserving parsed values when compacting JSON evidence.

The manifest covers 89 files totaling 19,968,826 bytes. Its SHA-256 is
`46f1b7b5fc40b8d8ca02e5ecf0bcd2b689d190cb21bc0e66655197d6de874a83`.
