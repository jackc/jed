# Swift experiment verification

Status: **Linux acceptance and structural-performance revalidation are complete.**
Current implementation revision: `fb34b0e8cb3d82287fbd3401e3426f04b3b86c7a`.
The original snapshot and its evidence below remain preserved at
`3db8fc2d2653475abc5e9d43049254ccd1e92387`. Semantic target revision:
`0b76dd6e9c2675b552eb8af9e820065351ffa4b3`.
Platform exercised: Ubuntu Linux x86-64, Swift 6.4.2. Apple platforms are
unverified. The package and all runners are explicitly opt-in.

This report records completed gates and their limits. The [progress log](PROGRESS.md)
preserves intermediate failures, exact reproduction details and their resolutions.
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

## Fifth follow-up revalidation (2026-11-10)

The direct key read and cached access inventory recorded in
[swift-native-performance.md](../../spec/design/swift-native-performance.md#fifth-follow-up-direct-key-reads-and-cached-access-inventories-2026-11-10)
were checked with the same gates, on top of `ec1968ab`.

| Gate | Result |
|---|---|
| Release native/API and all frozen SQL | **555 tests**, zero failures; **749/749 files**, 21,597 records, 39,148 cells, 2,271 cost / 1,154 name / 100 type assertions. |
| New unit tests (`PreparedPointReadTests`) | The cached key read gives the rows, cost, error text and lifetime cost of the unprepared statement, including under statement and lifetime cost ceilings, memory budgets and restricted privileges, for integer and UUID keys; other shapes never take it. |
| New unit tests (`ForkedHandleTests`) | After a real `fork`, under exclusive and automatic locking, the child's resident read succeeds and its write and writable BEGIN fail `55006`; the parent is unaffected. |
| Concurrent stress, Swift only | Two core/storage runs, 8,000 snapshot checks, zero failures; final checksum `f3e75496a3111181`. |
| File interchange, Go peer only | **897/897 phases**, zero failures (diagnostic subset matrix). |
| Saved fault-simulation regressions, Swift only (debug) | All **18** frozen traces pass. |
| Encrypted SQL corpus, Swift only (debug) | **19,403/19,403** frozen record measurements, zero failures (diagnostic subset matrix). |

Not re-run: exact cross-core resource accounting, the cross-core encrypted
matrix, Rust and TypeScript interchange, generated fault-simulation sweeps,
mixed processes and the page service.

## Fourth follow-up revalidation (2026-11-10)

The scan, grouping and lookup changes recorded in
[swift-native-performance.md](../../spec/design/swift-native-performance.md#fourth-follow-up-scans-grouping-and-lookup-setup-2026-11-10)
were checked with the same gates, on top of `13db292f`.

| Gate | Result |
|---|---|
| Release native/API and all frozen SQL | **548 tests**, zero failures; **749/749 files**, 21,597 records, 39,148 cells, 2,271 cost / 1,154 name / 100 type assertions. |
| New unit tests (`WriterStructureTests`) | `ChildList` against a plain array; leaf-at-a-time folds give the rows of row-at-a-time folds. |
| Concurrent stress, Swift only | Two core/storage runs, 8,000 snapshot checks, zero failures; final checksum `f3e75496a3111181`. |
| File interchange, Go peer only | **897/897 phases**, zero failures (diagnostic subset matrix). |
| Saved fault-simulation regressions, Swift only (debug) | All **18** frozen traces pass. |
| Encrypted SQL corpus, Swift only (debug) | **19,403/19,403** frozen record measurements, zero failures (diagnostic subset matrix). |

Not re-run: exact cross-core resource accounting, the cross-core encrypted
matrix, Rust and TypeScript interchange, generated fault-simulation sweeps,
mixed processes and the page service.

## Third follow-up revalidation (2026-11-10)

The per-statement and bulk-write changes recorded in
[swift-native-performance.md](../../spec/design/swift-native-performance.md#third-follow-up-per-statement-and-bulk-write-costs-2026-11-10)
were checked with the same subset of gates. Committed as `13db292f`.

| Gate | Result |
|---|---|
| Release native/API and all frozen SQL | **546 tests**, zero failures; **749/749 files**, 21,597 records, 39,148 cells, 2,271 cost / 1,154 name / 100 type assertions. |
| New unit tests (`WriterStructureTests`) | `PendingKeyMap` and `RecordList` against the plain collections; batched tree edits equal unbatched ones node for node, and leave earlier snapshots untouched. |
| Concurrent stress, Swift only | Two core/storage runs, 8,000 snapshot checks, zero failures; final checksum `f3e75496a3111181`. |
| File interchange, Go peer only | **897/897 phases**, zero failures (diagnostic subset matrix). |
| Saved fault-simulation regressions, Swift only (debug) | All **18** frozen traces pass. |
| Encrypted SQL corpus, Swift only (debug) | **19,403/19,403** frozen record measurements, zero failures (diagnostic subset matrix). |

Not re-run: exact cross-core resource accounting, the cross-core encrypted
matrix, Rust and TypeScript interchange, generated fault-simulation sweeps,
mixed processes and the page service.

## Second follow-up revalidation (2026-11-09)

The copy-cost changes recorded in
[swift-native-performance.md](../../spec/design/swift-native-performance.md#second-follow-up-copy-and-dispatch-costs-2026-11-09)
were checked with a subset of the gates below. Committed as `3bea4580`.

| Gate | Result |
|---|---|
| Release native/API and all frozen SQL | **543 tests**, zero failures; **749/749 files**, 21,597 records, 39,148 cells, 2,271 cost / 1,154 name / 100 type assertions. |
| Concurrent stress, Swift only | Two core/storage runs, 8,000 snapshot checks, zero failures; final checksum `f3e75496a3111181`. |
| File interchange, Go peer only | **897/897 phases**, zero failures (diagnostic subset matrix). |

Not re-run: exact cross-core resource accounting, encrypted SQL, Rust and
TypeScript interchange, saved and generated fault simulations, mixed processes
and page-service SQL. The tables below describe `fb34b0e8`, not this source.

## Structural follow-up acceptance

The explicitly authorized follow-up changes value representation, scan and
aggregate execution, immutable dirty leaves, and prepared-query ownership.
It changes no shared fixture, logical accounting rule, file format or dependency.
The production source checkpoint is local-only `fb34b0e8cb3d82287fbd3401e3426f04b3b86c7a`.
Performance evidence and its subset limits are separate in [BENCHMARKS.md](BENCHMARKS.md).

Evidence root: `results/architecture-20261106/verification-final`. Its compact
`manifest.json` covers 2,093 report/log/accounting/source-identity files; SHA-256
`8b11c003440238e3a704f3e6c5060371e95b37807fe35e958424c70b92b05080`.
Captured executables, resource bundles and emitted images remain in the gate
directories but are outside this compact manifest. The source patch, added
files and 1,490 source/fixture/tool hashes record the precommit worktree used
for these builds. Committing it did not change production source.

| Gate | Result | Evidence relative to this root |
|---|---|---|
| Release native/API and all frozen SQL | **543 tests**, zero failures; **749/749 files**, 21,597 records, 39,148 cells, 2,271 cost / 1,154 name / 100 type assertions and five host actions. Zero omitted files, blocked adapters, aborted host actions or auto-mode exclusions. | `native-all749.log` |
| Exact resource accounting | All four cores × memory/disk/disk-spill; **241,412 measurements**, zero discrepancies. The diagnostic ceiling remains 1 TiB, not a physical heap guarantee. | `v04/report.json` |
| Encrypted SQL | Four cores, 662 applicable files and 19,403 records each; **77,612 records**, zero discrepancies. Frozen and postbaseline exclusions remain explicit. | `encrypted/report.json` |
| File interchange | All 112 ordinary frozen images plus generated cases; **2,691/2,691 phases**, zero failures. | `interchange/report.json` |
| Saved fault simulations | All 18 traces in four cores, 54 transcript comparisons and 153 independent Swift image checks; zero failures/exemptions. | `simulation-saved/summary.json` |
| Generated fault simulations | 22 profiles × eight seeds; **176 traces per core**, 459 transcript comparisons, **7,049 independent Swift image checks**, zero failures. The original 23 per-core-fault exemptions remain explicit. | `simulation-generated/summary.json` |
| Mixed processes | **496/496** pairings plus both plaintext/encrypted Swift-client/Rust-owner streams pass. | `process-driver.log`, `rust-stream-driver.log` |
| Concurrent stress | Five core/storage runs, **20,000 snapshot invariants**, 5,000 writer transactions; identical final checksum `f3e75496a3111181`. Correctness only, not timing evidence. | `stress/report.json` |
| Page-service SQL | Complete six-direction matrix, **116,628 records**, zero failures with freshly built supported adapters. | `service-fresh-peers/report.json` |
| Repository spec/code generation | All checks pass; default tasks remain independent of Swift. | `repository-verify.log` |

Commands, all through `mise run`: `swift-native:test --configuration release
--all-sql --trace-sql`; `swift-native:query-memory`, `swift-native:encrypted`,
`swift-native:interop`, and both forms of `swift-native:sim:sweep` use
`--configuration release --no-build`, with `--regress` or `--seeds 1..8` for
simulations. Process checks use `JED_PROCESS_CORES=swift-native,rust,go,node`,
then `JED_PROCESS_CORES=swift-native JED_PROCESS_FILTER=_stream` with
`--pages-owner rust`. Stress uses `--configuration release --no-swift-build`;
the successful full service refresh uses those same flags. Each report retains
its output paths and actor identities; driver logs preserve invocation details.

The first service run with `--no-build` **failed** and is retained in `service/`.
Only Go-client/Swift-owner failed: a reused Go adapter lacked the already
committed `a71c0662` GiST-loader staging correction. Disassembly, old/fresh
binary hashes and the exact record-8/25-unrun-record diagnosis are retained in
`stale-go-audit.txt`. The rebuilt-Go probe in `service-fresh-go-probe/`, followed
by the complete rebuilt-peer matrix, passed against the **unchanged Swift owner**.
No fixture or Swift fix was used to make the retry pass.

The release XCTest actor is
`3bed91b6345d597308d2101c385fd615e6fbcb8b7cbc5832a8d4c758200c30bd`,
with unchanged resource identity
`0568c671be49ea6e8e3ff902f41b48a110b612ab428fd105a8bbd67cabb5ba10`.
The separate production build completes in 183.73 seconds. Its compiler
descriptions confirm `-O` and whole-module optimization without testability,
sanitizers or library evolution. Production actors:

| Actor | SHA-256 |
|---|---|
| Benchmark | `5bc33bffebd2064a393e0f8a8acecd5c3792d4ef8affeeccb64fb2f6575d4c6d` |
| Process/page owner | `6e75981e14c5a19c7958c021828d6dc2008297180f6b2a6cb288f27b87424dc2` |
| Simulation | `abc854fe6fb1b72fb424fab0a13bab7b5a1b5715c3dbef19f51a5625aa4a4bbf` |
| Interchange | `a8dc02326367c6191328b886ee93da48f42053865edfcf42744ad88dc9e9a445` |
| Stress | `c62de8ea6b2546139db13e1a46c3811f5bbe254273ca25d89cc92cefd061a093` |

These gates refresh the affected execution, resource, storage, ownership and
process paths. Earlier CLI PTY, debug-build, oracle and default-CI results below
retain their original actors and dates; they are not claimed as reruns of this
source. Apple platforms remain unverified.

## Original postoptimization acceptance

Optimized implementation: `3db8fc2d2653475abc5e9d43049254ccd1e92387`.
The complete release benchmark comparison and its limits are recorded separately
in [BENCHMARKS.md](BENCHMARKS.md). Source labels do not replace the actor identities
below, and earlier successful actors are not credited with testing later edits.

Durable compact archive: `/home/jack/dev/jed/impl/swift-native/results/verification-postoptimization-20261106`. It contains 56 evidence/metadata files plus `manifest.json`, totaling 9,588,305 bytes. Manifest SHA-256: `2be0039648767a32276d6a5675f1ca160994e9f6bcd3cc2ca32c9f60a60d4762`.

| Optimized gate | Verified result | Retained evidence within archive |
|---|---|---|
| Complete release native/API and frozen SQL | 515 tests, zero failures. All 749 frozen SQL files completed: 21,597 records, 39,148 result cells, 2,271 cost / 1,154 name / 100 type assertions, five host actions. Zero omitted files, SQL failures, adapter blocks, failed host-action aborts, or storage-mode exclusions in auto mode. | `native/release-all749.log` |
| V04 query-memory accounting | All 12 core/mode runs passed across Swift/Rust/Go/TS and memory/disk/disk-spill. 241,412 exact record measurements; zero discrepancies. The diagnostic account ceiling is 1,099,511,627,776 bytes; this measures the shared peak schedule and does not claim a physical heap bound. Exact mode membership and authored exclusions remain in report/logs. | `v04/report.json`, `v04/swift-actor.json`, `v04/full-release.log`, all 12 core/mode logs |
| Encrypted SQL, socket-enabled retry | All four cores passed 662 applicable files and 19,403 records each: 77,612 total record measurements, 2,755 expected output records per core, zero discrepancies. Frozen disk exclusions and four explicitly excluded postbaseline comparison files remain visible. | `encrypted/socket-retry/report.json`, `swift-actor.json`, `full.log`, four core logs |
| Full interchange | All 112 frozen ordinary images plus generated interchange cases across three supported peers; all 2,691 expected phases accepted, zero failures. Uses real independent Swift/Rust/Go/TS host actors. | `interchange/report.json`, `actors.json`, `expected-phases.json`, `inventory.json`, `full.log` |
| Saved simulation regressions | All 18 saved traces passed in each of four cores; 54 transcript comparisons, 153 independently checked Swift images, 612 emitted images overall, zero failures and no per-core fault-comparison exemptions. | `simulation-saved/summary.json`, `manifest.json`, `traces.list`, per-core result/stdout/stderr logs and full driver log |

The first encrypted attempt is deliberately preserved as a failure in `encrypted/initial-failure/`. Swift completed its 19,403 records, but supported-peer Unix-socket endpoint creation was denied by the sandbox. Its full report retains 76,731 reported failure entries, including derived missing-record/output failures. Do not describe that attempt as passed or suppress its report. The unchanged retry with required local socket permission is the successful encrypted evidence above.

### Actor and source provenance

| Capture | Recorded identity |
|---|---|
| V04 release XCTest executable | SHA-256 `8a6ab03b598c31d73c14a3468cdf9ca047d1329f809af22671f889cb8f44e56e` |
| V04 / both encrypted attempts' resource bundle identity | SHA-256 `0568c671be49ea6e8e3ff902f41b48a110b612ab428fd105a8bbd67cabb5ba10` |
| Encrypted initial/retry XCTest | Same executable digest as V04; original capture manifests retained separately |
| Swift interchange actor | SHA-256 `ec9513a36dadf6faa5e11072132cb1522c957488c0d79b43443a4281fde068d2` |
| Swift saved-simulation actor | SHA-256 `b700af5d38441fb611eb6f463dfe19b94b53ad89cef0e00a8e18c7d05a420e12` |

`actor-identities.json` also retains the supported interchange and simulation command/actor identities, and the saved simulation's supported-source hashes. The original V04/encrypted manifests retain every resource-file hash. The native test log itself does not embed an executable digest; the subsequent immutable XCTest captures identify the release test binary from that build. Do not invent a native-log hash attestation it did not record, or substitute a digest from a later build.

`source-identity.json` identifies the optimization and docs-only checkpoint, the archive's observed HEAD, and the frozen contract revision `0b76dd6e9c2675b552eb8af9e820065351ffa4b3`. Actor identity and observed source identity are distinct evidence. The archive copies small reports/logs with pre-copy/post-copy SHA equality; its manifest hashes every copied/generated evidence file. No engine, fixture expectation, or tracked documentation was changed to produce this archive.


### Interactive CLI verification

The current release CLI also passed correct, wrong and empty passphrase PTY
probes: actual ECHO disabled at the live prompt, no entered phrase echoed, full
terminal settings restored and child processes reaped in all three cases. The
correct phrase produced an integrity report with no findings (six pages, five
rows; unavailable host function `hosted_geo` explicitly skipped); wrong and empty
phrases produced the expected exit-1 errors. Database copies remained unchanged.
See [the PTY report](results/cli-pty-postoptimization-20261106/verified/report.json)
and [integrity acceptance](Tools/INTEGRITY_PLAN.md). CLI SHA-256:
`887b902725495e796643f4f19cf31406f3aeea8ce1fa1fc9c212c27c59d3588f`.
The initial Ruby `echo?` false positives are preserved; the corrected probe
observes the actual ECHO bit and required no CLI source change.

### Coverage of the optimization

The five production-file changes retain packed leaf positions, remove intermediate
byte copies and use the existing fixed-cell aggregate accumulators and accounting
schedule. Full native/SQL, three-mode accounting, encrypted comparison, interchange
and saved fault regressions revalidate those affected read/decoder/accounting paths.
The earlier mixed-process 496-case matrix plus two Rust-serving streams, generated
22-profile/176-trace simulation sweep, six-direction service SQL and concurrent
stress remain preoptimization evidence. No protocol or write-policy change is part
of the optimization; those earlier results are retained with their original actors.
Independent reviews covered the storage lifetime and resource boundaries; the
integrated gates above provide execution evidence beyond those reviews.

## Preoptimization acceptance and integration history

These dated gates retain their original actor identities. The later optimized
checks above supersede their affected-path refresh requirements.

| Gate | Command | Result and evidence |
|---|---|---|
| Ordinary frozen SQL and native suite | `mise run swift-native:test --all-sql --trace-sql` | Fifth run: 749/749 files, 21,597 records, 39,148 cells, 2,271 cost / 1,154 name / 100 type assertions; 484 native tests; zero failures or adapter omissions. `/tmp/jed-swift-native-all749-all-host-fifth.log`. At that checkpoint, later accounting changes required fresh integration; the subsequent release and exact-account gates below completed it. |
| Release ordinary/native | `mise run swift-native:test --configuration release --all-sql --trace-sql` | Final preoptimization run: **509 native tests and all 749 frozen files / 21,597 records / 39,148 cells / 2,271 cost / 1,154 name / 100 type assertions pass**, zero blocked/aborted files and five host actions. `/tmp/jed-swift-native-release-all749-eof-fourth.log`. Prior first/view-second/handoff-third logs preserve the resolved six handoff/client assertions and asynchronous EOF-observation test race. This is correctness evidence, not a performance measurement. |
| Handoff admission integration | `mise run swift-native:test --filter 'PageServiceAutomaticTests\|PageServiceClientTests\|PageServiceRoleTests\|PageServiceWireTests\|PostbaselineSQLTests\|SimServiceBoundaryTests'` | 21 debug native tests pass, including the new delayed-worker/canceled-request ordering regression and memory/disk view and GiST regressions. `/tmp/jed-swift-native-handoff-admission-debug-first.log`. |
| Final diagnostic/storage follow-up | `mise run swift-native:test --sql-files … --filter …` | 42 native tests and 78 SQL records pass: 407 shared diagnostic fixtures, 10 object cases, 20 runtime probes, four RETURNING ordering cases, GIN serialization atomicity and the unchanged depth boundary. `/tmp/jed-swift-native-diagnostics-final.log`. |
| View FROM-producer integration | `mise run swift-native:test --sql-files … --filter …` | 14 native tests, five frozen view/depth files with 301 records / 231 cells / 20 names; the new shared 41-record / 23-cell regression passes in both memory and disk modes. PostgreSQL regenerates it byte-identically and all three supported actors pass. `/tmp/jed-swift-native-view-producers-retry.log`, `/tmp/jed-view-srf-gap/peer-exact-report.json`. |
| Accounting/storage integration | `mise run swift-native:test --sql-files … --filter …` | 29 complete SQL files, 1,056 records, 1,592 cells and 71 native tests pass. Exact full selection and command are retained in `/tmp/jed-swift-native-accounting-integration-first.log`; this includes pending-index serialization, Q4 compaction and attached catalog checks. |
| Second accounting integration | `mise run swift-native:test --sql-files … --filter …` | 34 complete SQL files, 1,463 records, 4,074 cells and 32 native tests pass. `/tmp/jed-swift-native-accounting-integration-second.log`. Later collation/window and planner fixes were subsequently covered by the complete release and exact-account gates. |
| Third accounting integration | `mise run swift-native:test --sql-files … --filter …` | 42 complete SQL files, 1,781 records, 4,364 cells, 268 cost assertions and 54 native tests pass. `/tmp/jed-swift-native-accounting-integration-third-retry2.log`. Captured actor `2087236c…` was an intermediate checkpoint; the later corrected complete accounting matrix is listed below. |
| Public integrity | `mise run swift-native:test --filter …` | 21 integrated native tests, 24 damaged images through 50 exact report probes, 112 ordinary files; an additional plaintext/encrypted real-socket API test passes snapshot/pin/role-loss behavior. `/tmp/jed-swift-native-integrity-second.log`, `/tmp/jed-swift-native-integrity-sockets-retry.log`. CLI also passes all 50 frozen reports plus 11 boundary probes; `/tmp/jed-swift-integrity-cli-20261106-675919-qw5gej/report.json`. |
| Forced TEMP spill | `mise run swift-native:spill --no-build --temp` | Release refresh: 28 files, 1,255 records, 571 cells, 23 cost assertions; 26 native scratch files, zero failures. `/tmp/jed-swift-native-temp-spill-full-second.log` (with `--configuration release`). |
| Exact resource accounting | `mise run swift-native:query-memory --configuration release --no-build` | **241,412 measurements, zero failures, complete four-core/three-mode matrix**: 21,477 memory records per core and 19,438 disk plus 19,438 disk-spill records per core, exact peaks and committed storage. `/tmp/jed-swift-query-memory-20261106-1996511-bxwolu/report.json`, XCTest `944718bb…`, resource digest `0568c671…`. Prior 376 GiST-load differences are resolved by the shared correction; failed and interrupted attempts remain documented in PROGRESS and BASELINE_FINDINGS. |
| Public embedding examples | `mise run swift-native:examples` | In-memory prepared binding/transaction/typed cursor; durable committed row survives close/reopen; canceled pull reports 57014 and a later query succeeds. `/tmp/jed-swift-native-examples-first.log`. |
| Full encrypted comparison | `mise run swift-native:encrypted --no-build` | Refreshed all four cores: 77,612 records, all required file/record/cost/EXPLAIN outputs present and equal, zero failures. `/tmp/jed-swift-encrypted-corpus-20261106-939870-6kf73i/report.json`, actor `b7d500a9…`. Original adapter-failure and strict recheck evidence remain preserved. |
| Full six-direction service comparison | `mise run swift-native:service --no-build` | Refreshed Swift client with Rust/Go/TS owners and each peer client with Swift owner: 116,628 records, zero failures. `/tmp/jed-swift-native-service-full-second.log`, `/tmp/jed-swift-service-corpus-20261106-1040687-2wheah/report.json`. Captured XCTest `d292c9b4…` predates the handoff correction, covered subsequently by targeted tests and the completed mixed-process refresh listed below. |
| Full file interchange | `mise run swift-native:interop --no-build` | Refreshed all 112 frozen images, all three supported peers, both writer orders and generated plaintext/key/passphrase/attachment cases: 2,691/2,691 phases, zero failures. `/tmp/jed-swift-interchange-20261106-996175-9832xa/report.json`. This excludes the separately required 24 integrity image/report fixtures. |
| Layer 3 concurrent stress | `mise run swift-native:stress` | Five complete core/storage runs; 20,000 snapshot invariants and 5,000 writer transactions; exact final checksum `f3e75496a3111181` everywhere. Swift uses 12 real threads in memory and durable-file modes with reopen, Rust threads, Go race detector, TS seeded interleaving. `/tmp/jed-swift-stress-20261106-19-vqyx3y/report.json`. Correctness evidence, not a performance comparison. |
| Simulation profile sweep | `mise run swift-native:sim:sweep --seeds 1..8 --no-build --configuration release` | Current release refresh: 22 profiles, 176 traces per core, 459 transcript comparisons, 7,049 independently checked Swift images, zero failures; all 23 frozen per-core-fault exemptions remain explicit. `/tmp/jed-swift-sim-sweep-20261106-2104641-awof5y/summary.json`, actor `27977eaa…`. |
| Structured diagnostics | `mise run swift-native:test --filter …` | All 61 Detail, 346 Position/Hint and 10 object-field cases pass; corrected runtime suite passes 20 probes. `/tmp/jed-swift-native-diagnostics-complete-first.log`, `/tmp/jed-swift-native-runtime-diagnostics-retry.log`. The four-case RETURNING-order follow-up passed in the final diagnostic gate listed above. |
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

## Coverage and limits

All required Linux behavior in the recorded scope and host/tool matrices is
implemented and verified. The original `3db8fc2` snapshot's complete 143-workload,
four-core, three-round release baseline and after run passed their logical
comparisons. The structural follow-up uses separate diagnostic performance
subsets; [BENCHMARKS.md](BENCHMARKS.md) records their membership, measurements
and limits without extending them into a new whole-suite or parity claim.
This is a finite experimental acceptance record, not exhaustive fault coverage,
cryptographic certification, or a whole-engine physical memory guarantee.

Apple platforms remain unbuilt and untested. The snapshot is opt-in and frozen;
normal CI, tests, benchmarks and website artifacts do not depend on Swift.
Promotion and native-versus-wrapped production delivery require separate decisions.
All experiment commits are local-only on `codex/swift-native-experiment`; no
master merge, push or package publication is part of this delivery.

No implementation omission is classified as an authorized exclusion. The
frozen corpus's declared storage-mode exclusions and documented fixture-host
exceptions remain explicit in each runner's report. New dependencies, website
integration, promotion, package publication and merging to master are outside
this experiment's delivery.
