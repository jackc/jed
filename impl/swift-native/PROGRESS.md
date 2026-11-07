# Swift experiment progress

## Authorized structural performance follow-up (2026-11-06, complete)

### Final result

Implementation is frozen at local-only `43481e2047da1cee34a627b910c8a7c7c59c473f`.
All affected correctness gates and final measurements are complete. The updated
[benchmark report](BENCHMARKS.md) leads with direct peer comparisons and retains
the original complete 143-workload evidence. [VERIFICATION.md](VERIFICATION.md)
records 543 native tests, all 749 SQL files and the refreshed resource, storage,
process and simulation gates. No engine source changed during measurement.

The final 22-workload peer matrix completes all 12 actor-rounds with zero
failures. Swift/peer geometric means are 5.4065× Rust, 5.1083× Go and 1.0423×
TypeScript; median wins are 0/22, 0/22 and 13/22 respectively. The separate
eight-workload paired run also completes all six actor-rounds: lookup 1.93×,
fixed aggregate 2.74×, array COUNT 11.55× and array containment 12.58× faster
than the retained original Swift actor under matched conditions. These are
diagnostic subsets, not a new complete 143-workload result.

The first Ruby supplement passes its Swift process but stops before executing
Ruby: the supplemental worker closes a `lambda do` with a brace. Its failure
and captured worker remain in `results/architecture-20261106/final-ruby-supplement`.
Only the staged runner is corrected and syntax-checked; no engine source or
retained result is changed. A new output directory holds the retry.

The corrected `final-ruby-supplement-retry` completes all six actor-rounds.
Ruby/YJIT wins lookup (16.757 versus 23.620 µs); Swift wins full aggregation by
23.81× and durable insertion by 2.71×. Source review confirms normal Ruby compiler
loading and default work-memory/accounting settings. Its file-backed aggregate
uses the existing streaming fold; the compiled-aggregate path declines storage.

The strict `final-report` summary revalidates all 22/8/3-workload memberships,
authored inputs, result signatures, Swift/Ruby raw sample statistics and actual
archived actor bytes. It confirms the same final Swift actor in all three runs:
`5bc33bffebd2064a393e0f8a8acecd5c3792d4ef8affeeccb64fb2f6575d4c6d`.
The report's `evidence-sha256.json` digest is
`9fd2aa16e2281fbd3d040c9cf0e62eab26d3d38c3abc3c387d1cba156ecd0e9e`.
Three exact CSV copies are committed alongside BENCHMARKS.md. The complete
original evidence, intermediate regression, failed matrix setup and failed
Ruby runner remain preserved. Swift remains opt-in and Apple platforms remain
unverified; no push, master merge, publication or promotion is implied.

### Investigation record

The following notes preserve intermediate pending and failed states. The final
status and actor-specific acceptance are above.

The maintainer requested investigation and broad fixes for the final Swift/peer
gap. The original completed snapshot and all evidence below remain preserved;
the new investigation starts at local-only `56972d2f`. See
[the investigation design](../../spec/design/swift-native-performance.md).
No new production-delivery decision or default-task inclusion is implied.

Initial static audit confirms the original actor was compiled with `-O` and
whole-module optimization on Swift 6.4.2, with matching runtime libraries and
no sanitizer/testability flags. Current cgroups show no CPU/memory cap. This
does not establish controlled historical clock/governor conditions.

Identified structural hypotheses: enum-wide `indirect` boxes scalar SQL values
and types; every TreeCursor row copies the BPlusTree value; nominal columnar
paths commonly reconstruct rows and contexts; variable-width storage-cost
preflight constructs complete records; peers use packed nullness/span filters
and reusable scratch rows. Ruby's old timing reports use different revisions,
locking and statistics, and its generated kernels are a material architectural
difference. No direct Ruby speed ratio is claimed.

The first controlled source experiment changes only Value.swift/Types.swift to
selective indirection (plus an untimed layout diagnostic). All eight canonical
workloads pass across three alternating rounds against the retained original
actor, pinned to CPU 1. SQLValue now occupies 16 bytes inline, SQLType and
SQLJSONValue eight each. Full-scan aggregate median time falls from 217.843 to
151.244 ms (1.4403×); point lookup improves 1.0450×, array COUNT 1.0855×,
array containment 1.0553×, text nullness 1.0909×, text MAX 1.0794×. Durable insert
changes 1.0223× amid visibly overlapping round ranges. These are focused
experiments, not new whole-suite or peer ratios. Evidence:
`results/architecture-20261106/value-layout-comparison`; candidate actor SHA-256
`7bc64cc8df35aedc92979c123d5eb11de7b12487a9f2b735ca675f7c397ccf7d`.

The lookup sampler identifies retained-tree ownership and linear page-cost
counting in the hot path; its 49 observations are call-path evidence, not CPU
percentages. The array COUNT sampler mostly observed setup and cannot establish
the query's hot path. Both captures and the initial socket-denied failure remain
under `results/architecture-20261106`.

Cursor ownership, binary-window page counting and reusable packed-row execution
pass 21 focused release tests. A separate production build (173.60 seconds) is
captured as actor `9b9cdad788cc7b92ed422e988e6526906ef91b6ba09ebe1c0042617458bc1e9f`.
The same eight-workload, three-round alternating comparison passes all exact
signatures: primary lookup 45.398→31.412 µs (1.4452×), miss 36.604→26.513 µs
(1.3806×), array COUNT 138.167→19.438 ms (7.1080×), text nullness
83.353→18.835 ms (4.4254×), and text MAX 120.620→26.873 ms (4.4886×).
Full-scan aggregate 215.081→178.757 ms is faster than the original actor but
slower than the value-only candidate; this intermediate regression is retained,
and the unfiltered frame/aggregate dispatch is under investigation. Array
containment improves 1.3830× and durable insert 1.0129×. Evidence:
`results/architecture-20261106/packed-foundation-comparison`.

Encoded-span filters and immutable record leaves are independently reviewed and
integrated for focused release testing. The writer change removes per-edit PAX
encoding/padding/CRC/reparse while retaining immutable snapshots, canonical
column descriptors, stored spans and pending overflow ownership. Swift's dirty
records contain encoded bytes, unlike peers' decoded SQLValue records; their
available nonpending spans can therefore use the same validated atom path.
All broad correctness and final performance acceptance remains pending.

The encoded-span/record-leaf stage passes 39 focused release tests. After the
reviewed resolved aggregate kernels and direct unfiltered operand vector are
integrated, 45 focused tests pass, followed by the complete release suite:
538 native tests, all 749 SQL files, 21,597 records, 39,148 cells and the unchanged
2,271 cost / 1,154 name / 100 type assertions, five host actions and no failures
or exclusions. Logs: `/tmp/jed-swift-architecture-span-record-tests.log`,
`/tmp/jed-swift-architecture-kernel-tests.log`, and
`/tmp/jed-swift-architecture-prepared-prestage-all749.log`. Later prepared-query
ownership work still needs its own integrated acceptance.

The final diagnostic matrix is selected in advance: 22 unchanged authored
workloads, all four cores, three rounds, CPU 1. It includes lookup, fixed and
variable scans, filtered/grouped controls, rollback/durable writes and cold
open, with 121 omitted workloads explicit. The projection changes membership
only; field-by-field TOML and normalized-input equality are checked. Initial
metadata now records actual corpus hashes, canonical hashes separately, and
diagnostic status before any actor starts. This does not replace the original
143-workload report. A supplementary three-workload Ruby comparison is being
prepared against the same retained **v50** dataset files; the older sibling
`bench/data/small.jed` is v34 and is not a matrix input.

The final prepared-plan/bundle ownership stages pass the integrated release
gate: **543 native tests and all 749 frozen SQL files**, with the same complete
record, value, cost, type and name totals above. Generic cache hits reuse their
already admitted values, exact detached plans own sequence/subquery facts,
bundles retain their domain map/order, and scalar inputs are created only when
needed. Cold/custom error order, profile boundaries and role checks remain
unchanged. Independent cumulative ownership and execution reviews found no
blockers. Log: `/tmp/jed-swift-architecture-final-all749.log`.

Repository spec/code-generation verification passes. Final cross-core exact
accounting and encrypted SQL gates are running from immutable XCTest actor
`3bed91b6345d597308d2101c385fd615e6fbcb8b7cbc5832a8d4c758200c30bd`
with unchanged resource identity `0568c671be49ea6e8e3ff902f41b48a110b612ab428fd105a8bbd67cabb5ba10`.
The separate production build and remaining affected storage/process gates are
pending; no final performance conclusion is recorded yet.

Final accounting passes all 241,412 measurements, encrypted SQL all 77,612,
saved simulations all 18 traces/core and 153 Swift images, mixed process all
496 pairings plus two Rust-serving streams, and stress all 20,000 invariants.
The production build completes in 183.73 seconds; benchmark actor SHA-256 is
`5bc33bffebd2064a393e0f8a8acecd5c3792d4ef8affeeccb64fb2f6575d4c6d`.
Compiler descriptions confirm ordinary `-O`/whole-module optimization without
testability or sanitizer flags.

The first final service run is preserved as **failed** under
`results/architecture-20261106/verification-final/service`: five pairs pass,
but Go-client/Swift-owner fails the later shared
`resource/query_memory_gist_reopen.test` at a one-byte-budget statement and
misses 25 expected page-client records. The reused Go test binary reports old
source `48c0c068`; a fresh-Go diagnostic is running with the same Swift owner.
No expectation is changed or failed run relabeled. Interchange and generated
simulation gates remain in progress.

**Final correctness acceptance is green.** Full interchange passes 2,691/2,691
phases. Generated simulations pass all 176 traces/core, 459 comparable
transcripts and 7,049 independently checked Swift images, with the original
23 per-core-fault exemptions unchanged. The fresh-Go probe passes, followed by
the complete rebuilt-peer service matrix: 116,628 records, six pairs, zero
failures. Disassembly confirms the old Go adapter lacked the existing
`01c483cc` committed-GiST staging fix; the Swift binary and fixtures did not
change. Both failure and successful retries remain in `verification-final`.
Production source is ready for the isolated final 22/8/3 workload comparisons.

The verified source is committed locally as
`43481e2047da1cee34a627b910c8a7c7c59c473f`. The first matrix setup failed before
starting an actor because the diagnostic singleton method used unqualified
`CORES`; qualifying it as `SwiftNativeBenchmark::CORES` fixes only the harness.
The initial directory/log remain retained. The unchanged production actor is
running in `results/architecture-20261106/final-peer-matrix-retry`; its first
complete four-core round passes all 22 signatures. Final three-round results,
the paired original/current Swift comparison and aligned Ruby comparison are
still pending. No new whole-suite performance claim is implied.

## Original completed experiment

Status: **Frozen experimental snapshot; required Linux acceptance and measured
optimization are complete.** Semantic baseline:
`362e0798ac3a8bd04bd2da556044b6ec73bf468a` (2026-11-05).
Branch: `codex/swift-native-experiment`, local-only, not pushed.

## Completed experiment (2026-11-06)

The frozen implementation revision is
**cec8170bfc33f087fa98949ae661b1eaa5490218**. The [verification report](VERIFICATION.md)
maps completed Linux gates to their actual actors and source identities; the
[benchmark report](BENCHMARKS.md) records the complete before/after comparison,
methodology, slower workloads and measurement limits. The scope and host/tool
matrices contain no remaining required Linux implementation omission. Apple
platforms remain unbuilt and untested. Swift remains opt-in, outside default
tasks and future lockstep updates; promotion and production delivery remain
separate decisions. No merge, push or package publication was performed.

Postoptimization acceptance passes **515 native tests and all 749 frozen SQL
files**, **241,412 exact resource measurements**, **77,612 encrypted records**,
**2,691 interchange phases**, and **18 saved traces per core / 54 transcript
comparisons / 153 independently checked Swift images**, all with zero failures.
The [compact evidence archive](results/verification-postoptimization-20261106/manifest.json)
retains exact actor/resource identities and original logs; manifest SHA-256 is
`2be0039648767a32276d6a5675f1ca160994e9f6bcd3cc2ca32c9f60a60d4762`.
The separate Linux CLI PTY check passes correct, wrong and empty passphrases,
with actual ECHO disabled, exact terminal restoration and unchanged database
copies. Its actor is
`887b902725495e796643f4f19cf31406f3aeea8ce1fa1fc9c212c27c59d3588f`;
see [integrity acceptance](Tools/INTEGRITY_PLAN.md). Earlier process, generated
simulation, service and stress gates remain explicitly preoptimization evidence.

Both full release benchmarks pass **143 workloads × four cores × three rounds**,
12/12 runs each, with matching logical results and zero failures. Baseline source
is `76f4c4c866ddf04f01a9b7d38f6e0b7573c592c6`; the after capture observes
`afbde4c6d2e87e5d3405e228ea233e7012eb209d`, which differs from the optimized
implementation only in this progress document. Swift benchmark actors are
`44a127af9e7891bb245e846fa38edd3cb1a186c3be6c90267365407ff39d3aa7`
before and `e33fd8a85db9193a84e9d6a0ae7a2531bd69bb2297dabda48dbc019133325d1f`
after. Retained runs are `results/benchmark-baseline-20261106` and
`results/benchmark-after-20261106`; strict comparison is
`results/benchmark-comparison-20261106`. Commands and complete provenance are in
the benchmark report. Independent raw-result review agrees with all 143 workload
signatures, all 1,144 medians and all 11 category groups.

Measured full-scan aggregate time falls from **1,119.919 to 217.729 ms (5.1436×)**;
full-scan min/max falls from **1,227.483 to 284.925 ms (4.3081×)**. Across all
143 workloads, the equal-weight geometric mean of before/after ratios is
**1.0325×**: 100 Swift medians are faster and 43 slower. Final Swift/peer geometric
means are **9.1990× Rust, 7.5125× Go and 2.1292× TypeScript**. Three rounds and
observed peer drift do not establish statistical significance or performance
parity. All slower medians, including substantial percentage changes on small
operations, remain in the committed CSV and report.

No implementation or required verification work remains queued. The historical
checkpoints below preserve unsuccessful attempts and their resolutions; their
then-pending work is superseded by this completed checkpoint and the final
reports. Unrelated `tasks/memory/` work remains untouched.

## Historical performance checkpoint (2026-11-06 11:45 CST)

The complete preoptimization release baseline passed: **143 workloads × four
cores × three rounds**, all 12 runs complete, zero failures. Values, row counts,
checksums, parameters and workload settings agree across all cores and rounds.
Command: `mise run swift-native:bench --output /home/jack/dev/jed/impl/swift-native/results/benchmark-baseline-20261106`.
The retained immutable actors and source snapshot record local-only source
revision **76f4c4c866ddf04f01a9b7d38f6e0b7573c592c6**. The run completed at
17:19 UTC; log: `/tmp/jed-swift-native-benchmark-baseline.log`.

Strict analysis is in `results/benchmark-baseline-summary-20261106`. The median
of three round means for `full_scan_agg` is 1,119.919 ms in Swift, 20.261 ms in
Rust, 13.790 ms in Go and 135.551 ms in TypeScript. These are matching authored
operations, not a performance-parity claim. The unweighted geometric mean of
all 143 Swift/peer ratios is 9.5012 / 7.7748 / 2.2143 respectively.

The first diagnostic LLDB capture stopped after six observations because the
sampler misread an asynchronous debugger transition. Its failed report and
stacks remain in `results/profile-full-scan-before`. The corrected capture in
`results/profile-full-scan-before-second` passed with 91 observations in 20
seconds; all main-thread stacks were beneath `GroupedSelectExecution`, with 29
leaf observations in `swift_release`, 25 in `swift_retain` and four in slow
reference-count decrement. These coarse debugger observations identify repeated
row/context ownership work; they are not CPU percentages or benchmark timings.
Exact sampler code, cleanup proof and analysis are retained in its `tooling/`
directory (manifest SHA-256
`8f700635f12eee8de4b5da3f046db75ff3857fc6549e4351990ebb4d645accd7`).

Independently reviewed source changes remove intermediate decoder byte copies
and add a retained leaf-position driver only for already-eligible, unfiltered
whole-table aggregates with fixed-width touched columns. Existing accumulator,
finalization, charge and memory schedules remain in use. Four focused host tests
cover lane equivalence, memory admission, cancellation/I/O cleanup and retained
pending cells; decoder tests cover nonzero slice offsets, truncation, trailing
bytes and recursive validation. Optimized source is committed locally at
**cec8170bfc33f087fa98949ae661b1eaa5490218**. The full release gate passes **515
native tests and all 749 SQL files**, with the same 21,597 records, 39,148 cells
and 2,271 costs, zero failures or omissions:
`/tmp/jed-swift-native-release-packed-all749-first.log`.
Full three-mode V04 also passes **241,412 measurements**, zero differences,
at `results/query-memory-postoptimization-20261106/report.json`. The captured
XCTest SHA-256 is `8a6ab03b598c31d73c14a3468cdf9ca047d1329f809af22671f889cb8f44e56e`;
resource digest remains `0568c671be49ea6e8e3ff902f41b48a110b612ab428fd105a8bbd67cabb5ba10`.
Saved simulations pass all 18 traces per core, 54 comparisons and 153 independent
Swift images at `results/sim-regress-postoptimization-20261106/summary.json`.
Full encrypted comparison passes **77,612 records**, zero failures, at
`results/encrypted-postoptimization-20261106-sockets/report.json`. Its first
attempt is preserved separately: Swift completed, while the peers could not
bind local sockets inside the sandbox. The unchanged retry used local-socket
permission; no expectation, adapter or source change was needed.

A one-round, two-workload diagnostic at `results/benchmark-packed-diagnostic-20261106`
measured unfiltered COUNT/SUM at 229.996 ms versus the baseline median 1,119.919 ms;
the filtered control measured 978.662 ms versus 1,001.085 ms. Row counts and
checksums agree. This diagnostic does not replace the required full after run.
The matching after profile completed its canonical workload with 19 observations
and exit 0; 17 show `foldPackedWholeTable`, with no observed `SelectInput` frame.
Its archive manifest is `aefaeb1157162f5e9325fc99465abcd74ad7ef6be7e274a7a97c35d0a0cac67b`.
Before/after observations have different stopping conditions (duration versus
natural exit), so their counts are not CPU-percentage comparisons.

File interchange, the full matched after benchmark, final reporting and freeze
remain outstanding. The strict opt-in report task is committed locally as
**fd70237c**; its 61 synthetic adapter checks and independent review pass.

## Preoptimization correctness acceptance (2026-11-06 07:04 CST)

**Preoptimization release acceptance is green:** all **509 native tests and 749 frozen SQL
files** pass, with 21,597 records, 39,148 cells, 2,271 cost / 1,154 name / 100 type
assertions and five host actions. There are zero SQL failures, blocked files or
aborted host controls. Command:
`JED_SWIFT_SQL_TRACE_PATH=/tmp/jed-swift-native-release-eof-all749-progress.log mise run swift-native:test --configuration release --all-sql --trace-sql`.
Log: `/tmp/jed-swift-native-release-all749-eof-fourth.log` (exit 0).
The remaining pin assertion below is resolved by observing EOF on the captured
old connection before checking its pin, retaining the original invalidation,
promotion and persisted-data assertions. This changes test ordering, not the
engine's asynchronous transport-loss contract.

Current local-only commits: view **592bb88c**, shared GiST accounting **01c483cc**,
empty Rust corpus rejection **7bf87b13**, Swift handoff **4cf6f1fc**. Canonical Rust
artifacts have been repaired and checked in release/checked memory/disk modes.
Default CI now passes **18/18** in 137.9 seconds:
`/tmp/jed-swift-native-default-ci-gist-canonical-retry.log`. The additional
Rust-pages stream lane passes **2/2** plaintext/encrypted scenarios at
`/tmp/jed-swift-native-process-rust-stream.log`. Corrected full release V04 passes
**241,412 measurements, zero failures** across all four cores and all three
storage modes: `/tmp/jed-swift-query-memory-20261106-1996511-bxwolu/report.json`,
log `/tmp/jed-swift-native-query-memory-release-gist-third.log`. Its captured
XCTest is `944718bbc37e4af7e56f13d13981611503dab04f1af454caaf9f0270fbdea4da`,
resource digest `0568c671be49ea6e8e3ff902f41b48a110b612ab428fd105a8bbd67cabb5ba10`.
Current release mixed-process acceptance passes **496/496** at
`/tmp/jed-swift-native-process-mixed-release-final.log`; saved simulations pass
all **18 traces / 54 comparisons / 153 checked Swift images** at
`/tmp/jed-swift-sim-sweep-20261106-2057112-ozakk7/summary.json`. Generated release
simulation also passes all **176 traces per core, 459 transcript comparisons and
7,049 independent Swift image checks**, with zero failures and the same 23 frozen
fault-comparison exemptions:
`/tmp/jed-swift-sim-sweep-20261106-2104641-awof5y/summary.json`. All required Linux
correctness gates are green before optimization. Its subsequent baseline is
preserved under the ignored durable ext4 path
`impl/swift-native/results/benchmark-baseline-20261106`, not temporary tmpfs.
Additional local-only tooling commits are **1c27c331** (Rust pages serving) and
**41a90bb2** (strict benchmark timestamps).
The successful preoptimization evidence is also retained durably in
`impl/swift-native/results/verification-preoptimization-20261106`: 50 reports,
logs and TSVs, 10.86 MiB, with a complete per-file hash manifest. Manifest SHA-256:
`f5aa79e7f6e4213f7cb088a5ffdb883a1e01b9be889224c5417638f35c4a2f00`.

### Prior attempts superseded by the acceptance above

The following checkpoints retain their original failure and pending-work wording
as an audit trail. They do not describe the final state of the experiment.

Local-only integration commit **33a0add5** records the reviewed engine, host APIs,
fixtures and opt-in runners; **40d15151** records the shared Go float metadata
correction. None is pushed. Unrelated `tasks/memory` remains preserved.

The release build passes **505 native tests and all 749 frozen SQL files**:
21,597 records, 39,148 cells, 2,271 cost / 1,154 name / 100 type assertions,
five host actions, zero failures or omissions. Command:
`mise run swift-native:test --configuration release --all-sql --trace-sql`;
log: `/tmp/jed-swift-native-release-all749-first.log`. This is correctness
verification, not a performance measurement. The debug sixth run passes every
SQL record; its two stale GIN assertions are corrected and the final **42-test**
diagnostic/storage follow-up passes at `/tmp/jed-swift-native-diagnostics-final.log`.

Release forced-TEMP refresh passes **28 files / 1,255 records / 571 cells /
23 costs / 26 scratch files** (`/tmp/jed-swift-native-temp-spill-full-second.log`).
Encrypted refresh passes **77,612 records**, zero failures, with report
`/tmp/jed-swift-encrypted-corpus-20261106-939870-6kf73i/report.json`.
The complete release V04 executes **241,412 measurements** but fails with
**376 exact-accounting differences**: 188 in disk and the same 188 in disk-spill,
across 22 files. All storage-byte measurements agree. Memory has **21,477 records
per core**, exact zero differences. The complete debug run produces exactly the
same 376 failures. Independent spec/loader review identifies a shared defect:
Go/Rust/TS reconstruct committed GiST leaf-key stores through ordinary insertion
without clearing the temporary staging charge. Loading committed records is not
a transaction write under memory.md §7; Swift's zero initial balance is correct.
The reviewed shared correction and six host regressions are now integrated; the
private before/after runs fail before and pass after in all three supported cores.
The full corrected matrix is pending. Failed evidence is preserved at
`/tmp/jed-swift-query-memory-20261106-1135756-exgp9z/report.json` and
`/tmp/jed-swift-native-query-memory-release-first.log`; debug evidence is
`/tmp/jed-swift-query-memory-20261106-939537-ml7e92/report.json`.
Refreshed interchange passes **2,691/2,691 phases** at
`/tmp/jed-swift-interchange-20261106-996175-9832xa/report.json`.
The generated simulation refresh passes all **176 traces per core, 459 transcript
comparisons and 7,049 independently checked Swift images**, zero failures, at
`/tmp/jed-swift-sim-sweep-20261106-980401-9e3v3g/summary.json`.
The service refresh passes all six directions and **116,628 records**, zero
failures, at `/tmp/jed-swift-service-corpus-20261106-1040687-2wheah/report.json`.
Its source-hash freeze is released. This captured actor predates the new Swift
handoff correction; targeted integration and process refresh cover that change.

**Remaining implementation gap:** the guard audit found CREATE VIEW rejects SRF
and JSON_TABLE FROM sources during dependency tracing, although query execution
and all three supported cores accept them. The independently reviewed correction
is integrated; its new 41-record regression passes PostgreSQL byte-identically,
all three supported actors, and Swift in both memory and disk modes. The focused
Swift gate passes **14 native tests**, five frozen files / 301 records / 231 cells /
20 names, with log `/tmp/jed-swift-native-view-producers-retry.log`.
The full release/native refresh executes **507 tests**, passes all 749 SQL files,
but fails six assertions in two page-service tests:
`/tmp/jed-swift-native-release-all749-view-second.log`. A focused optimized run
reproduces four assertions at `/tmp/jed-swift-native-release-gist-service-first.log`;
the new GiST regression passes 33 records / 10 cells in both Swift memory and disk
modes. The exact-version client fixture races an allowed post-ACK maintenance
commit and needs a reader pin plus protocol barrier. The ownership fixture exposes
a real admission race: `requestOwnership` acknowledges before its asynchronously
dispatched worker closes admission. The independently reviewed correction reserves
the drain and pauses admission before acknowledgment, preserves cancellation and
replacement ordering, and includes a deterministic delayed-worker regression.
The integrated debug gate passes **21 tests** at
`/tmp/jed-swift-native-handoff-admission-debug-first.log`. The next full release
run passes all 749 SQL files and executes **509 native tests**, with one remaining
failure: the graceful-close test checks a child's old pin before it has observed
connection closure (`PageServiceAutomaticTests.swift:50`). That failure is under
investigation; the six earlier handoff/client assertions pass. Log:
`/tmp/jed-swift-native-release-all749-handoff-third.log`. No scalar/aggregate
catalog kernel gap was found. The new shared corpus files are being integrated
after the service run. No performance measurements or optimization pass have run.

The queued release command reset the default trace path while debug SQL was
running; use the complete separate test logs for acceptance, not the mixed
`.build/sql-progress.log` progress file. The experiment remains incomplete.

The first integrated shared-GiST CI attempt finishes **16/18**: formatter and
conformance fail (`/tmp/jed-swift-native-default-ci-gist-load.log`). The new TS
host test was formatted outside the repository configuration; that formatting
is corrected. More importantly, the private before/after Rust probe reused the
canonical Cargo target directory, leaving a cached conformance binary compiled
with `/tmp/jed-gist-load-staging/probe/rust` as its manifest path. Its ordinary
walk finds zero corpus files. The strict Swift V04 adapter rejects the missing
21,477 Rust measurements; the run is stopped and retained at
`/tmp/jed-swift-query-memory-20261106-1610643-e6008r`, log
`/tmp/jed-swift-native-query-memory-release-gist-second.log`. No acceptance is
claimed from that attempt. Private probe targets are being isolated, canonical
jed package artifacts cleaned/rebuilt, and CI/V04 rerun from the actual checkout.

A final acceptance audit identifies one additional V07 role combination: the
496-pair process runner captures Swift as the remote page server even for the
two frozen stream scenarios whose baseline specifies Rust serving. Existing
coverage proves Swift serving, but does not prove Swift clients with Rust's
remote stream endpoint. An explicit opt-in Rust page-server selection and two
targeted stream scenarios are being added; the existing Swift-server matrix
remains required. This is a verification gap, not an observed engine failure.

## Integration checkpoint (2026-11-06 05:50 CST)

Full page-service comparison passes all six Swift/peer directions and
**116,628 records** (`/tmp/jed-swift-native-service-full-first.log`). Full
interchange passes **2,691/2,691 phases** over all 112 frozen files, the three
supported peers and generated plaintext/encrypted/attachment cases
(`/tmp/jed-swift-interchange-20261106-19-17b0uj/report.json`). The complete
Layer 3 stress profile passes five core/storage runs, **20,000 snapshot
invariants and 5,000 writer transactions**, with checksum `f3e75496a3111181`
throughout (`/tmp/jed-swift-stress-20261106-19-vqyx3y/report.json`). These are
correctness gates, not performance measurements.

Public integrity acceptance now passes **21 native tests**, including **24
damaged image/report fixtures (50 mode/constraint probes) and 112 ordinary file
fixtures**, simulation-integrity and spec checks. The two older GiST decoder
assertions are aligned with frozen readers: stored routing bounds and unused
padding are not rejected by open, but the damaged-bound integrity report is
still checked exactly; cycles and malformed fanout remain rejected. Evidence:
`/tmp/jed-swift-native-integrity-second.log`. A separate real-socket test passes
plaintext/encrypted remote snapshots, writer visibility, pin release and role
loss (`/tmp/jed-swift-native-integrity-sockets-retry.log`). CLI checks pass all
50 exact frozen reports and 11 credential/exit/text/read-only probes:
`/tmp/jed-swift-integrity-cli-20261106-675919-qw5gej/report.json`.

Direct shared-contract consumers pass **246 coercion captures and 23 estimator
vectors**. The diagnostic integration now passes all **61 Detail, 346
Position/Hint and 10 structured object-field cases**, plus three ordinary SQL
files (78 records, 274 cells, 21 cost / 12 name assertions). The deep-view
resource fixture passes after two mechanical resolver helper extractions:
the dominant per-level query-source frame shrank from 18,792 to 8,056 bytes;
seven repeated frame reservations total 20,032 bytes. No nesting limit or stack
configuration changed. Evidence:
`/tmp/jed-swift-native-diagnostics-complete-first.log`. This gate exits 1 only
because a new runtime metadata test incorrectly expected SUM(i64) to overflow;
the canonical result widens to decimal. The test is corrected to direct i64
addition and all **20 runtime metadata probes pass** at
`/tmp/jed-swift-native-runtime-diagnostics-retry.log`.
Original failed reports remain
`/tmp/jed-swift-native-shared-contract-first.log`,
`/tmp/jed-swift-native-diagnostics-first-retry.log`, and
`/tmp/jed-swift-native-diagnostics-name-first.log` (the stack crash).

The runtime metadata audit found Go omitted type names on finite floating
operation overflow where Rust/TypeScript and the canonical spec supply them.
The narrow two-file Go correction has independent review and all **93 new
public host cases** pass (31 per supported core). Swift's reviewed correction
is published; generic operation underflow/domain diagnostics retain their
contract. Normal CI passes **18/18** at
`/tmp/jed-swift-native-default-ci-float-fields.log`; the six-file shared correction
is local-only commit `40d15151`.

The third V04 run completed all **21,477 Swift memory** and **19,438 each
Swift disk/disk-spill** measurements without SQL failures. Comparison of available memory
records identifies three remaining Swift accounting differences; all three
now pass exact focused comparisons over all four cores and all three modes:
360 short-circuit, 804 EXPLAIN and 384 correlated-query measurements, zero
failures. Logs: `/tmp/jed-swift-native-memory-short-circuit-fixed.log`,
`/tmp/jed-swift-native-memory-explain-followon-fixed.log`, and
`/tmp/jed-swift-native-memory-correlated-fixed.log`. This run also has
infrastructure failures: Go memory records omit 25 file-backed fixture cases,
and all peer disk executions fail local socket bind/setsockopt with EPERM.
It was launched inside the restricted sandbox. It cannot establish complete
acceptance; a new run with required local socket access is necessary. Original
evidence remains `/tmp/jed-swift-query-memory-20261106-19-ydlyx6`.

The refreshed mixed process gate passes **496/496 ordered pairs**
(`/tmp/jed-swift-native-process-mixed-second.log`); the saved simulation gate
passes **18 traces, 54 transcript comparisons and 153 checked images**
(`/tmp/jed-swift-native-sim-regress-final.log`). The new broad actor is
`b7d500a94102edb3efe02d30ce0778031b17b89a39e7608fd1474f68a47e5852`,
resource digest `06cdcd8d00fdf7d7f2232e0a338d2ca42c2408520895d2424c272500a16c9b71`.
Full native/749 SQL (sixth run), V04 (fourth run, now with local socket access),
encrypted/service (second runs) and all 22 simulation profiles × eight seeds
are running from captured executables. Logs use `/tmp/jed-swift-native-` prefixes:
`all749-all-host-sixth.log`, `query-memory-full-fourth.log`,
`encrypted-full-second.log`, `service-full-second.log`, `sim-profiles-final.log`.

The sixth native run has identified one stale GIN unit expectation: oversized
key-only pending nodes are admitted until serialization, per the previously
verified shared page-fit contract. Its reviewed test correction checks `0A000`
at serialization, unchanged allocator high-water/free entries and original
snapshot keys. A separate diagnostic-only correction preserves authored RETURNING
relation order for target aliases literally named old/new; all three supported
public APIs confirm four direct/nested probes. The final diagnostic/GIN/depth
gate is queued at `/tmp/jed-swift-native-diagnostics-final.log`. These changes
postdate the captured broad actor; no executor, storage or accounting logic changes.

No performance measurements have run. The experiment remains incomplete.

## Integration checkpoint (2026-11-06 04:51 CST)

The second complete memory-only V04 run produced all **21,477 records per
core, 85,908 measurements**, with zero Swift SQL failures or omitted records
and **65 exact accounting differences**. Evidence:
`/tmp/jed-swift-query-memory-20261106-148914-gcpdwl`. The second focused
integration passes **32 native tests, 34 SQL files, 1,463 records and 4,074
cells** (`/tmp/jed-swift-native-accounting-integration-second.log`). Additional
reviewed collation/window, PK point-set, VALUES folding and implication fixes
are awaiting the next build.

Full encrypted execution completed **77,612 records** across four cores;
Swift completed all **662 eligible files, 19,403 records and 38,026 cells**
with zero SQL failures. The first report has 20 adapter errors: five Unicode
SQL keys in each core were read as binary Ruby strings. The parser now
validates UTF-8 explicitly, with positive Unicode and invalid-byte checks;
independent review is clean. The strict retained-evidence recheck passes all
**77,612 records with zero failures**, preserving the original report and
recording input hashes in `recheck.json`. Full service comparison continues. Full interchange reached
**2,667/2,691 phases**; three intentionally opaque-definition fixtures expose
an adapter metadata assumption, under independent review.

The coverage audit found two concrete outstanding requirements: the public
quick/full integrity API (I1–I5) and its 24 image/report fixtures, and the
Layer 3 threaded reader/writer stress profile adapter. Internal simulation
checks do not replace the public integrity contract. Implementation is
assigned; the experiment remains incomplete. No performance measurements
will run before correctness acceptance. The reviewed benchmark actor is
published, and four canonical datasets have been generated locally.

Full forced-TEMP mode passes **28 files, 1,255 records, 571 cells and 26
native scratch files** (`/tmp/jed-swift-native-temp-spill-full-first.log`).
The public embedding examples pass memory, durable reopen and cancellation
checks (`/tmp/jed-swift-native-examples-first.log`).

## Integration checkpoint (2026-11-06 04:31 CST)

The fifth complete ordinary SQL/native run is **green**: **749 frozen files,
21,597 records (11,990 statements, 9,607 queries), 39,148 cells, 2,271 cost /
1,154 name / 100 type assertions**, five host actions, and **484 native tests**.
There are zero SQL failures, adapter blocks, control aborts or file exclusions.
Command: `mise run swift-native:test --all-sql --trace-sql`; evidence:
`/tmp/jed-swift-native-all749-all-host-fifth.log` and
`/tmp/jed-swift-native-all749-fifth-progress.log`. The immutable actor captured
after this build has SHA-256
`02d0b80e6a83938b44641601838dda4c0342a4521175bf4a23952f03e0cae999`;
the resource digest is
`5834cf5c6274f1f8bd23b191fa18ac4e2a78e19654a6a9283277c0b7368eedb6`.
This precedes the accounting corrections now being integrated.

The first complete V04 matrix failed, with all peers complete and Swift
stopping at the already-fixed view stack boundary in each mode. The available
Swift prefixes contain **446 memory, 561 disk and 580 disk-spill accounting
differences**. Together with absent records this yielded 12,323 failures over
230,679 measurements. All original evidence remains at
`/tmp/jed-swift-query-memory-20261106-4001051-bbkrw9`; exact differences are in
`/tmp/jed-swift-all-modes-accounting-differences.txt`. No expectations or
comparisons have been relaxed.

Independently reviewed fixes are published for inherited Q3 floor/admission
phases, pre-FK write admission, delayed INSERT SELECT assignment diagnostics,
statement-owned INSERT source buffers, eager scan/DISTINCT output ownership,
cast/row-admission timing, aggregate and synthetic ORDER keys, forced Q4
compaction after a rejected limit, and oversized pending index-node rejection
at serialization. The focused integrated build/test passes **71 tests,
29 files, 1,056 records, 1,592 cells, 82 cost / 49 name / four type assertions,
zero failures** at `/tmp/jed-swift-native-accounting-integration-first.log`.
The fresh full-memory diagnostic captures immutable actor
`f232355dcc75f65b6dbe3a6bbf67ac66e892f30d62fff993c5c186eec0c058f4`
at `/tmp/jed-swift-query-memory-20261106-148914-gcpdwl`; the first full
encrypted comparison is also running. Remaining join,
recursive and DISTINCT ON accounting differences are being investigated in
isolated drafts. Full exact-accounting acceptance is still required.

V06's six real client/owner directions pass a 17-record point-lookup smoke
test (**102 records, zero failures**):
`/tmp/jed-swift-native-service-six-pair-first.log`, retained output
`/tmp/jed-swift-service-corpus-20261106-4062030-42nfv4`. V10's encrypted-empty
TS/Swift fixture smoke passes all eight phases after reviewed adapter cleanup;
the generated interchange diagnostic exposed the specified attached-view
column omission, now corrected with a host-boundary regression test awaiting
this build. Full encrypted, service, interchange and forced-TEMP runs remain
outstanding. Public examples compile but still need their runtime check.

Default `mise run ci` passed **17 of 18 steps**, including all supported-core
conformance, process, simulation and browser gates. Its sole failure was the
new opt-in Go adapter's octal permission literal formatting. That literal is
corrected and `mise run fmt` passes. Evidence:
`/tmp/jed-swift-native-default-ci-first.log` and
`/tmp/jed-swift-native-default-fmt-fixed.log`. No default task requires Swift.
Release baseline, measured optimization, final reports and freezing remain
required; this experiment is still incomplete and local-only.

## Integration checkpoint (2026-11-06 04:12 CST)

H16 now passes all **15 batching boundary tests**, including 64-row primary
windows, 1,024-row in-hand chunks, strict NULL/error replay, finite cost caps,
DISTINCT/LIMIT, interval charging, sorted/spooled output, cleanup and exact
query-account schedules. The integrated resource gate passes **30 tests** and
**six SQL files, 255 records, 333 cells, zero SQL failures**. Evidence:
`/tmp/jed-swift-native-host-batching-resource-integration.log`. This includes
the native forced-TEMP adapter check; the full TEMP mode remains pending.

The fourth full SQL/native run aborted on a planner stack overflow at the
deepest valid view chain; it is not passing acceptance. Its obsolete rebuild
fixture also expected a 600-byte key to exceed an encrypted 1,024-byte page,
which no longer holds after the independently justified actual-page-fit fix.
The fixture now uses a 1,200-byte key, preserving its failure/source-unchanged
contract. Splitting SELECT/WITH/VALUES dispatcher construction into separate
helpers reduces the recursive dispatcher frame from 13,504 to 1,488 bytes
without changing the canonical 256-depth limit. Independent review is clean.
The fresh focused gate passes **10 files, 342 records, 562 cells, 43 cost /
65 name / five type assertions, zero failures**, plus all **three rebuild
tests**: `/tmp/jed-swift-native-planner-stack-rebuild.log`. The failed full run
is retained at `/tmp/jed-swift-native-all749-all-host-fourth.log` and
`/tmp/jed-swift-native-all749-fourth-progress.log`.

The first full V04 matrix captured immutable pre-stack-fix actor
`d27b0b058bc8f2377beeb94e28894cae39e0ed9b723a7ec610d3667b2a42f282` at
`/tmp/jed-swift-query-memory-20261106-4001051-bbkrw9`. It is still collecting
disk modes, but Swift memory stopped at 17,602/21,477 records while all three
peers completed. The available prefix exposes **446 exact accounting
differences**, with the peers agreeing; these require implementation fixes,
not comparison exceptions. Full differences are retained in
`/tmp/jed-swift-memory-all-prefix-differences.txt`. Query-side Q1/Q2 review is
delegated; parent owns mutation/transaction pending accounting and the live
storage divergence. A new actor and complete matrix are required afterward.

Reviewed V05 encrypted and V06 real-service test-adapter hooks are published;
V05's bounded adapter check passes, native compile/runtime acceptance remains
pending. V06's six-direction driver and V10's exhaustive 112-image fixture /
bidirectional interchange driver are being integrated. Local runnable public
API examples are added, awaiting compile/run. The default `mise run verify`
gate passes all **17 checks** (`/tmp/jed-swift-default-verify-final.log`);
full default CI and measured release optimization remain outstanding.

## Integration checkpoint (2026-11-06 03:44 CST)

The third **22 profiles × eight seeds** simulation comparison is green:
**176 traces, 459 transcript comparisons, 7,049 independently checked images,
zero failures**. Immutable actor
`b4c1180774d7f48c4df911248156aaea822ec0e03a988f1b4cd871be0ed2e34f` includes
the ALTER pending-write, create publication, mixed overflow refault,
credential/rekey and direct attachment reuse corrections. Command:
`mise run swift-native:sim:sweep --seeds 1..8`; evidence
`/tmp/jed-swift-native-sim-profiles-allocation-q3-fixed.log`, retained output
`/tmp/jed-swift-sim-sweep-20261106-19-lawllp`. Later host API and batching
changes are outside this actor and need fresh integrated acceptance.

All seven residuals from the third full749 run pass the socket-enabled
focused gate: **13 files, 437 records, 1,328 cells, 79 cost / 96 name / five
type assertions, zero SQL failures**. Page-service6, attachment-reuse1,
credential2, root-residency3, prepared12, planning-probe6 and maintenance6
pass. The one ergonomic fixture failure (an eagerly materialized SRF under
a streaming-only budget) was corrected to use a primary-key table scan.
Evidence: `/tmp/jed-swift-native-final-sql-residuals-ergonomic-host.log`.

The fresh host/API foundation gate passes **30 tests**: cursor admission3,
escaped file-host lifetime1, native argument/typed row helpers6, scripts7,
session settings5, frozen resource budgets2, corpus accounting2 and host
batch primitives4. Command: `mise run swift-native:test --filter
'HostBatchBoundaryTests|CursorAdmissionBoundaryTests|CursorHostLifetimeBoundaryTests|ErgonomicBoundaryTests|ScriptExecutionBoundaryTests|SessionSettingsAccessTests|SQLCorpusAccountingTests|FrozenResourceBoundaryTests'`;
log `/tmp/jed-swift-native-host-api-batch-foundation.log`. The first compile
found one omitted catalog validator in the new admission fixture; corrected
before the passing run. Cursor timing evidence from all three supported
cores justifies a narrow streaming.md prose correction, recorded separately
in `Tools/BASELINE_FINDINGS.md`.

V04 now has a strict opt-in four-core exact peak/live-storage adapter.
`mise run swift-native:query-memory --no-build --filter
resource/query_memory_state.test --mode memory` passes **42 records per
core, 168 measurements, zero differences**. Evidence:
`/tmp/jed-swift-native-query-memory-diagnostic.log`,
`/tmp/jed-swift-query-memory-20261106-19-ugjlso/report.json`. This is a
diagnostic subset; full memory/disk/disk-spill acceptance remains unrun.
The reviewed forced-TEMP adapter and tests are published but not yet built.

H16 speculative batch/scalar replay framework and all named emitter sites
are implemented and independently reviewed. New primary-scan, interval and
emitter tests still need integration. Review found and repaired DISTINCT's
speculation cap (its inner producer must use the outer survivor window).
A remaining audit concerns the top-level plain buffered projection's source
admission/lifetime; do not claim H16 complete until its 1,024-row in-hand
lane and exact Q1 schedule are tested. V10 exhaustive bidirectional
interchange tooling is being implemented. No release optimization has run.

## Current integration checkpoint (2026-11-06 03:14 CST)

The **third full749 run completes 749/749 files and 21,597 records** (11,990
statements, 9,607 queries), **39,135 cells, 2,271 cost / 1,154 name / 100 type
assertions and five host actions**. It reports **seven SQL failures across
three files**, zero adapter blocks/control aborts/storage-mode exclusions:
`aggregates/select_distinct.test` five incorrect DISTINCT/ORDER admission
errors, `joins/comma.test:127` cost20/19, and `query/subquery_point_set.test:123`
cost36014/44. All three corrections are independently reviewed and published,
but postdate this actor. Evidence: `/tmp/jed-swift-native-sql-all749-third.log`,
`...-progress.log`, `...-report.json`; command adds `--all-sql --trace-sql` to the
focused prepared/probe/root/credential/page-service/maintenance filter. The
staged-memory fixture is corrected and all three RootResidency tests pass;
credential2, maintenance6, prepared11 and probe6 pass. Six native page-service
tests fail Unix socket `errno1` under the sandbox; these require the separate
socket-enabled rerun, not source changes or acceptance exclusions.

The next socket-enabled focused gate is running in
`/tmp/jed-swift-native-final-sql-residuals-ergonomic-host.log`, selecting 13 SQL
files including all three failures, neighboring derived/CTE/aggregate cases,
partial-index EXPLAIN and pending-memory limits. It also tests new ergonomic
native-argument/typed-row helpers, exact large-value memo keys, corrected
credential/rekey ordering, direct attachment reuse and native page clients.
Root's H09 implementation is independently reviewed; runtime acceptance is
pending. A host audit found escaped cursors retain DatabaseCore but not the
Database/file-host lifetime; a narrow ownership repair and native regression
are being drafted. H12 executeScript remains missing and is in implementation.
Reviewed ALTER rewrite Q3 staging now matches cumulative frozen behavior;
new simulator build/profile acceptance remains pending.

Two further **local-only** reviewable commits: `d8b23ec9` clarifies the existing
zero-manifest bootstrap stabilization exemption; `40ac047d` adds the reviewed,
previously tested bounded scratch row/sort/hash primitives. Neither is pushed.

### Previous focused checkpoint

The prepared/probe/deep-view gate completes **12/12 SQL files, 560 records,
1,138 cells, 35 cost assertions, zero SQL failures**, without adapter blocks,
control aborts or exclusions. All **10 prepared API/cache**, **six planning
probe**, and **two bounded-operator cleanup** tests pass. Two of three new
root-residency tests pass; the staged-memory case fails `XX001` because its
live-page delta disagrees with reachability, under host diagnosis. Evidence:
`/tmp/jed-swift-native-prepared-select-probe-deep-view.log`. Command:
`mise run swift-native:test --sql-files resource/view_expansion_limit.test,resource/view_depth_limit.test,ddl/views_options.test,dml/merge_view.test,dml/view_check_option.test,dml/view_on_conflict.test,subquery/correlated_grouped.test,subquery/outer_level_aggregate.test,subquery/derived_correlated.test,params/like_pattern_proof.test,params/custom_plans.test,json/to_json_sources.test --trace-sql --filter 'SQLCorpusBoundaryTests|PreparedBoundaryTests|PreparedSelectBoundaryTests|PlanningProbeBoundaryTests|BlockingOperatorBoundaryTests|RootResidencyBoundaryTests'`.

H06 now has `Database`/`Session` prepare, queryPrepared and executePrepared
through the total cursor seam. Typed VALUES INSERT and SELECT caches contain
immutable detached metadata, rebound to the current domain snapshots, index
contexts, permissions and meters. SELECT uses source-ordered schema/estimator
identity tokens, exact value-read and NULL-read probes, committed-only fills,
and up to eight exact-value custom plans. Tests cover rollback, working DDL
and DML, database identity, temporary shadowing, ANALYZE, memo eviction,
joins/compound queries, cancellation and read-only admission. Independent
reviews corrected pushed-predicate cacheability and dynamic-timezone index
exclusion; the latter has a new host test awaiting its next gate. Every
subquery remains uncacheable, matching the frozen resolvers. Full host/value
acceptance and refreshed whole-corpus verification are still required.

The second full749 attempt (`/tmp/jed-swift-native-sql-all749-second.log`,
trace `...-progress.log`) **aborted**, so it is not a completed conformance
result. Recursive AND scanning and derived-view pushdown both overflowed on
`resource/view_expansion_limit.test`; ordered iterative walks now pass that
whole file. The focused gate also verifies restored pushed-down view write /
CHECK OPTION predicates, grouped correlation, outer aggregates and
array_to_json overload resolution. Remaining known SQL failures are comma
join cost, partial-index EXPLAIN wording and derived subquery point-set
folding time. The EXPLAIN correction is applied but not yet rerun.

The second **22 profiles × eight seeds** simulation comparison completes
**176 traces, 459 transcript comparisons and 6,884 independently checked
images**, with **112 failures** (previous run: 634). Swift passes 173/176
local oracles; all three supported cores pass 176/176. Evidence:
`/tmp/jed-swift-native-sim-profiles-corrected-hosts.log`, output
`/tmp/jed-swift-sim-sweep-20261106-19-89lt3a`, actor
`56ea638a5fa57adb96153fd9cb737c4fcaa9e0bc93ad520cda77ad289ea6592a`.
A reviewed create-publication fix closes the temporary handle and reopens
the published path after the directory barrier; mixed inherited/fresh
root-leaf overflow refaulting is published with the focused test failure
above. Credential admission/entropy ordering fixes are being integrated.
Resource profile5 reveals an earlier ALTER rewrite pending-byte hold being
released by a later UPDATE only in Swift; correction is in progress. These
are post-actor changes; the sweep has **not** passed acceptance.

Next: finish the focused storage/resource repairs, rerun all749 SQL files,
refresh simulator actor/regressions/profile comparisons, complete the host
matrix audit and all remaining byte/interchange/resource/process gates,
then release-baseline and measured optimization. No completeness or
performance-parity claim; Apple platforms remain unverified.

### Earlier integrated checkpoints

The integrated VALUES, proof, pushdown and bounded DISTINCT gate completes
**18/18 files, 495 records, 2,187 cells, 209 cost / 12 name / 5 type assertions**:
**9 SQL failures**, zero adapter blocks or exclusions. All **24 spill, sort and
storage-domain boundary tests pass**. Evidence:
`/tmp/jed-swift-native-bounded-distinct-proof-values.log`; command selects
`setops/values_query.test,setops/parenthesized.test,subquery/values_order_by.test,types/utf16_split.test,query/table_column_alias.test,query/grouped_setop_pushdown_explain.test,query/safe_conjunct_pushdown_explain.test,query/contradiction.test,dml/contradiction.test,query/where_rewrite_explain.test,params/custom_plans.test,joins/safe_conjunct_pushdown.test,storage/malformed_row_count.test,subquery/derived_pushdown.test,subquery/grouped_setop_pushdown.test,joins/on_pushdown.test,aggregates/blocking_spill.test,resource/query_memory_spill.test`
through `mise run swift-native:test --sql-files ... --filter
'SQLCorpusBoundaryTests|SpillBoundaryTests|SpillHashBoundaryTests|ExternalSortBoundaryTests|StorageDomainBoundaryTests'`.
Remaining failures: VALUES scalar-subquery ORDER cost, one opaque INL estimate,
two derived pushdown costs, and five memory-spill assertions. Bounded aggregate,
window and DISTINCT ON have independently reviewed drafts; bounded joins are
drafted and under review. Derived-result Q1 admission also has a reviewed fix.
No claim that all consumers are bounded or that full SQL conformance passes.

The first **complete 749/749 frozen SQL file run** finished: 21,597 records,
39,042 cells, 2,271 cost / 1,154 name / 100 type assertions and 5 host actions.
It reports **246 record failures in 46 files**, with zero adapter blocks,
control aborts or storage-mode exclusions. File-only suites now use the native
file host. Exact evidence: `/tmp/jed-swift-native-sql-all749-native-file.log`
and `...-report.json`; command `mise run swift-native:test --all-sql --trace-sql
--filter 'SQLCorpusBoundaryTests|PostbaselineSQLTests|JSONBoundaryTests|ResourceTests'`.
All completed records remain authoritative; this is diagnostic, not acceptance.

All **18 frozen cross-core simulation regressions pass**: all four actors pass
18/18 local oracles, **54 transcript comparisons and 153 independent images**
match, zero failures. Swift actor:
`ecedde3298fef53ec972d0d0d55237ae1eb0b4fee5b7e741a2156069caad8a8f`.
Command `mise run swift-native:sim:sweep --regress`; evidence
`/tmp/jed-swift-native-sim-cross-core-final-hash.log`. The final three differences
were an adapter hashing post-open repaired key blocks while emitting the saved
pre-open image; the adapter now hashes the emitted bytes and has a passing host
assertion. No engine format or fixture changed. The **22 profiles × 8 seeds**
comparison completed from this immutable actor, command
`mise run swift-native:sim:sweep --no-build --seeds 1..8`; log
`/tmp/jed-swift-native-sim-profiles-all22-eight-seeds.log`, output
`/tmp/jed-swift-sim-sweep-20261106-19-rfibm0`. It **fails 634 checks** across
176 traces, 459 transcript comparisons and 5,344 independent images. Swift
passes 131/176 local oracles; all three supported cores pass 176/176. First
divergences involve directory-barrier fault behavior during backup, attachment
recovery, sync/crash generation selection and durable-floor truncation; later
differences include cascades. Host agent is diagnosing and correcting causes.
The 18-regression result remains valid but does not establish sweep acceptance.

Reviewed optional affected-row metadata, exact promoted INL bounds, LEFT hash
estimates, and quantified EXPLAIN admission pass the focused 152-record gate
except two then-outstanding lateral records. Evidence:
`/tmp/jed-swift-native-rows-sim-promoted.log` (14 tests, two corpus failures).
The subsequent temporal/lateral/depth gate completes 133 records with only the
excess-alias error-priority record remaining; all date/interval corrections,
lateral cost/EXPLAIN and all22 deep-view records pass. Evidence:
`/tmp/jed-swift-native-spill-foundation-temporal-lateral.log` (14 tests, two
failures: that SQL record and a new sorter test using an inactive unlimited
memory account; the test now enables a positive limit).

Scratch row/config/spool and stable external sorting are implemented; five
codec/spool host checks and both forced-sort/fault checks pass. The keyed disk
state/hash-row structures and general derived/group/window/set-op plus ON scan
pushdown passed all 13 host tests in their first integrated focused gate:
`/tmp/jed-swift-native-pushdown-spill-foundation.log`; 209 SQL records in nine
files then had 20 failures, many resolved by the newer gate above.
Operator consumers are being connected through a
separate bounded file-host lane; no full spilling claim is made. Root owns
bounded joins and plain sort integration; crypto agent owns BlockingRows,
DISTINCT and aggregate consumers; host owns primitives; scope owns VALUES/set-op
resolution. CTE/bound-parameter body pushdown and other fresh SQL failures remain.
Linux completion, fresh process/interchange acceptance, release baselines,
measured optimization, coherent implementation commits and final reports are
still outstanding. No freeze or completion is claimed.

## Previous integration checkpoint (2026-11-06 01:14 CST)

The unchanged deep-view file now completes **22/22 records**, with all **26/26**
tests in the combined view, maintenance, client, simulated-service and record
gate passing (`/tmp/jed-swift-native-view-host-recovery.log`). The earlier parser
gate passes **6/6**, including maximum-depth nested function probes and all 10
records in `query/order_by_topk_explain.test`
(`/tmp/jed-swift-native-max-function-topk.log`). The native 256-level policy and
all frozen expectations remain unchanged. Planner and executor helper splits
reduce retained frames; query-expression collection uses an explicit work stack.

The first completed cross-core simulation regression comparison found **180
disagreements**, despite all four actors passing their local history oracles.
See `Tools/SIMULATION_COMPARISON.md` for immutable evidence and first divergences.
Reviewed native corrections preserve recovery stabilization across credential
rewrap, retain poisoned owner tenure until close/death, and borrow the writer's
existing pin for transaction cursors. Their new host invariants pass in the
26-test gate. ANALYZE and lost-close transcript formatting are corrected. The
remaining ordered-LIMIT discrepancy was traced to attachment PK order metadata;
its reviewed correction is published but awaits the next test/comparison gate.
Cross-core regression acceptance and the 22-profile sweep remain outstanding.

Focused multi-file corpus selection now validates every requested filename and
rejects duplicates/omissions. Its adapter invariant passed in the same gate.
The latest full 749-file attempt remains the incomplete 602-file result below;
no completed whole-corpus result is claimed. Correlated query-block cost meters
and terminal cursor guards have reviewed corrections awaiting resource checks.
File-only corpus lanes, JSON resource corrections, and optional host affected-row
metadata are being integrated before the next full run. Linux completion,
release baselines, and measured optimization remain outstanding.

## Previous integration checkpoint (2026-11-06 00:54 CST)

Native service, backup, maintenance, deadline, transport, tree and record-boundary
checks pass **92/92** in `/tmp/jed-swift-native-native-service-wide-params.log`.
This includes `params/custom_plans.test`: all 59 records, 482 cells and 12 cost
assertions pass. The unchanged default integer/ANY/ALL binding files had no
reported failures in the subsequent broad run. The exact-fit wide-row correction
and plain/encrypted index separator boundaries pass; root pending-cache reuse has
a separately reviewed follow-up awaiting its next integrated gate.

All **31/31 native Swift process schedules** pass using immutable actor
`ffe818a97e17f7f159ac6bc7ebdb5641dae768d540923949e3ef598ca0752e56`
(`/tmp/jed-swift-native-process-service-integrated.log`,
`mise run swift-native:process --no-build`). Mixed 496 pairings still require a
fresh run. All 18 same-core simulation regressions remain verified at the prior
checkpoint; a cross-core comparison is now running in
`/tmp/jed-swift-native-sim-cross-core-regress-bytecheck.log`. Its first launch
stopped before any actor because a UTF-8-vs-binary Ruby string comparison falsely
rejected identical pinned generator bytes; comparison now uses binary strings.
No simulation comparison or generated-profile sweep completion is claimed yet.

The second full SQL attempt reached `resource/view_depth_limit.test:30` and
crashed during nested view parsing/planning. Preserved trace:
`/tmp/jed-swift-native-sql-all749-after-depth-progress.log`; diagnostic JSON:
`/tmp/jed-swift-native-sql-all749-after-depth-report.json`. It records 605 file
starts, 602 ends, 17,672 whole-corpus record starts, **201 distinct observed
mismatches across 60 files**, and two explicit adapter setup blocks for file-only
resource cases. These remain partial diagnostics, not a completed corpus result.
A reviewed planner stack split is compiling; the previous recursive frames were
47,264 bytes in SelectPlan.resolve and 109,472 bytes in RelationPlan's source
resolver. The shared depth policy and all fixture expectations remain unchanged.

Reviewed corrections for scalar short-circuiting, filter conjunct ordering,
LIKE/SIMILAR error and NULL behavior, keyword result names, radix LIMIT bounds,
and fenced N-way join islands are published, but their broad semantic gates are
still pending. The file-mode corpus adapter and generated simulation sweeps are
being connected explicitly; no unsupported records are silently omitted.

Shared prose clarifications are local-only commit `1ee030e4` (not pushed).
Unchanged `dml/index_spilled_key`, `ddl/views_long_definition` and
`ddl/views_owned_sequences` pass in all three supported cores in both memory and
disk modes (18 file/mode/core checks; `/tmp/jed-index-page-fit-{go,rust,ts}.log`,
`/tmp/jed-wide-record-{go,rust,ts}.log`, `/tmp/jed-view-default-{go,rust,ts}.log`).
The correction records existing page-fit/default admission, with no byte-format
version, fixture, or supported-core algorithm change. The Swift experiment is
still active, incomplete and unoptimized.

## Previous integration checkpoint (2026-11-06 00:37 CST)

The integrated depth, index boundary, independent integrity and simulated service
checks pass **28/28**:
`mise run swift-native:test --sql-file resource/depth_limit.test --trace-sql --filter 'SQLCorpusBoundaryTests|ExpressionBoundaryTests/testOperatorProbes|ExpressionBoundaryTests/testFunctionProbes|ExpressionBoundaryTests/testConstructorHints|TreeBoundaryTests|SimIntegrityBoundaryTests|SimulationServiceBoundaryTests|SimulationRunnerBoundaryTests'`
(`/tmp/jed-swift-native-depth-key-service.log`). The unchanged allowed 256-level
expression now resolves/evaluates successfully, and the forbidden depth keeps its
specified error. Splitting recursive resolver/evaluator case families alone was
insufficient: `/tmp/jed-swift-native-depth-integrity-service-compiled.log` still
crashed in container overload probing. Moving initial operand resolution before
that larger dispatch frame reduces its active debug frame from 14,552 to 936
bytes; no parser limit, corpus expectation or type-hint rule was weakened.

Named-composite literal ROW costs, unanchored local parameter widths, scalar
ANY/ALL array context, constraint-name truncation/classification, RETURNING alias
scope and INSERT SELECT literal provenance have reviewed corrections in this
build. Full SQL verification is pending. Key-only index entries now use actual
leaf/interior page fit; plain/encrypted host boundaries verify single-leaf
capacity, contextual separator rejection without replacing the prior tree, and
exact-capacity interiors. Shared format prose is corrected against the existing
index design and unchanged large-key corpus (see `Tools/BASELINE_FINDINGS.md`).

The simulated host now routes actual page-service protocol traffic through
resident crash-clean coordination and framed streams. Focused checks cover
client deltas, async pin release, owner death/promotion, and the unchanged
service-disconnect trace twice. Retained root overflow caches are validated
against their real sealed chains, including an independently corrupted chain.
These focused passes do not establish whole-simulation acceptance.

All 18 frozen simulation regressions now pass twice: **18 PASS, zero failures,
zero skipped**, immutable actor SHA-256
`e5494608f618785d74297a9318875e77c8a3dcf19c251eae65369cd18b8692d0`,
`/tmp/jed-swift-native-sim-service-integrated.log`, outputs
`/tmp/jed-swift-sim-results-20261106-3772157-pnbmp6`. The eight earlier passing
traces retain their prior transcript and operation hashes. Generated-profile
sweeps and comparisons with supported cores remain separate work.

The all-749 attempt in `/tmp/jed-swift-native-sql-all749-after-depth.log` later
crashed at nested view depth; see the newer checkpoint for preserved partial counts. Fenced N-way joins remain drafted, and full native process/mixed-core
reverification is still required after the service-host extraction. This is an
active, incomplete experiment; no completion or performance-parity claim.

## Previous integration checkpoint (2026-11-06 00:13 CST)

The combined concurrency, deadline and resolver gate passed **27/27**:
`mise run swift-native:test --sql-file ddl/catalog_long_table_indexes.test --trace-sql --filter 'BatchFlushBoundaryTests|SimulationRunnerBoundaryTests|ConcurrencyCorpusBoundaryTests|PageServiceCoreTests|SQLCorpusBoundary|ExpressionBoundaryTests/testOperatorProbes|ExpressionBoundaryTests/testFunctionProbes|IntervalBoundary'`
(`/tmp/jed-swift-native-deadline-concurrency-green.log`). All seven frozen
concurrency schedules ran through real sessions and cursor draining: 180 steps,
72 SQL records, 40 version/watermark assertions and two queued writer handoffs.
Eight deadline tests verify foreground expiry, native idle flushing, multi-domain
budget-before-flush ordering, writer exclusion and poisoning after a client clock
failure following body writes. The unchanged S14 deadline simulation also passes.

The earlier operator fix exposed the same recurrence in nested scalar-family
fallthrough and array-concat polymorphic retyping. Those paths now retain the
same compound operands; literals, parameters, arrays and rows still receive
their contextual type hints. Two callback-count invariants cover five 32-term
operator chains and 32 nested lower/upper/array-append calls. Broader nested
constructor retention is still under review; this is not an arbitrary-expression
resource-safety proof. The original long catalog file passes all 16 records.

The reviewed all-base N-way access planner and collation/datetime dependency
corrections are compiled in this gate. The full SQL attempt used
`--all-sql --trace-sql --filter 'SQLCorpusBoundaryTests|PostbaselineSQLTests'`
(`/tmp/jed-swift-native-sql-all749-complete-attempt.log`). It crashed at the
explicitly allowed 256-level arithmetic chain in `resource/depth_limit.test:89`.
Direct tracing records 576 file starts, 575 ends and 16,901 record starts;
stdout contains 260 distinct observed mismatches across 67 files, no adapter
blocks and no completion summary. These are partial diagnostics, not full-corpus
results. The parser limit stays unchanged; focused resolver case dispatch is
being split to remove the oversized recursive debug stack frame.

All 18 frozen simulations completed, each run twice, with
immutable actor SHA-256
`fb88b3a47af72578f2cb40f465795943c177260640f20e2dff51f9352a5d1f3d`
(`/tmp/jed-swift-native-sim-resolver-fixed.log`, output directory
`/tmp/jed-swift-sim-results-20261106-19-eto6n2`): **8 PASS, 10 FAIL, zero skipped**.
Nine failures explicitly lack simulated service routing; the other exposed a
checker assumption that committed root leaves could not retain overflow payloads.
The reviewed correction validates retained bytes against their real sealed
chains and has been published for the next gate. Simulated service and fenced
join islands remain unfinished. The experiment remains opt-in and incomplete.

## Previous integration checkpoint (2026-11-05 23:50 CST)

The new interval, two-base join and GiST query paths compile together with the
single-process simulation runner and independent image/live integrity checkers.
The focused integration command was:
`mise run swift-native:test --sql-file ddl/alter_table_type_primary_key.test --trace-sql --filter 'SQLCorpusBoundary|IntervalBoundary|GistBoundary|SimIntegrityBoundary|SimulationBoundary|SimulationRunnerBoundary|ResourceTests|SpecDataTests'`
(`/tmp/jed-swift-native-access-simulation-integration.log`). **33/34 tests passed**;
two assertions in one interval byte-adjacency test failed. The selected SQL file
completed all 57 records, 43 cells and three cost assertions with zero failures.
Both integrity tests passed, including a validly encoded row with a wrong primary
key that the independent checker rejects; the first shared simulation regression
also reproduced its transcript and operations hash twice. This is not all-trace
or whole-corpus acceptance. Simulated service and timed relaxed flushes remain
unfinished and fail explicitly.

Three `--all-sql` attempts were interrupted after sustained CPU use; none
produced a completion report. Direct progress tracing identified
`ddl/catalog_long_table_indexes.test:21`: overload probes repeatedly resolved
the same nested arithmetic operands, making a 40-term expression exponential.
The same issue stalled a long concat in the encrypted simulation trace. A local
operand retention fix preserves contextual literals/parameters/arrays/rows while
reusing compound expressions across JSON/container/scalar operator probes.
The unchanged catalog file now completes **16/16 records in 0.254 seconds**.
Focused gate **11/11 PASS**:
`mise run swift-native:test --sql-file ddl/catalog_long_table_indexes.test --trace-sql --filter 'SQLCorpusBoundary|IntervalBoundary|SimIntegrityBoundary|ExpressionBoundaryTests/testConditionalParameterAdoptionAndByteInputBoundaries'`
(`/tmp/jed-swift-native-resolver-long-chain.log`, 2026-11-06 00:00 CST).
The interval test correction follows the frozen prefix-successor contract;
the new 7-dimensional secondary-index boundary also passes. No shared expected
SQL or cost result changed. Full 749-file verification must still complete.

The preceding native filesystem/simulation foundation gate passed **44/44**
(`/tmp/jed-swift-native-simulation-foundation.log`), and the rebuilt native
process actor passed **31/31** after that host seam refactor
(`/tmp/jed-swift-native-process-filesystem-seam.log`; executable SHA-256
`0a531a19a605dd2cd82644d275c00631379e27fc1efba5634977563c7de31edc`).
Commit `0afb7301` records that independently verified host foundation, local-only.
Mechanical inclusion of the baseline's 18 simulation regressions and 22 profiles
expanded the frozen input fingerprint to
`369ad9def9a39d16e8812714f9100f1ad86df6822910021c32cfa19e4c2e7bfc`;
codegen and `--check` pass. The baseline revision and 749 SQL files did not change.

Next: correct adjacency, finish the direct-traced whole corpus, route all seven
shared concurrency schedules through an audited adapter, complete simulation
service/deadline coverage, then continue full acceptance and measured release
optimization. All changes remain opt-in and all commits local-only/unpushed.

## Previous checkpoint (2026-11-05 23:26 CST)

The expanded gate compiled and completed:
`mise run swift-native:test --filter 'SQLCorpusBoundary|GinBoundary|ResourceTests|SessionCursorTests'`
(`/tmp/jed-swift-native-sql-344-gin-merge.log`). **343 frozen files completed**;
one mistakenly selected postbaseline GIN regression was absent from the frozen
bundle and explicitly reported as an adapter block. It is now pinned in the
separate `PostbaselineSQLTests` lane. The run consumed 9,743 records
(6,278 statements / 3,465 queries), 15,533 cells, 921 cost / 893 name / 14 type
assertions and two host actions; **78 SQL failures**, no failed-action aborts.
All 21 tests outside the whole SQL test passed (nine GIN boundaries, five resource
invariants, four session cursors, three adapters). 406 baseline files remained
outside that run; the earlier printed 405 counted the mistaken extra selection.

GIN physical scans, candidate estimates, OR gathers, scalar-array/JSON/path
operations and MERGE now compile together. Term-statistics failures fell from
19 to one. Review traced the new failures to array comparison promotion hiding
bare GIN columns, quantified varlen charges, row-independent concat growth,
statistics fraction scaling, MERGE source aliasing/qual ordering, and existing
join/interval/GiST gaps. The eager materialization filter move introduced one
resource regression: only a proven scan-pushed filter may precede row admission,
not a whole WHERE. That move has been reverted. New tests also exposed a collated
top-k temporary-memory mismatch. No expected SQL or cost result was changed.

The next gate uses explicit `swift-native:test --all-sql` to attempt all **749**
frozen SQL files, with both later shared regressions counted separately. Interval
planning, join access selection and simulation are being developed independently.
The experiment remains incomplete, local-only/unpushed, and excluded from defaults.

## Previous checkpoint (2026-11-05 23:13 CST)

Latest completed gate:
`mise run swift-native:test --filter 'SQLCorpusBoundary|SerializationBoundary|SessionAdmissionBoundary'`
(`/tmp/jed-swift-native-sql-308-prefix-host.log`): **308/308** selected whole files,
8,922 records (5,875 statements / 3,047 queries), 13,764 result cells,
728 cost / 893 name / 14 type assertions, two host actions; **56 SQL failures**,
zero adapter blocks or failed-action aborts, 441 frozen files not yet admitted.
All seven tests outside the whole SQL test passed: three adapter, two named
serialization, and two session admission tests. The newly admitted collation skew
and upgrade files passed. LIMIT estimation, expression-key default collation
identity, and decimal comparison bound admission now pass their prior failures.
Named attachment serialization pins its committed domain without waiting for a
writer or pinning main; eager write sessions own their gate immediately.
GIN query execution/term estimates, MERGE, and eager scan accounting are being
integrated for the next gate; this is not a completed-core acceptance result.

## Previous checkpoint (2026-11-05 23:06 CST)

Status remains **incomplete**. Latest completed SQL gate:
`mise run swift-native:test --filter 'SQLCorpusBoundary|GinBoundary'`
(`/tmp/jed-swift-native-sql-289-gin-with.log`): **289/289** selected files,
8,528 records (5,645 statements / 2,883 queries), 12,851 result cells,
588 cost / 893 name / 14 type assertions; **54 SQL failures**, no adapter
blocks, 460 frozen files not admitted. All nine GIN boundary tests and both
adapter tests passed. GIN DDL, incremental posting maintenance, rebuilds, and
ordinary/upsert mutation history now run through shared stored-index plans.
Recursive views and writable-WITH EXPLAIN are connected. The preceding 282-file
SQL/storage gate completed all selected files with 77 SQL failures, no adapter
blocks, and all ten storage-domain invariant tests passing
(`/tmp/jed-swift-native-sql-282-bootstrap.log`). SELECT INTO EXPLAIN now acquires
its normalized write target.

The mixed process matrix now passes **496/496** (31 scenarios × 16 pairings of
Swift/Rust/Go/Node), including ordinary/encrypted stream modes and all backup
byte comparisons: `/tmp/jed-swift-native-process-mixed-fixed.log`. The focused
backup rerun also passed all 16 pairings. Swift actor snapshot SHA-256:
`392161151fbe4add55a0219335a28c616b482262189958c99fe0d40a93c53a41`.
The byte difference was fresh-file reclamation initialization: creation now
starts with the same zero compaction baseline as the supported cores, while
reopening preserves its recovered live count. A new allocation-history test
pins the distinct fresh/reopened paths. The prior 484/496 result below is
historical, not an outstanding backup failure.

The subsequent gate now compiled and ran B-tree candidate inventory,
complete-pipeline pricing, physical index-to-table cursors, shared DML target
selection, and collation host migration:
`mise run swift-native:test --filter 'SQLCorpusBoundary|ValueCodec|CollationMigrationBoundary'`
(`/tmp/jed-swift-native-sql-305-access.log`). **305/305** whole files completed,
8,904 records (5,868 statements / 3,036 queries), 13,747 cells,
726 cost / 893 name / 14 type assertions; **62 SQL failures**, no adapter
blocks, 444 frozen files not admitted. All 14 tests outside the whole SQL test
passed: four collation migration, eight value/key boundary, two adapter tests.
The added 16 index/collation files expose remaining paths; temp-index,
statistics_p9, basic EXPLAIN and EXPLAIN-estimates files now pass. New key framing
checks consume the shared ordered-slot bytes and cover recursive composite depth,
variable storage-key suffixes, malformed metadata and missing suffixes.

Remaining failures include GIN/GiST and interval access inventory, secondary
access in joins, LIMIT prefix estimates, expression-key collation identity, and
some exact frozen bound-admission details. MERGE is being implemented separately.
The collation host APIs use retained metadata and atomic rebuilds; no-op and
catalog-only autocommits leave temporary ON COMMIT actions alone. The tests pin
skewed-file migration, retained old readers, rollback/failure phases and canonical
GIN rebuild packing. Named attachment serialization and adapter migration actions
are the next host slice. Remaining acceptance still includes the rest of the
frozen SQL corpus, shared host/concurrency/simulation/interchange and encrypted
coverage, then measured release optimization. Apple platforms remain unverified.
All commits remain local-only/unpushed; no default task depends on Swift.

## Previous checkpoint (2026-11-05 22:42 CST)

Status remains incomplete. Latest gate:
`mise run swift-native:test --filter 'SQLCorpusBoundary|PostbaselineSQL|SequenceWriterBoundary|AttachmentMaintenance|PageServiceClient|SessionCursor'`
(`/tmp/jed-swift-native-sql-281-explain-dml.log`). **All 17 tests outside the whole
SQL corpus test passed**: attachment maintenance, page clients, session cursors,
sequence-writer admission, two adapter tests, and the separate shared KMV
regression. **281/281** selected whole files completed from 749 frozen files:
8,295 records (5,452 statements / 2,843 queries), 12,154 result cells,
566 cost / 893 name / 14 type assertions; **79 SQL failures**, no adapter blocks,
468 files not admitted. The preceding expanded gate had 128 failures and one
adapter block (280/281 completed). No expected result was edited or discarded.
The nosort header correction follows the shared runner; exact metadata directives
and all result cells still compare. The separate postbaseline KMV case passed all
14 records and 12 result cells without changing the frozen corpus fingerprint.

Cross-domain temp-view DML, external main ALTER/private-view dependencies,
REINDEX and deterministic bounded ANALYZE are now connected. The latest gate has
no failures in the previously failing temp-view, temp type-narrowing,
`reindex_jed`, savepoint, or introspection-statistics files. Remaining failures
concentrate on GIN DDL/maintenance, secondary-index planning, writable WITH
EXPLAIN, and estimate/shape details. Read, DML, and CTAS EXPLAIN now share retained
preparation and actual-cost instrumentation. The latest gate verifies retaining
analyzed results under Q1 before rendering, the unguarded display-cost boundary,
and metadata-only classification of main-sequence effects in temporary DML.
SELECT INTO writer routing still needs a small normalization correction.

Native `mise run swift-native:process --no-build` passes **all 31 scenarios**
(`/tmp/jed-swift-native-process-all-rerun.log`), including ordinary and encrypted
Swift-to-Swift streams, attachments, backup, handoff, and exact live-page limits.
The preceding full run passed 28/31. Its fixes reject a second durable writer at
COMMIT, use the active catalog for existing-writer admission, and inspect
provisional resident metadata without an extra autocommit read pin that could
interrupt post-commit maintenance. That last change also passed 16 focused
Session/Database/admission tests (`/tmp/jed-swift-native-routing-admission.log`).
The mixed-core matrix completed **484/496 pairings** twice, including an immutable
Swift actor snapshot (`/tmp/jed-swift-native-process-mixed-snapshot.log`). The 12
failures are backup byte-identity differences; all functional scenario steps pass.
Investigation traced the first difference to newly created Swift files inheriting
open-time reclamation accounting, delaying first orphan-page reuse. A focused
creation-accounting correction and allocation-history test are being prepared.
The process task now snapshots its Swift executable so later builds cannot change
actors halfway through a matrix.

`mise run corpus:check spec/conformance/suites/ddl/reindex.test` and
`mise run corpus:check spec/conformance/suites/query/analyze_oracle.test` both
regenerated byte-identically against this checkout's PostgreSQL oracle.
Logs: `/tmp/jed-swift-reindex-oracle.log`, `/tmp/jed-swift-analyze-oracle.log`.

An independent collector review found and reproduced a shared KMV boundary
bug: exactly 4,096 distinct hashes plus duplicate rows incorrectly entered
estimation. Separate local-only commit `616b3a3e` corrects Rust/Go/TS and adds a
shared regression, preserving the canonical exact-count rule. Focused statistics
conformance passed all 6 memory and 4 disk files in each core (two existing disk
exclusions); formatting and all TS typecheck configurations passed. Logs:
`/tmp/jed-kmv-{go,rust,ts}-after-native.log`. Swift's collector follows
this rule and passes the separate postbaseline regression.

The native stream process lifetime uses stable pidfd identities, bounded
termination and reaping, independently reviewed for application SIGCHLD handlers.
Earlier transport/domain gate passed 18 tests
(`/tmp/jed-swift-native-pidfd-domains-2.log`). Default `mise run verify` passed again after the shared KMV correction
(`/tmp/jed-swift-default-verify-kmv.log`). Frozen-input fingerprint remains
`52909f248d3829bbbed754ca784fa0a4e5a3b105af0fa505028683b5cb7c6249`.
All experiment commits remain local-only/unpushed. Active next work: close
EXPLAIN/GIN/planner gaps, widen whole SQL admission, complete shared host,
concurrency/interchange, encrypted, simulation and release-performance acceptance.
Apple platforms remain unverified. See `Tools/BASELINE_FINDINGS.md` for explicit
spec/baseline mismatches. Older entries below are historical.

## Accepted constraints

Read [experiment design](../../spec/design/swift-native-experiment.md),
[scope matrix](../../spec/design/swift-native-scope.md),
[host/tool matrix](../../spec/design/swift-native-host-matrix.md), and
[crypto audit](../../spec/design/swift-native-crypto.md). Full baseline surface is the
target; never silently filter failures or infer completion from a partial SQL pass.
No default tasks, website integration, master merge, or package publication.
The separate user work under `tasks/memory` is not part of this experiment.

## Environment verified

- `git log -1 --format='%H %s'`: baseline above, correlated aggregate FILTER/window
  touched-set fix.
- `swift --version`: Swift 6.4.2 (`swift-6.4.2-RELEASE`),
  `x86_64-unknown-linux-gnu`.
- Swift path: `/home/jack/.local/share/swiftly/bin/swift`.
- `git switch -c codex/swift-native-experiment`: succeeded after requesting sandbox
  access for the read-only Git metadata; no unrelated files changed.

## Work ledger

- Initial architecture/lifecycle and full scope/host acceptance matrices persisted.
  All 475 capability rows remain unverified: foundation tests are not SQL conformance.
- Documentation erratum: the format's meta table and encrypted-format introduction
  still said v49 although format.md's version scope, UNIQUE-ownership change, all
  supported cores and golden bytes are v50. Corrected those two stale labels only;
  no byte format, fixture or expected result changed.
- Implemented foundation: byte-exact UTF-8 text/names; structured errors; open type
  and lossless value representations; base-10^9 exact arithmetic (including uncapped
  aggregate addition, PG division scales, decimal key/body codecs); bounded byte
  reader/writer; CRC32/CRC64; pinned LZ4; cost/scalar/query-memory accounts and logical
  sizes; atomic cancellation; memory and Unix positioned-I/O byte hosts.
- Approved crypto integration: swift-crypto 5.1.1 at
  `f8fad2f95ad67b45f6340c0960e2396094d67e07`; required cryptographic serialization
  dependency swift-asn1 1.7.4 at `917b472dbe53ba699cbb0495f9a95a084bc6364d`;
  reference Argon2 20190702 at `62358ba2123abd17fccf2a108a301d4b52c01a7c`, portable
  `ref.c` with `ARGON2_NO_THREADS` preserving lanes. Package.resolved and vendored
  source provenance/checksums are tracked. SHA224 uses the pinned bundled backend
  through a narrow synchronous buffer borrow; other primitives use public Swift APIs.
- Opt-in build/test/resolve/vendor/codegen mise tasks added, with package-local
  caches and frozen baseline metadata. Default tasks do not depend on Swift.
- Independent foundation review found no current byte/accounting defect; flagged
  future session lifetime-limit freezing and per-record concurrent peak isolation.
  Independent crypto/tooling review verified pinning and native-code confinement.
  Full durability, accounting and adapter review remains required after integration.
- Added byte/position-preserving lexer, lazy script splitting, exact numeric
  functions and guarded transcendental kernels. Separate numeric and crypto reviews
  found no actionable defect in those scoped modules; SQL corpus execution is still
  required. Added key-block selection/rewrap and plaintext/encrypted page codecs.
- Added v50 meta encoding, manifest construction, bounded page-at-a-time recovery,
  bootstrap handling, overflow identity checks and explicit host-error propagation.
  Encrypted meta authentication follows keyless selection at the pager seam.
- Added native SQL AST/parser, recursive value/key codecs, temporal parsing and
  functions, and explicitly loaded pinned JTZ/TZif timezone data. Fixture adapters
  use every case in the eight key-encoding datasets and shared temporal/TZ vectors.
- Added pager stabilization/publication, cumulative relaxed manifests, failure
  poisoning/revocation, deterministic allocation, persisted free-list handling,
  packed PAX leaf regions, and overflow chains. These are tested storage primitives;
  a database/tree/catalog transaction path is not yet connected.
- Independent storage review caught an equal-txid fallback defect: an incomplete
  sibling's valid header cannot make it the durable floor. Swift now validates the
  sibling manifest before parity selection. The same existing issue was found in
  supported cores; the separate correction/regression slice is committed locally
  as `196feb8a`. Focused Go/Rust tests, all 1,276 TypeScript unit tests and default
  spec verification pass. The pinned Swift scope baseline remains unchanged.
- Independent codec work found reversed f32/f64 stable type codes in the initial
  Swift enum; corrected to canonical f64=12/f32=13, with a byte-boundary regression.
- No complete SQL resolver/executor, database, complete tree/catalog commit path or
  conformance adapter yet. Parser acceptance is not SQL conformance.
  No database-level encryption, recovery, process, simulation or interchange result.
- No performance baseline or Apple-platform verification yet.
- Frozen parser audit covers all 21,525 SQL records in 749 files: 21,171 parse,
  354 reject with the expected SQLSTATE, zero unexpected rejection. The 262
  accepted records expecting eventual `42601` still require later semantic checks.
- Added persistent packed B+tree mutation, bounded cursors, interior skeletons,
  CLOCK leaf pool with single-flight loading and invalidation, incremental and
  whole-image serialization, and deterministic large-value disposition/kept chains.
  Three frozen B+tree goldens rebuild byte-exactly. A near-capacity delete regression
  exposed an inherited empty unary subtree; Swift fixes it. The same issue has a
  confirmed Go reproduction; the separate supported-core correction is saved locally
  as `7b4883b6`. Focused tests in all three cores, two shared simulation traces,
  TypeScript type checks, selected formatting and default spec verification pass.
- Added datetime and exact numeric template processors, exact IEEE-to-decimal
  conversion, JSON syntax/spans/canonical values, accessors/mutations/builders and
  streaming JSON SRF helpers. SQL resolution/evaluation is not connected yet.
- Added v50 catalog metadata, two-pass nominal type resolution, reference/statistics
  validation and continued-entry page chains. Catalogs from all 87 ordinary golden
  images (including six encrypted images) decode and rebuild byte-exactly. All 112
  file fixtures are now snapshotted from the frozen baseline; malformed/recovery
  database-level acceptance remains separate and incomplete.
- Added handwritten regex compiler/VM, SIMILAR translation and scalar/SRF helpers.
  All shared regex program/match/SIMILAR vectors pass, including exact instruction
  listings, captures and step counts. Regex SRFs retain compact capture/span state
  after the specified whole-search admission and materialize one row per next call.
- Added native float input, exact shortest-round-trip output and scalar math kernels;
  three internal float tests pass. Added recursive total/SQL comparison and static
  type rules; an independent key-order agreement invariant passes. Independent
  review caught and corrected JSON-family element-wise array cast admission.
- Added JSONPath compiler/evaluator and a retained query cursor. The frozen kernel
  adapter passes 723 records / 898 cells; 133 records are explicitly excluded because
  they need SQL resolution, relations, session state or SQL/JSON syntax. This is not
  SQL conformance. Initial probes had 30 failures, corrected against unchanged shared
  inputs/expectations: adapter SQLSTATE extraction, numeric member names/comments,
  and decimal method negative/over-precision scales. Exists/first/match retain at most
  one value while preserving required evaluation; arithmetic/comparison operands
  collect the sequences required by jsonpath.md. Relation integration must reserve
  producer rows and charge generated_row. The array function intentionally collects
  and charges its growth.
- Added GiST scalar/range tuple bounds, canonical resident R-tree construction/search
  and page codecs. Four plaintext goldens and malformed/encrypted boundary tests
  pass, together with actual encrypted golden rebuilding and empty-root/equal-endpoint
  refinements. Independent review found no additional primitive defect.
- Added GIN term/query extraction, exact posting runs, packed-tree edits and span-based
  gathering. Six tests cover shared run/history/JSON term vectors, nine plaintext or
  encrypted golden images, candidate/cost/cancellation behavior and malformed data.
  GIN candidate buffers have explicit lifetime but no new logical-memory charge:
  the frozen Q1/Q2 schedule does not charge this storage-read staging. SQL row admission
  remains required; this is not a new whole-engine memory guarantee.
- GIN integration uncovered a supported-core false negative for a strict root-string
  JSONPath equality. PostgreSQL 18's forced GIN scan has the same bug, while its direct
  predicate/sequential scan returns the row. Separate local correction `2e13d1b8`
  widens the candidate term to KEY OR VALUE in all cores; shared regression, oracle
  check, 36 scoped memory/disk suite executions and default verification pass.
  The divergence is documented in gin.md §9; the original frozen baseline is retained.
- Added loaded Unicode/JCOLL/JUCD decoding/merge, UCA sort keys and casing; three
  tests include 46 frozen sort-key vectors and loaded-versus-bare environment behavior.
  Initial delta decoding followed a stale README layout; corrected to the canonical
  frozen counts-first bytes. No host Unicode/ICU fallback or compiler claim.
- Scalar casts and the first typed expression resolver/evaluator now compile and
  pass 432 frozen expression records, 527 result cells, 18 costs and five type
  assertions; 966 records are explicitly excluded. This uses the real expression
  path, but does not establish query/database conformance. Initial probes caught
  missing adapter timezone/clock directives, literal JSONPath/NULL-cast costs,
  temporal arithmetic wiring and an interval positional-field bug, all corrected
  against unchanged shared expectations. Clock-relative dates remain runtime nodes.
  Full function/container/subquery resolution and query execution remain incomplete.
- The immutable corpus snapshot now contains all 749 baseline files (742 SQL,
  seven concurrency). Expanded expression probes pass 642 records / 740 cells,
  72 costs and 11 type assertions; 3,630 records in the selected cast/type/expression
  files remain excluded by the explicit AST/query-shape gate. Expansion initially
  exposed 73 failures (preserved in `/tmp/jed-swift-expression-expanded.log`):
  text/bytea/jsonb concatenation routing, interval scaling, and preparation-only
  temporal literal charges. Independent review corrected NULLIF's left-width
  preservation and conditional parameter-registry adoption; host metadata and all
  256 octal input bytes now have boundary checks. A zero interval scales to zero
  before factor-width rejection, after divide-by-zero checking.
- Connected catalog-driven scalar overload selection, named/default/variadic
  arguments, native text/bytea functions, and recursive array/range kernels.
  Container input/output, casts, constructors, subscripts, quantified comparisons,
  range accessors/set operations and compatible array widening now enter the
  real expression resolver/evaluator. The expanded gate passes 2,202 frozen
  records / 2,425 cells / 281 cost checks / 13 type checks; 2,070 records remain
  explicitly outside its AST/query-shape gate. All 163 current tests pass.
  This still does not establish database/query conformance.
- Expansion first produced 118 failures (including one stale internal shape
  assertion), then 19 and two before passing. Fixes include handwritten overloads
  whose signatures are specified outside the catalog, recursive rendering costs,
  array upper-bound representation versus stricter operation limits, transient
  record/range arrays, subscript indexing, typed NULL cast costs, and numeric/
  timezone template edge cases. No frozen expected outputs were changed.
  Independent review checked container resolution and text/bytea kernels.
- The storage lifecycle source set compiles: domain snapshots/stores, exact
  commit layout/preflight, catalog-root patching, live-page deltas and deferred
  free-page accounting. Focused lifecycle tests and database writer/pin
  coordination are in progress; compilation is not durability acceptance.

## Verification ledger

2026-11-05, Linux x86-64, Swift 6.4.2, debug configuration.
XCTest timestamps below are checkout-local CST (UTC−06:00); earlier ledger
entries incorrectly labeled these local timestamps UTC.

| Command | Result |
|---|---|
| `mise run verify` | PASS: all default spec/fixture/catalog/codegen checks; 112 file goldens, 24 integrity images, 738 encoding vectors, corpus taxonomy and other existing checks |
| `mise run oracle:status` | PASS after sandbox socket access: existing PG18 `jed_oracle`, builtin C.UTF-8 and all profile pins agree; no cluster provisioning |
| `mise run swift-native:codegen --check` | PASS with expanded pinned encoding/temporal/TZ inputs and three binary fixtures; fingerprint `8d60f8b77c8a63bd5ea5815788eda7bb0743953e256a71f1d9128f26a454d974` |
| `mise run swift-native:test` | PASS 32 foundation/crypto tests, then PASS 35 after byte-host/logical-size slice; 0 failures |
| `mise run swift-native:test` | PASS 61 after lexer/numeric/encryption/meta slices; 0 failures (2026-11-05 16:31 CST) |
| `mise run swift-native:test` | PASS 92 after parser, temporal/value/key, pager and packed-page slices; 0 failures (2026-11-05 16:45 CST) |
| `mise run swift-native:parser-audit` | PASS 21,525 frozen SQL records; zero unexpected rejections (see Tools/PARSER_AUDIT.md) |
| `mise run swift-native:test` | PASS 113 after tree/disposition, temporal/numeric formatting and JSON foundations; 0 failures (2026-11-05 17:01 CST) |
| `mise run swift-native:test` | PASS 120 after catalog/golden and regex/SIMILAR slices; 0 failures (2026-11-05 17:08 CST) |
| `mise run swift-native:test` | PASS 122 after comparator/key invariants and regex admission; 0 failures (17:14 CST) |
| `mise run swift-native:test` | FAIL 127 tests / 30 JSONPath kernel probe failures; adapter and kernel issues preserved in `/tmp/jed-swift-jsonpath-first.log` (17:16 CST) |
| `mise run swift-native:test` | PASS 133 after JSONPath fixes, float kernels and GiST slice; 0 failures (17:20 CST); same 133 PASS after scalar-cast source compilation (17:22 CST) |
| `mise run swift-native:test` | PASS 135 after retained regex producers and encrypted GiST golden (17:24 CST) |
| `mise run swift-native:test` | FAIL 138 / two new Unicode decoder failures (17:30 CST); subsequent expression probe failures retained in `/tmp/jed-swift-expression-first.log` |
| `mise run swift-native:test` | PASS 146 after Unicode/interval corrections, GIN vectors/goldens and scalar-expression integration (17:35 CST) |
| `mise run swift-native:test` | FAIL 147 / 73 after full-corpus expression expansion; then PASS 147 after corrections (17:41 CST), including 642 shared expression records |
| `mise run swift-native:test` | FAIL 156 / 118 after function/container expansion, then 160 / 19 and 163 / two; PASS 163 (17:56 CST), including 2,202 frozen expression records |
| `mise run swift-native:codegen --check` | PASS frozen cast/type/expression corpus, Unicode and GIN fixtures; fingerprint `76da6b8479747b97050e7c7709a6ec4bd57bd2f322bd5d85aa781e1aa7547c28` |
| `mise run swift-native:codegen --check` | PASS all 749 frozen corpus files; fingerprint `37257dce1710682ff656ee83474597acd4adc91e8f040d8155e9d16e8b63ff50` |
| `mise run swift-native:codegen --check` | PASS all 112 frozen file fixtures plus regex vectors; fingerprint `802cfd6bda26761f7dc3c91d811278ced3bd0a4e368c3e11f3d6b21e917ba77f` |
| `mise run unit:go -- -run 'Test(Validated\|CommitCRC64\|CompleteManifest\|IncompleteEqualTxid\|Serialization)' -count=1` | PASS 8 selected recovery tests |
| `mise run unit:rust -- --lib validated_cow_tests` | PASS 7 selected recovery tests |
| `mise run unit:ts` | PASS 1,276 tests after sandbox Unix-socket access; initial sandbox-only EPERM failures resolved |
| `mise run durability-review …` (temporary wrapper, removed) | PASS 27 targeted TS recovery tests, TS typecheck, selected Rust/Go/TS formatting checks |
| `mise run verify` after shared recovery correction | PASS all default spec checks |
| Focused Go/Rust/TS PMap tests; `mise run tree-regression` | PASS shared empty-unary-subtree regression, plus delete-merge-separator-overflow simulation; exact transcripts and images agree |
| `mise run typecheck:ts`, selected formatting and `mise run verify` after shared deletion correction | PASS |
| `JED_CONFORMANCE_FILTER=gin_jsonb_ mise run conformance:{go,rust,ts}` (three commands) | PASS six memory + six disk suites per core; initial sandbox Unix-socket denials resolved by scoped reruns |
| `mise run corpus:check spec/conformance/suites/query/gin_jsonb_root_string.test` | PASS, byte-identical oracle output; separate forced-index PG18 probe confirmed the documented bug |
| Selected formatting, independent review, `mise run verify` after GIN correction | PASS |
| `git diff --check` | PASS before first integrated foundation test |

The 35 tests include all 11 shared LZ4 and 41 decimal key vectors, arithmetic
invariants, UTF-8/byte corruption and host boundaries, resource precedence/floors,
memory/Unix byte-device behavior, digest/AES known answers, the three shared XAES
cases, two subkeys, one KEK, one shared Argon2 vector, wrapped-key ciphertexts,
and an upstream multi-lane Argon2 vector. They do not establish SQL/file parity.

The 61-test run additionally proves lexer/script source boundaries, numeric guarded
kernel/account invariants, exact key-block/rewrap bytes, encrypted per-page golden
authentication/resealing, manifest chain/torn-content fallback and host-I/O failure
propagation. An initial integration run caught a stale v49 check in the new Swift
encrypted envelope; corrected to v50 without changing fixtures. New meta tests
initially expected a high-water bound without a file length and compared an unsealed
meta CRC with an authenticated one; corrected those test assumptions to the spec.
No shared expected results were changed.

The 92-test run additionally exercises all frozen key vectors/ordered-slot groups,
shared temporal/TZ vectors, parser source/nesting boundaries, byte-host pager write
order, torn writes and stabilization, relaxed cumulative manifests and failed-cap
read revocation, packed regions and malformed offsets, deterministic free/manifest
allocation, and overflow slab/chain validation. Initial integration found a fixed
zone abbreviation missing `:00` and an adapter assuming bytea vectors used `value`
instead of `hex`; both corrected against unchanged shared fixtures. Intermediate
build failures while parser files were still being written were not test failures;
subsequent complete sources compiled and all 92 tests passed.

The 113-test run additionally covers packed-tree snapshot/cursor boundaries, per-delete
near-capacity structure, golden rebuilds, cache faults/concurrent invalidation, kept/fresh
overflow bytes and costs, numeric template arithmetic and JSON internal boundaries.
Initial disposition test inputs did not fit after one compression as assumed; changed
the input from 80 to 70 bytes to exercise the intended boundary. An audit corrected
Swift's encrypted compression quantum to C (chain payload capacity is separately C−16).
No shared expected results changed. The 120-test run verifies catalog page bytes,
continued-entry corruption, nominal cycles and malformed FK/type references. A test
initially expected the malformed-row-count image to fail at catalog open; that
fixture explicitly defers count drift until a write/integrity check. The test now
checks that boundary and separately rejects a zero root with nonzero cardinality.
This is catalog verification, not complete database/file conformance.

Local-only commits so far: `91aef346` (approved scope/design), `f9c09043`
(verified foundations), `d2a56c3b` (syntax/numeric), `e83ff6d1`
(recursive codecs/temporal), `a0cb024b` (pager/packed primitives), and `196feb8a`
(shared recovery correction). The foundation commit's staged whitespace check reported two existing
whitespace defects in pinned, unmodified Argon2 upstream LICENSE/encoding.c. Those
vendor bytes remain unchanged to preserve the recorded provenance hashes.

Additional local-only commits: `b9e99410` (frozen syntax audit/tree fixtures),
`a645434f` (packed B+trees/disposition), `a87abe2c` (formatting/JSON foundations),
`16569ff1` (catalog codecs and frozen images), `7b4883b6` (shared deletion correction).
Further local-only checkpoints: `111932fa` (float/regex/type foundations),
`2e13d1b8` (shared GIN root-string correction).
Further experimental checkpoints: `4c14d61c` (GiST), `ba7859bd` (JSONPath and full
frozen corpus), `35579e82` (GIN and loaded Unicode runtime), all local-only.
`e2503626` records the initial typed expression/scalar-cast slice, also local-only.

Initial SwiftPM compile failed while SQLDecimal was not yet present, then four
missing `try` markers in decimal test autoclosures were corrected. No expected
results changed. Swift 6.4's default `swiftbuild` stalled when the sandbox denied
CFSocket creation; interrupted experiment processes were identified and stopped.
Opt-in tasks now select its supported `--build-system native` option, which builds
successfully and emits an upstream deprecation warning. No sandbox IPC dependency
is introduced into the engine. SwiftPM also warns about the direct swift-asn1
constraint, retained intentionally to pin CryptoExtras' required transitive package.

## Current integration checkpoint

Checkpoint `b7324c9b` saves the 250-test integration below, local-only and unpushed.
The Linux page-service slice is checkpointed separately at `40c9c6a4`, also local-only.
Work after it remains active. The latest complete integrated run (20:50 CST) ran
**287 tests in 118.181 seconds**: all **286 tests outside the expanded whole-file
SQL test passed**. That SQL test ran **187/749 files, 5,038 records (3,100 statements /
1,938 queries), 7,800 cells, 482 cost / 874 name / 11 type assertions; 91 failures**.
All records in every admitted file execute; no failing file or expectation was
removed. Remaining failures include writable CTEs, ON CONFLICT, ALTER column
rewrites, introspection/temp domains, EXPLAIN and two multi-table join costs.
The 20:43 SQL-only run had 92 failures; the subsequent DISTINCT ON aggregate-key
identity correction removed one. Earlier 178-file and 166-file runs had 124 and
117 failures respectively. No frozen expected outputs changed.

The selected ordinary and ordered-set/hypothetical window files, basic/nonrecursive
CTEs, recursive structure/SEARCH/CYCLE, all 11 subquery files, composite casts,
JSON record/populate, both JSON_TABLE files, JSON Unicode errors, DISTINCT ON and
main CTAS file pass. CTAS/SELECT INTO remains incomplete for temporary domains and
writable CTEs. Real columnar projection/group lanes, bounded top-k and eager-filter
lifetimes now satisfy the selected query-memory cases apart from EXPLAIN gaps.
Subqueries use isolated lexical scopes, statement-shared initplan results,
depth-aware outer references and explicit Q1 result lifetimes. Independent review
corrected scalar-subquery collation provenance and replaced recursive ordinary
expression initplan walking with an explicit stack.

The row-valued corpus pins IN-subquery as a flat node visiting every result
element; grammar.md §26's older OR-chain description is stale (literal IN-lists
still use an OR chain). Recursive UNION hash admission follows the frozen core
batch schedule (sum payloads before the cost guard); prose suggesting a per-key
guard needs a separate shared-spec audit.

Linux automatic owner/client activation is now connected. Native tests cover 27
frozen frames, two owner records, 14 malformed vectors, streamed deltas, Unix
framing/close wakeup, encrypted foreign commits, request multiplexing, retained
invalidations, role-epoch revocation, real-process election/promotion, read-only
handoff, lost-ack resolution and writer timeout/cancellation during election.
Independent review identified unsynchronized core publication and admission waits
that could outlive writer deadlines; both were corrected before this run. A first
full run was stopped after diagnosing the process-test helper's fixed-fill pipe
read; it now uses one polled native read. The focused automatic/role/file-host gate
then passed all 16 tests before the completed full run. Post-ack storage maintenance
and the shared process/simulation adapters remain outstanding.

The current snapshot fingerprint is
`7a1bcf53fefb01e43484e760a68d9f61faabbc0c15c35d2b1da39c4c90db8043`.
Latest command: `stdbuf -oL -eL mise run swift-native:test`, log
`/tmp/jed-swift-native-full-187-fixed.log`. The adapter reports every mismatch;
XCTest records the first 40 plus one summary failure while printing the remaining
51. The earlier 43 selected non-SQL tests (including CTAS privileges/parameters,
foreign keys and host boundaries) also passed. Scope rows remain unverified until
the full applicable acceptance evidence is available.
A final independent accounting review found JSON_TABLE admitted Q1 rows while
later column expressions were still evaluating. Production now completes first,
then generated-row charges run, then the relation admits Q1, as memory.md §5.1
requires. The follow-up `mise run swift-native:test --filter
'SQLCorpusBoundary|CreateTableAsBoundary'` completed at 20:55 CST: both CTAS boundary
tests and two corpus-adapter boundary tests passed; the 187-file SQL run retained
91 failures with unchanged counts. Log: `/tmp/jed-swift-native-sql-187-json-admission.log`.
Review also identified the same delayed-admission requirement for other bounded
SRFs and a missing general statement privilege-preflight phase (a denied-source
CTAS must reject before an invalid target name); these are outstanding corrections.


Next integration (after local-only SQL checkpoint `f615ed6b`) adds the nine main
catalog relations, ALTER ADD/DROP/TYPE/PRIMARY KEY rewrites, initial costed N-way
base join islands, bounded-SRF admission corrections and post-ack page maintenance.
The first 219-file attempt compiled after two Swift-only annotation/exclusivity
fixes. At 21:08 CST, all 14 selected native tests passed (CTAS2, FK2, Automatic7,
Maintenance3), as did two corpus-adapter checks; the SQL run aborted while parsing
`introspection/attached.test` because its `attach` harness action is unconnected.
Log: `/tmp/jed-swift-native-sql-219-3.log`. This is **not** a completed219-file run.
The adapter now records each blocked file as an explicit failure, continues with
other selected files and distinguishes completed from selected files. No file or
expected result was removed. A follow-up run is pending, with statement privilege
preflight, three whole privilege files and backup boundary coverage being added.

Independent review verified main-domain catalog shape/rendering/cost and corrected
N-way physical/logical cardinality and per-prefix INL Q1 retention, ALTER CHECK
name ordering and deferred preparation-cost admission, and bounded JSON producer
row-cost timing. Statement privilege review corrected short INSERT-row default
selection, first duplicate target selection, non-updatable invoker-view descent
and function error ordering. TEMP/attachment catalog scopes, MERGE-view admission
and the shared process/simulation adapter remain incomplete.

Potential separate shared defect: frozen ALTER TYPE default conversions accrue
lifetime cost in their preparation meters and again when transferring prepCost to
the statement meter. Preserved for frozen conformance, not adopted as new design.
Evidence: Go ddl.go2723–2733/2873–2877 and engine.go1442–1446; Rust executor/ddl.rs
3658–3661/3874–3876 and executor/mod.rs1271–1282; TS executor.ts10163–10174/10310–10312.

Verified integration (2026-11-05 19:24 CST): **250 tests passed, zero failures**
with `mise run swift-native:test` (82.999 seconds). The public
Database/Session/Rows path now executes DDL, INSERT/UPDATE/DELETE, ordered and bounded
base scans, parameter admission, transactional sequence functions, serial/identity
defaults, initial grouped aggregates, set operations, VALUES queries, derived
relations and authored nested-loop joins, SELECT DISTINCT, and initial stored-view
reads/inserts. This is
still a partial SQL engine: costed join search/hash/INL/pushdown, complete expression
and relation sources, complete view DML/dependencies, windows, constraints, prepared plans, attachments,
temporary domains, spill integration and process page service remain incomplete.

An earlier `mise run swift-native:test` at 19:04 CST ran 242 tests: two assertions failed in
one new catalog environment test because collecting an in-memory statistics summary
duplicated its continuation arrays. The collector fix is now published and its
targeted and full tests now pass. That run's original 28 SQL files passed (618 records,
721 cells, 60 cost and 45 name assertions); the expression adapter passed its
expanded scalar/temporal selection. File-host tests cover exclusive protocol-v2
ownership, atomic no-clobber creation, encrypted opening, descriptor/lock cleanup,
fork and special-file refusal, collation grading, and retained directory durability.
Automatic owner/client page service remains explicitly unavailable.

The latest full run passes **56/749 complete SQL files**, 1,334 records (814
statements and 520 queries), 1,458 cells, 152 cost, 192 name, and two type assertions.
All records in every admitted file execute; 693 files remain outside this gate.
The final failure was all-NULL set-operation metadata: `ResultColumn.type` is now
optional so unresolved `unknown` remains observable through the host API.
Independent review also corrected definer-view INSERT privileges leaking into an
unrelated SELECT source, view DEFAULT VALUES mapping, and identity defaults.
All five new maintenance tests pass, including encrypted rekey round trips and
pre-replacement failure isolation. Independent crypto review found no blocking
maintenance defect; native replacement/reopen failure injection remains outstanding.

An earlier `mise run swift-native:test --filter SQLCorpusBoundaryTests` at 19:13 CST ran
46 complete admitted files / 1,099 records / 1,084 cells, with 92 cost, 100 name,
and one type assertion. Seven failures remained: five consequences of unconnected
view operations and two consequences of unconnected SELECT DISTINCT. No failing
record was removed. At that point 703 frozen corpus files remained outside the gate.
The 45-file run before compound/derived integration had 15 failures; a duplicated
aggregate collector entry (resolver revisiting one source call during literal
adoption) was fixed, along with the missing query paths. Subsequent commands and
results are recorded above; these intermediate failures are retained as history.

The Linux host uses a first-party CJedHost shim containing only the OS atomic
no-replace rename call; it adds no dependency or native engine algorithm. Swift
retains all storage, SQL, type and codec policy. Scratch/rebuild and host maintenance
work are opt-in and remain separately tracked in Tools/STORAGE_LIFECYCLE_PLAN.md.

Previous storage checkpoint (2026-11-05 18:20 CST): **197 tests passed**. It includes
3,235 frozen expression records / 3,793 cells / 294 cost checks / 13 type checks,
with 2,547 records explicitly outside the expression adapter's gate. JSON/SQLJSON
initially exposed 14 failures, fixed against unchanged expectations. A speculative
JSON resolver path multiplied work on nested arithmetic; conservative type-candidate
admission restored the suite's roughly 65-second runtime. This remains an expression
slice, not full database conformance.

The same run verifies seven storage-domain tests, six coordinator tests, streamed
export of 85 canonical images plus normalized metadata for two torn-meta recovery
images (all body pages exact), independent cursor pins, reader-watermark reclamation,
Q4 preflight, and memory stage/adopt failure isolation. Export retains bounded packed
leaf/page payloads and chain references rather than collecting the whole image.
Two initial Q4 failures were test defects (a throwing XCTUnwrap recorded an expected
error, and a proposed growth did not exceed the existing high-water mark). Two export
assertions initially expected repaired meta CRCs to retain deliberate fixture damage.
The corrected boundaries pass without changing storage policy or shared fixtures.

The separate handwritten Unicode compiler rebuilds shared compiler vectors and
production collation/casing/bundle artifacts byte-exactly. Its command-line smoke
check, `mise run swift-native:unicode -- bundle --preset non-cjk --spec-dir spec
--out /tmp/jed-swift-native-unicode-smoke.jucd`, produced 340,621 bytes identical to
the shared fixture. Current snapshot fingerprint is
`7024bdb84a3dc257a81d965161303df5ae427f0870a4c5750dcc99432d6d7f83`.
Environment sources, host-kernel boundaries and lazy per-block clock/UUID services
are included in the 197-test run. New Database/Session cursor tests and expanded
clock/seed/variables expression probes added afterwards pass in the 250-test run.
`mise run swift-native:codegen --check` also passes at the current fingerprint.

## Next steps

1. Active delegated slices: Linux protocol-v2 page service (`host_inventory`),
   columnar/memory/join planning and corpus admission (`crypto_audit`), views and
   FK actions/ALTER/constraint DDL (`scope_inventory`). Root owns query/session integration
   and verification. Keep source publication coordinated with integrated checks.
   The experiment is not frozen.
2. Extend the whole-file corpus gate and connected execution paths. Audit cost
   charge granularity, view dependencies, costed join planning, resource accounting,
   and rejected/unconnected SQL surfaces. Do not substitute decoded dictionary
   storage or engine-owned cursor collection.
3. Continue vertical slices across expressions, SQL, pager/storage and host APIs,
   verifying against shared data/corpus at every slice and retaining failing counts.
4. Complete full verification and independent reviews, then measured optimization
   and final verification/benchmark reports. Freeze only once acceptance is met.
