# Native simulation integration

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

## Verified foundation (2026-11-05)

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

The full runner, transcript/history oracles, simulated process service and all
18 regression/22 profile acceptance remain outstanding. This foundation gate
does not establish full simulation acceptance.

## Files and seams

- `SimDecision` implements the shared splitmix decision function and file-name
  offset salt. Its lanes are fixed by the spec, and every shared PRNG vector is a
  byte assertion. SQL entropy and key entropy have independent prescribed lanes.
- `SimFilesystem` owns kernel and durable directories, ordered pending directory
  operations, the fault controller, process identities, and per-file byte devices.
  `SimDisk` implements BlockStore with cache/durable bytes and per-offset pending
  versions. The model includes destructive shrink, growth fsync, content-keyed
  process issue decisions, sector tears, fsyncgate, archive/replay and repair.
- A narrow common file-publication layer will extract existing create,
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

## Publication and verification order

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
