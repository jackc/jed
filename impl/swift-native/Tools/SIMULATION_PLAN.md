# Native simulation integration

Durable preoptimization evidence is preserved in the ignored ext4
[verification archive](../results/verification-preoptimization-20261106/manifest.json):
50 successful report/log/TSV files, 11,386,296 bytes, with original paths and
SHA-256 hashes in its manifest. Originals remain intact; binaries and simulation
images are excluded. The complete correctness matrix passes; only performance
baseline/optimization/reporting remains outstanding.

The acceptance contract is `spec/design/simulation-testing.md` at frozen baseline
`362e0798ac3a8bd04bd2da556044b6ec73bf468a`: 18 regression traces and 22 profiles.
The later shared empty-unary-subtree correction is a separately labelled regression,
as with the postbaseline KMV corpus case; it does not change the frozen inputs.

Simulation is an opt-in tooling product. It must execute the same Swift SQL,
StorageDomain, Pager, validated-COW serializer, publication, page-service owner and
client implementation as native hosts. Simulated-only commit or recovery logic is
not allowed. A missing operation produces a failed result, never a successful skip.
Only the trace format's specified references to vanished sessions are reported as
skipped operations.

## Current acceptance (2026-11-06)

The refreshed saved-regression comparison passes all 18 traces in every core,
54 transcript comparisons and 153 independently checked Swift images. The
refreshed generated sweep passes all 22 profiles × seeds 1–8: 176 traces per
core, 459 transcript comparisons and 7,049 independently checked Swift images,
with zero failures. Its 23 shared per-core-fault comparison exemptions remain
explicit; this is a finite sweep, not an exhaustive fault-space proof.

The generated report is
`/tmp/jed-swift-sim-sweep-20261106-980401-9e3v3g/summary.json`, with Swift actor
`40c80ea64c8eb37d78cea74192d60d59c8ea95b73645b66c399c349462894709`.
The newest saved release regression summary is `/tmp/jed-swift-sim-sweep-20261106-2057112-ozakk7/summary.json`
(18 per core, 54 comparisons, 153 checked Swift images, zero failures). The prior
log `/tmp/jed-swift-native-sim-regress-final.log` remains retained.
The refreshed native process matrix also passes all 496 ordered pairs.
[SIMULATION_COMPARISON.md](SIMULATION_COMPARISON.md) retains prior failures and
later corrections. The separate service SQL refresh passes all six directions
and 116,628 frozen records, with zero failures:
`/tmp/jed-swift-service-corpus-20261106-1040687-2wheah/report.json`.
After the shared GiST loader correction, full release V04 passes all 241,412
measurements with zero failures and complete membership:
`/tmp/jed-swift-query-memory-20261106-1996511-bxwolu/report.json`. Earlier 376-difference reports
and the Rust cache incident remain preserved in RESOURCE_ACCEPTANCE. The fourth release refresh passes
all 509 native tests and all 749 SQL files in
`/tmp/jed-swift-native-release-all749-eof-fourth.log`. Prior handoff/client and old-pin
assertions are resolved. Both Swift-client/Rust-pages stream scenarios pass in
`/tmp/jed-swift-native-process-rust-stream.log`. Canonical default CI retry passes
18/18 in `/tmp/jed-swift-native-default-ci-gist-canonical-retry.log`.
The fresh full release mixed-process run passes all 496 pairings, and saved
release simulations pass as above. The final generated release sweep passes
all 176 traces per core, 459 comparisons and 7,049 independently checked Swift
images at `/tmp/jed-swift-sim-sweep-20261106-2104641-awof5y/summary.json`. There are 28,196
emitted images, zero failures and 23 explicit canonical fault exemptions.
No performance measurements have been made.

## Historical foundation (2026-11-05)

The first model/parser and shared host-publication slice is implemented. Its
seven tests cover the shared decision vectors, process-visible unissued writes,
growth stabilization, content-keyed tears, directory barriers, read faults,
trace parsing, and actual create/compact/backup/reopen paths. The combined native
file, maintenance, backup, serialization and page-service gate passed **44/44**:
`mise run swift-native:test --filter 'SimulationBoundaryTests|FileHostBoundaryTests|MaintenanceBoundaryTests|AttachmentMaintenanceTests|BackupBoundaryTests|SerializationBoundaryTests|PageServiceAutomaticTests|PageServiceClientTests|PageServiceMaintenanceTests'`
(`/tmp/jed-swift-native-simulation-foundation.log`). The adapter now injects
`FileOperations` into the existing `FileDatabaseHost`; it retains the common
publication policy and supplies a `FileBlockStore`. No separate simulated
commit, recovery or maintenance implementation was introduced.

The runner, simulated page service and transcript/history oracles are connected.
All 18 frozen regressions passed deterministic double replay after this
foundation. At that checkpoint the 22-profile sweep and cross-core comparison
were still outstanding; the current acceptance above supersedes that status.

## Files and seams

- `SimDecision` implements the shared splitmix decision function and file-name
  offset salt. Its lanes are fixed by the spec, and every shared PRNG vector is a
  byte assertion. SQL entropy and key entropy have independent prescribed lanes.
- `SimFilesystem` owns kernel and durable directories, ordered pending directory
  operations, the fault controller, process identities, and per-file byte devices.
  `SimDisk` implements BlockStore with cache/durable bytes and per-offset pending
  versions. The model includes destructive shrink, growth fsync, content-keyed
  process issue decisions, sector tears, fsyncgate, archive/replay and repair.
- A narrow common file-publication layer shares existing create,
  replacement and backup sequencing from the POSIX adapter. Native operations
  still own permissions, descriptors and crash-clean lock bundles. Both native
  and simulated adapters supply file existence, temporary creation, close/open,
  atomic rename/remove and directory barriers; the engine owns ordering, image
  construction, poisoning and generation adoption.
- Database scratch discovery gains a host-supplied scratch provider. Simulated
  spill files implement the existing ScratchStore seam and its fault operations;
  no simulation touches the host's filesystem for engine data.
- The page-service owner receives an ordinary framed listener/transport seam,
  matching the client's existing ServiceFrameTransport abstraction. Simulated
  endpoints settle each trace step before the next one; no background wall-clock
  activity may change simulated outcomes. The protocol codec, writer leases,
  deltas, acknowledgements, history, pins, reclamation and client pager remain
  the production implementation. Simulated crash-clean locks/election live at
  the same role boundary as native protocol-v2 locks.
- `SimTrace` parses only the language-neutral operation stream. `SimRunner`
  drives public sessions plus narrow tooling hooks, emits canonical transcripts
  and checks generation histories/durability floors. `SimIntegrity` walks packed
  tree metadata/rows for image and live invariants; it does not ask the same
  reachability answer twice and call that independent verification.
- A small `jed-sim` executable calls an ExperimentalTools SPI entry point. Input
  and output use the shared JED_SIM_IN/JED_SIM_OUT/JED_SIM_RESULTS/JED_SIM_IMAGES
  protocol. `swift-native:sim:{regress,sweep,replay}` are opt-in mise tasks; default
  sim/core lists and normal CI remain unchanged.

## Implementation and verification sequence

1. Publish fault-model/parser foundations with shared decision-vector and model
   invariants. Freeze all baseline profiles/regressions as mechanical data.
2. Extract host publication operations in a separately reviewed change. Re-run
   existing file, maintenance, backup, serialization and automatic service tests
   before connecting simulated files. Native 31-scenario process acceptance must
   remain green.
3. Run single-process traces through the actual database and simulated disk;
   implement all history, floor, live/image, growth and truncation assertions.
4. Connect simulated scratch, replacement, backups, attachments and crypto,
   including stale held keys, restore and pointer-replay checks.
5. Connect simulated service processes, role locks and transport. Keep client
   SQL native and fault-free owner foreground admission resident.
6. Run every frozen regression and all profile sweeps; compare transcripts and
   images with supported cores according to the shared per-core-fault rules.
   Preserve every failed trace and exact result; report counts and immutable
   executable/input identities. The independent Ruby image checker remains a
   separate verification voice.

## Resource and determinism constraints

The simulator may retain its bounded test history and byte images; this does not
authorize a full-residency assumption in the database. A shared fault can be
armed for one step only, oracle reads are exempt from read faults, and every
faulted step ends the simulated process even if the chosen barrier was not
reached. Directory sync without pending operations is not a fault point. Growth
setSize is a full durable allocation but never a fault barrier. Failed process
writes remain readable until death, then only never-issued versions disappear.
All map walks that affect bytes, transcripts or decisions use explicit order.

The role/transport refactor must preserve the already verified zero-coordination
owner foreground path, reader watermark admission, and post-ack maintenance
recheck. The time-based flush deadline must use the injected simulation clock;
no wall time or OS entropy may enter a simulated result.

## Historical runner and deadline verification

The single-process runner and all 18 frozen regressions are now wired through
`swift-native:sim:regress`. The first diagnostic attempt completed three traces
before a long concatenation exposed repeated operand resolution; that fourth
trace was interrupted, and the remaining 14 were not attempted. Its result file
is `/tmp/jed-swift-sim-results-20261105-19-do7jdb/results.jsonl`, actor SHA-256
`5b268fad0396977ee38c7aec8342ade170eb76f1dbc5d940813e857dcb4829cb`.
No full regression acceptance is claimed from that attempt.

The real pager deadline policy now uses the sanctioned clock, once after Q4
prechecks and before planning. Service begin/application timing is distinct,
and the native idle waiter takes the nonwaiting writer gate and commit boundary.
Simulation disables only that waiter, retaining the same foreground policy.
Eight host tests cover clock counts, file/memory/read-only scope, deadlines, Q4,
service timing, clock/barrier failures, and idle close. All eight passed in the
27-test focused integration gate (`/tmp/jed-swift-native-deadline-concurrency-green.log`).

The next full regression attempt completed every one of the 18 frozen files:
**8 passed, 9 failed explicitly at service setup, and 1 failed the live integrity
check**. Actor SHA-256:
`fb88b3a47af72578f2cb40f465795943c177260640f20e2dff51f9352a5d1f3d`.
Frozen input fingerprint:
`369ad9def9a39d16e8812714f9100f1ad86df6822910021c32cfa19e4c2e7bfc`.
Exact results, progress, images and transcripts are retained under
`/tmp/jed-swift-sim-results-20261106-19-eto6n2`; console log:
`/tmp/jed-swift-native-sim-resolver-fixed.log`.

Passing traces: `analyze-then-drop-table`, `compaction-crash-before-dir-sync`,
`delete-merge-separator-overflow`, `encryption-rekey-rename-not-durable`,
`relaxed-attachments-close-order`, `relaxed-close-truncation-floor`,
`relaxed-deadline-flush`, `relaxed-floor-unsynced-compaction-rename`.
The 403-step encrypted trace now completes after the resolver correction.

The nine service failures at that checkpoint were all the frozen `service-*` and
`encryption-service-*` files; each reported that the simulated page transport
was unimplemented. `stale-root-leaf-chains` failed at step 3/source line 13:
`simulation integrity: committed leaf retains pending overflow bytes`.
The native root leaf intentionally keeps a sealed-payload cache beside its
current persisted chain references. The checker was then corrected to validate
those cached bytes against the actual chains instead of rejecting every cache;
the later complete runs verify that correction.

At that stage, service simulation, the 22-profile sweep and supported-core
comparison were pending. Unsupported service files were counted as failures,
not exclusions; the later implementation and complete passes close them.

The S8 host/runner implementation was then published for verification. A narrow
`ServiceHostPlatform` supplies ownership devices, owner records, endpoints,
byte streams, polling time and background-worker policy. Native hosts retain
normalization/hard-link checks, crash-clean bundle locks, native dispatch and
ordinary protocol readers. Simulated hosts use the same election, connection,
commit, recovery, maintenance and backup algorithms. The network waits for EOF
cleanup and tracked writer/yield/failure workers, not merely empty byte buffers.
Explicit disconnect targets main's connection, leaving served attachment
connections intact.

The runner now tracks owner loss and each domain's durable floor, admits only
produced history on promotion, checks client free-list subsets, and records
post-ack maintenance as same-content generations. Faulted steps retain their
entry owner set so role revocation cannot omit an owner from process death.
Held-key reopen and client backup/scratch use the same host policy seams.
Independent reviews of the foundation and runner found no outstanding blocker;
the first compiled/runtime gate and all-18 rerun were still pending at that
checkpoint. The historical 8/18 result above remains unchanged; the later results
below and current acceptance establish the subsequent implementation.

The post-extraction verification requirements included automatic election
and handoff, PageServiceOwner/Client/Connection, socket and process transport,
file creation/replacement/backup, and deadline tests. The full 31-process native
suite and mixed-core matrix were required to run against a newly built immutable
actor. Their completed refresh is recorded in the current acceptance above.


## Complete frozen regression replay (2026-11-06)

The focused integration gate passed **28/28**, including the S8 host foundation,
shared disconnect runner replay, corrected independent live checker, and tree
capacity boundaries. The next full frozen run completed **18 PASS / 0 FAIL /
0 skipped**. Every trace ran twice; transcripts and filesystem-operation hashes
matched. The eight previously passing traces retained their exact hashes.

- Actor SHA-256: `e5494608f618785d74297a9318875e77c8a3dcf19c251eae65369cd18b8692d0`.
- Frozen fingerprint: `369ad9def9a39d16e8812714f9100f1ad86df6822910021c32cfa19e4c2e7bfc`.
- Results/transcripts/images/progress: `/tmp/jed-swift-sim-results-20261106-3772157-pnbmp6`.
- Console log: `/tmp/jed-swift-native-sim-service-integrated.log`.

This closes the nine previously explicit service failures and the stale-root
checker failure. It establishes the 18 frozen traces under the Swift runner;
it does not establish the 22-profile sweep, cross-core transcript/image equality,
or the native process matrix after the service host extraction.

The next native host gate passed 92/92 and the native process suite passed 31/31
with actor `ffe818a97e17f7f159ac6bc7ebdb5641dae768d540923949e3ef598ca0752e56`.
The first four-core simulation comparison then returned 18 local-oracle passes
per core, but **180 transcript/image disagreements across seven traces**. All 153
Swift crash/backup images independently passed the byte-format checker. This is
an incomplete differential result; diagnoses and retained evidence are recorded
in [SIMULATION_COMPARISON.md](SIMULATION_COMPARISON.md). The generated sweep
and mixed process matrix were still pending then; both refreshed gates now pass
as recorded in the current acceptance section.
