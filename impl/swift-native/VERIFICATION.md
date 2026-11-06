# Swift experiment verification

Status: **Linux correctness acceptance passes; performance acceptance and final
freeze remain in progress**.
Target revision: `362e0798ac3a8bd04bd2da556044b6ec73bf468a`.
Platform exercised: Ubuntu Linux x86-64, Swift 6.4.2. Apple platforms are
unverified. The package and all runners are explicitly opt-in.

This report records completed gates and their limits. The [progress log](PROGRESS.md)
contains intermediate failures, exact reproduction details and current work.
The [scope](../../spec/design/swift-native-scope.md) and
[host/tool](../../spec/design/swift-native-host-matrix.md) matrices define
acceptance; passing the ordinary SQL corpus alone does not complete them.

The durable local [evidence manifest](results/verification-preoptimization-20261106/manifest.json)
retains 50 successful reports, logs and accounting TSVs (10.86 MiB) on ext4.
It records original paths, sizes and SHA-256 values, with no actor binaries or
simulation images. Its SHA-256 is
`f5aa79e7f6e4213f7cb088a5ffdb883a1e01b9be889224c5417638f35c4a2f00`.
The ignored archive supplements this committed report; the original `/tmp`
paths below remain useful provenance references.

## Completed checks

| Gate | Command | Result and evidence |
|---|---|---|
| Ordinary frozen SQL and native suite | `mise run swift-native:test --all-sql --trace-sql` | Fifth run: 749/749 files, 21,597 records, 39,148 cells, 2,271 cost / 1,154 name / 100 type assertions; 484 native tests; zero failures or adapter omissions. `/tmp/jed-swift-native-all749-all-host-fifth.log`. Later accounting changes require fresh integrated acceptance. |
| Release ordinary/native | `mise run swift-native:test --configuration release --all-sql --trace-sql` | Latest run: **509 native tests and all 749 frozen files / 21,597 records / 39,148 cells / 2,271 cost / 1,154 name / 100 type assertions pass**, zero blocked/aborted files and five host actions. `/tmp/jed-swift-native-release-all749-eof-fourth.log`. Prior first/view-second/handoff-third logs preserve the resolved six handoff/client assertions and asynchronous EOF-observation test race. This is correctness evidence, not a performance measurement. |
| Handoff admission integration | `mise run swift-native:test --filter 'PageServiceAutomaticTests\|PageServiceClientTests\|PageServiceRoleTests\|PageServiceWireTests\|PostbaselineSQLTests\|SimServiceBoundaryTests'` | 21 debug native tests pass, including the new delayed-worker/canceled-request ordering regression and memory/disk view and GiST regressions. `/tmp/jed-swift-native-handoff-admission-debug-first.log`. |
| Final diagnostic/storage follow-up | `mise run swift-native:test --sql-files … --filter …` | 42 native tests and 78 SQL records pass: 407 shared diagnostic fixtures, 10 object cases, 20 runtime probes, four RETURNING ordering cases, GIN serialization atomicity and the unchanged depth boundary. `/tmp/jed-swift-native-diagnostics-final.log`. |
| View FROM-producer integration | `mise run swift-native:test --sql-files … --filter …` | 14 native tests, five frozen view/depth files with 301 records / 231 cells / 20 names; the new shared 41-record / 23-cell regression passes in both memory and disk modes. PostgreSQL regenerates it byte-identically and all three supported actors pass. `/tmp/jed-swift-native-view-producers-retry.log`, `/tmp/jed-view-srf-gap/peer-exact-report.json`. |
| Accounting/storage integration | `mise run swift-native:test --sql-files … --filter …` | 29 complete SQL files, 1,056 records, 1,592 cells and 71 native tests pass. Exact full selection and command are retained in `/tmp/jed-swift-native-accounting-integration-first.log`; this includes pending-index serialization, Q4 compaction and attached catalog checks. |
| Second accounting integration | `mise run swift-native:test --sql-files … --filter …` | 34 complete SQL files, 1,463 records, 4,074 cells and 32 native tests pass. `/tmp/jed-swift-native-accounting-integration-second.log`. Later collation/window and planner fixes still need integration. |
| Third accounting integration | `mise run swift-native:test --sql-files … --filter …` | 42 complete SQL files, 1,781 records, 4,364 cells, 268 cost assertions and 54 native tests pass. `/tmp/jed-swift-native-accounting-integration-third-retry2.log`. Captured actor `2087236c…` now runs the complete three-mode accounting matrix. |
| Public integrity | `mise run swift-native:test --filter …` | 21 integrated native tests, 24 damaged images through 50 exact report probes, 112 ordinary files; an additional plaintext/encrypted real-socket API test passes snapshot/pin/role-loss behavior. `/tmp/jed-swift-native-integrity-second.log`, `/tmp/jed-swift-native-integrity-sockets-retry.log`. CLI also passes all 50 frozen reports plus 11 boundary probes; `/tmp/jed-swift-integrity-cli-20261106-675919-qw5gej/report.json`. |
| Forced TEMP spill | `mise run swift-native:spill --no-build --temp` | Release refresh: 28 files, 1,255 records, 571 cells, 23 cost assertions; 26 native scratch files, zero failures. `/tmp/jed-swift-native-temp-spill-full-second.log` (with `--configuration release`). |
| Exact resource accounting | `mise run swift-native:query-memory --configuration release --no-build` | **241,412 measurements, zero failures, complete four-core/three-mode matrix**: 21,477 memory records per core and 19,438 disk plus 19,438 disk-spill records per core, exact peaks and committed storage. `/tmp/jed-swift-query-memory-20261106-1996511-bxwolu/report.json`, XCTest `944718bb…`, resource digest `0568c671…`. Prior 376 GiST-load differences are resolved by the shared correction; failed and interrupted attempts remain documented in PROGRESS and BASELINE_FINDINGS. |
| Public embedding examples | `mise run swift-native:examples` | In-memory prepared binding/transaction/typed cursor; durable committed row survives close/reopen; canceled pull reports 57014 and a later query succeeds. `/tmp/jed-swift-native-examples-first.log`. |
| Full encrypted comparison | `mise run swift-native:encrypted --no-build` | Refreshed all four cores: 77,612 records, all required file/record/cost/EXPLAIN outputs present and equal, zero failures. `/tmp/jed-swift-encrypted-corpus-20261106-939870-6kf73i/report.json`, actor `b7d500a9…`. Original adapter-failure and strict recheck evidence remain preserved. |
| Full six-direction service comparison | `mise run swift-native:service --no-build` | Refreshed Swift client with Rust/Go/TS owners and each peer client with Swift owner: 116,628 records, zero failures. `/tmp/jed-swift-native-service-full-second.log`, `/tmp/jed-swift-service-corpus-20261106-1040687-2wheah/report.json`. Captured XCTest `d292c9b4…` predates the handoff correction, covered by targeted tests and the pending mixed-process refresh. |
| Full file interchange | `mise run swift-native:interop --no-build` | Refreshed all 112 frozen images, all three supported peers, both writer orders and generated plaintext/key/passphrase/attachment cases: 2,691/2,691 phases, zero failures. `/tmp/jed-swift-interchange-20261106-996175-9832xa/report.json`. This excludes the separately required 24 integrity image/report fixtures. |
| Layer 3 concurrent stress | `mise run swift-native:stress` | Five complete core/storage runs; 20,000 snapshot invariants and 5,000 writer transactions; exact final checksum `f3e75496a3111181` everywhere. Swift uses 12 real threads in memory and durable-file modes with reopen, Rust threads, Go race detector, TS seeded interleaving. `/tmp/jed-swift-stress-20261106-19-vqyx3y/report.json`. Correctness evidence, not a performance comparison. |
| Simulation profile sweep | `mise run swift-native:sim:sweep --seeds 1..8 --no-build --configuration release` | Current release refresh: 22 profiles, 176 traces per core, 459 transcript comparisons, 7,049 independently checked Swift images, zero failures; all 23 frozen per-core-fault exemptions remain explicit. `/tmp/jed-swift-sim-sweep-20261106-2104641-awof5y/summary.json`, actor `27977eaa…`. |
| Structured diagnostics | `mise run swift-native:test --filter …` | All 61 Detail, 346 Position/Hint and 10 object-field cases pass; corrected runtime suite passes 20 probes. `/tmp/jed-swift-native-diagnostics-complete-first.log`, `/tmp/jed-swift-native-runtime-diagnostics-retry.log`. Final RETURNING-order follow-up is queued separately. |
| Refreshed mixed process | `JED_PROCESS_CORES=swift-native,rust,go,node mise run swift-native:process --no-build --configuration release` | 31 scenarios × 16 ordered pairs = **496/496 pass** on the current release actor `8265ffa8…`; canonical peer hashes remain unchanged. `/tmp/jed-swift-native-process-mixed-release-final.log`. |
| Swift client / Rust remote pages | `JED_PROCESS_CORES=swift-native JED_PROCESS_FILTER=_stream mise run swift-native:process --no-build --configuration release --pages-owner rust` | Both frozen plaintext/encrypted stream scenarios pass, 2/2. Swift release actor `8265ffa8…`, captured Rust pages actor `e909fdfd…`. `/tmp/jed-swift-native-process-rust-stream.log`. This closes the separate serving-role requirement while retaining the broader Swift-server matrix. |
| Refreshed saved simulations | `mise run swift-native:sim:sweep --no-build --configuration release --regress` | All 18 saved traces in four cores, 54 transcript comparisons and 153 independently checked Swift images, zero failures or exemptions; actor `27977eaa…`. `/tmp/jed-swift-sim-sweep-20261106-2057112-ozakk7/summary.json`, `/tmp/jed-swift-native-sim-regress-release-final.log`. |
| Relevant PostgreSQL oracle | `mise run corpus:check spec/conformance/suites/expr/short_circuit.test` and `… subquery/correlated.test` | Both regenerate byte-identically against the checkout’s declared oracle profile. `/tmp/jed-swift-native-oracle-short-circuit.log`, `/tmp/jed-swift-native-oracle-correlated.log`. |
| Repository verification | `mise run verify` | All 17 spec/code-generation checks passed. `/tmp/jed-swift-default-verify-final.log`. |
| Default repository CI | `mise run ci` | Latest run passes all 18/18 steps after the shared GiST correction and canonical Rust artifact repair, including supported-core semantics, all unit/process/simulation/browser checks, generators and formatting/lint. `/tmp/jed-swift-native-default-ci-gist-canonical-retry.log`, 137.9 seconds. Earlier formatting/cached-manifest failures remain preserved. |

The fifth-run actor is SHA-256
`02d0b80e6a83938b44641601838dda4c0342a4521175bf4a23952f03e0cae999`.
The accounting-integration actor is
`f232355dcc75f65b6dbe3a6bbf67ac66e892f30d62fff993c5c186eec0c058f4`.
Both used frozen resource digest
`5834cf5c6274f1f8bd23b191fa18ac4e2a78e19654a6a9283277c0b7368eedb6`.
The acceptance runners capture immutable executables and resource bundles;
subsequent source edits cannot alter a running comparison.

## Outstanding acceptance

- Preserve a release baseline, perform the measured optimization pass, and
  reverify affected behavior after optimization.
- Complete the benchmark report and reconcile final evidence before freezing
  the experiment. Earlier failures remain in the progress and findings records.

No implementation omission is classified as an authorized exclusion. The
frozen corpus's declared storage-mode exclusions and documented fixture-host
exceptions remain explicit in each runner's report. New dependencies, website
integration, promotion, package publication and merging to master are outside
this experiment's delivery.
