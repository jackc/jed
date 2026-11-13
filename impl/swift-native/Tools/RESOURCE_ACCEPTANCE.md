# Swift resource acceptance audit

The authorized 2026-11-12 refresh targets `b1a6affca7725e0b07a4f734eddb1c569ef76bcf`
(v52, 763 SQL files, 116 database fixtures, 19 saved simulations). See
[current verification](../VERIFICATION.md) for its results and actor provenance.
The investigation and measurements below retain their original baseline.

Durable preoptimization evidence is preserved in the ignored ext4
[verification archive](../results/verification-preoptimization-20261106/manifest.json):
50 successful report/log/TSV files, 11,386,296 bytes, with original paths and
SHA-256 hashes in its manifest. Originals remain intact; binaries and simulation
images are excluded. This archive remains preoptimization evidence. Final
actor-specific verification and the measured comparison are recorded separately
in [VERIFICATION.md](../VERIFICATION.md) and [BENCHMARKS.md](../BENCHMARKS.md).

This audit uses **R01–R07 in `swift-native-host-matrix.md`**, not the differently
numbered scope ledger. Baseline: `0b76dd6e9c2675b552eb8af9e820065351ffa4b3`.
Original optimized-source verification for `3db8fc2d2653475abc5e9d43049254ccd1e92387`
passed 515 native tests including all 749 frozen SQL files. Its complete V04 refresh
passed all 241,412 measurements with zero failures: all four cores in memory,
disk and disk-spill, with exact peak/storage comparisons and complete membership.
[VERIFICATION.md](../VERIFICATION.md) maps these gates to their captured actors;
[BENCHMARKS.md](../BENCHMARKS.md) records measured performance separately.

The structural follow-up at `fb34b0e8cb3d82287fbd3401e3426f04b3b86c7a` passes
543 native tests, all 749 frozen SQL files and a fresh complete 241,412-measurement
V04 comparison. The [structural acceptance record](../VERIFICATION.md#structural-follow-up-acceptance)
identifies its actors and additional resource/storage/process gates. Earlier
evidence below retains its original scope.

## Preserved preoptimization evidence (2026-11-06)

The sixth frozen SQL run passes all 749 files and 21,597 records;
the earlier release suite passed all 505 native tests including those files.
The view FROM-producer correction subsequently passes its 41-record/23-cell
regression in memory and disk and host capability checks. The latest release
refresh passes all 509 native tests and all 749 SQL files
(`/tmp/jed-swift-native-release-all749-eof-fourth.log`). The prior handoff/client
and old-pin assertion failures remain preserved in their original logs; all
are resolved in the fourth release refresh. Performance was unmeasured at this
checkpoint. The refreshed service corpus passes all six directions
and 116,628 frozen records with zero failures:
`/tmp/jed-swift-service-corpus-20261106-1040687-2wheah/report.json`.

The corrected full release V04 run passes **241,412 measurements, zero failures**,
with `complete_matrix=true` and all 12 processes passing: 21,477 memory records
and 19,438 records in each of disk and disk-spill, for every core. Ordinary SQL
assertions, exact peaks and committed-storage bytes agree without adding any
comparator exception. **Exact-account acceptance is complete.**

- Report: `/tmp/jed-swift-query-memory-20261106-1996511-bxwolu/report.json`.
- Log: `/tmp/jed-swift-native-query-memory-release-gist-third.log`.
- XCTest SHA-256: `944718bbc37e4af7e56f13d13981611503dab04f1af454caaf9f0270fbdea4da`.
- Resource SHA-256: `0568c671be49ea6e8e3ff902f41b48a110b612ab428fd105a8bbd67cabb5ba10`.

The earlier full debug and release reports each retain **376 peak differences**
(188 records in each disk mode, 22 files). Independent review traced them to
supported-core GiST reconstruction staging after committed load, contrary to
memory.md §7; Swift correctly started at zero. The three loader corrections,
six deserialize/attachment host cases and shared 33-record reopen regression
are integrated. The Swift regression passes 33 records/10 cells in both memory
and disk; its original focused process separately had four host failures, now
resolved. The reviewed shared fixture remains byte-identical to its Swift copy.

The intervening canonical Rust build-cache contamination caused zero collected
corpus files; strict V04 membership rejected that attempt. Isolated probe targets,
a canonical rebuild and the fail-closed empty-corpus guard precede the passing
run. Default CI passes its canonical 18/18 retry. Preserve the failed evidence:

- Earlier debug: `/tmp/jed-swift-query-memory-20261106-939537-ml7e92/report.json`.
- Earlier release: `/tmp/jed-swift-query-memory-20261106-1135756-exgp9z/report.json`.
- Cache incident: `/tmp/jed-swift-query-memory-20261106-1610643-e6008r`.

Earlier prefix differences, stack aborts and socket-refused runs remain preserved
as history in `../PROGRESS.md`; they are not the current result.

| Row | Existing implementation and direct evidence | Acceptance evidence / scope boundary |
|---|---|---|
| R01 cost | `CostMeter` captures limits, retains first crossed ceiling under saturation, shares lifetime and statement resources. `ResourceTests` cover equality, saturation, precedence and internal meters. `DatabaseBoundaryTests` cover frozen cursor cost settings and later lifetime admission. All frozen cost/resource SQL remains admitted. | All current V04 ordinary SQL mode assertions pass. All native assertions and the corrected exact-account matrix pass. H07 charge-at-open/checkpoint-on-pull behavior is resolved by actual Rust/Go/TS probes and the separately recorded narrow streaming prose correction; CursorAdmission tests pass. |
| R02 structural limits | UTF-8 length and scalar positions, parameter/index limits, parser/expression/view expansion, regex/JSON/composite bounds are implemented. Parser/JSON/value/catalog tests cover byte and internal boundaries; unchanged depth-limit SQL plus 255-deep function and deep-view tests exposed and verified stack-frame/traversal repairs. | The complete release run and focused view-source gate pass unchanged depth limits. The fourth release refresh also passes all 509 native tests; no additional missing limit mechanism was found. |
| R03 scalar | `StatementResources` has a finite 64 MiB default and cumulative scalar admission; scratch meters share it, new statements own fresh accounts, nonpositive setters restore the default. Text/array/regex/JSON/decimal boundaries test allocation-before-admission ordering and cost precedence. | `FrozenResourceBoundaryTests` passes the unexpressible host setter/interleaved-cursor check. Full scalar corpus remains part of each applicable mode. |
| R04 Q1/Q2/Q3 | Live account/floor/underflow instrumentation is present. Operators, collectors, spill structures and pending ledgers have native boundary tests. Q3 ALTER rewrite and rebuilt-index cumulative staging was corrected from generated resource-5 evidence; the refreshed generated simulation sweep passes all 176 traces per core. | Corrected full release matrix passes all 241,412 exact peak/storage measurements with zero failures and complete membership. The historical 376 GiST-load differences are resolved by the shared loader correction. |
| R05 cache/spill | `TreeBoundaryTests` check minimum one-leaf residency, resize/invalidation and cache-independent logical visits. Real scratch, ordered spill replay, hash/state accounting, fault/early-close cleanup and bounded file counts are exercised by `ScratchBoundaryTests`, `SpillBoundaryTests`, `SpillHashBoundaryTests`, `ExternalSortBoundaryTests`, `BlockingJoinBoundaryTests`, and `BlockingOperatorBoundaryTests`. | Full work_mem=256 disk-spill comparison passes all 19,438 records per core with ordinary assertions and exact peak/storage agreement. The full comparison complements the operator lifetime/fault tests. |
| R06 TEMP | `TemporaryDomain` separates scratch residency from retained storage, applies the private stage hook, and keeps cursor roots after session close. Native scratch tests cover real unlinked/sealed files and migration. Temp lifecycle/budget/ON COMMIT SQL files remain admitted; session accessor tests cover runtime zero semantics. | The reviewed `swift-native:spill --temp` adapter now supplies the shared `JED_CONFORMANCE_TEMP_BUFFERS=1` mode with a native scratch host for in-memory main, canonical source selection and a nonzero actual TEMP-file assertion. The release forced-TEMP refresh passes 28 files, 1,255 records, 571 cells, 23 cost assertions and 26 real scratch files. Dependent changes still require their own verification. |
| R07 storage | Exact committed/live storage accounting, database/domain setters, grouped preflight, no-write failure and reader-aware reclamation have direct `StorageDomainBoundaryTests`, `DomainCommitGroupBoundaryTests`, `BatchFlushBoundaryTests`, process storage-limit and simulation coverage. | Exact committed-storage and query-peak bytes agree throughout the corrected complete matrix. The canonical fixture-storage exception remains explicit because Go opens fixtures as files while Rust/TS/Swift deserialize. |

## V04 runner boundary

The resource runner captures the Swift XCTest executable and both resource bundles
once under the evidence directory before running any mode. `swift-actor.json` and
the final report retain the executable hash, resource hashes, and direct test
selector; later builds cannot change the captured actor. `--no-build` skips only
the initial build, then performs the same capture and fixture validation.

`mise run swift-native:query-memory` defaults to a huge active 1 TiB budget,
all frozen sequential SQL files, all four cores, and memory/disk/disk-spill.
The task verifies each frozen corpus hash and equality with the live shared copy
used by supported runners. It does not rewrite either corpus. Supported-core
files added after the frozen baseline are individually reported as outside this
comparison; they cannot hide a missing frozen record. Live shared sources and
corpus remain unchanged during a comparison; a new regression waits until
all running peer matrices have finished.

Each TSV row is `file<TAB>ordinal<TAB>peak<TAB>main-storage-bytes`. Ordinals
include both statements and queries, including expected errors. The comparison
rejects missing, duplicate, unexpected and malformed records before checking
values. Only canonical `#skip` storage exclusions and the fixture storage
exception apply. Concurrency schedules are not sequential record measurements;
their existing separate seven-file schedule gate remains required.

Every record resets the peak after configuring host settings, drains/closes its
ordinary cursor, and fails if the global underflow counter changed. Per-record
directives override run defaults, including an explicit zero. The task retains
logs, progress files, TSVs and `report.json`; `--filter`, `--mode` and `--cores`
produce explicitly labelled diagnostic subsets. `--check-adapter` exercises
strict comparison/record parsing without building or running any core.

The default repository task/core lists are unchanged. Build/test execution is
coordinated by the parent agent. Bounded adapter checks and the accounting tests
pass. The initial historical diagnostic ran `resource/query_memory_state.test` in memory mode
on all four cores: 42 measurements per core, 168 total, zero missing keys or
peak/storage differences. Evidence is retained at
`/tmp/jed-swift-query-memory-20261106-19-ugjlso/report.json`. This is a diagnostic
subset, not complete V04 acceptance.

## Forced spill modes

`mise run swift-native:spill` runs the complete applicable frozen disk corpus
with `work_mem=256`. `mise run swift-native:spill --temp` selects the shared
CREATE TEMP/TEMPORARY lane, keeps main in memory, and supplies a real native
scratch host with `temp_buffers=1`. The adapter counts successfully created TEMP
scratch files and fails if the forced mode never spills. Per-record settings
still override defaults. `--filter` is explicitly labelled a diagnostic subset;
`--no-build` reuses the existing executable. Both tasks preserve original SQL,
values, errors and cost expectations. They do not change default repository
verification or claim encrypted spill coverage from an unsealed scratch run.

## Interchange inventory (V10)

Existing evidence includes all shared key/value vectors, tree page rebuilding,
encrypted golden pages, page-service frames/malformed inputs, all ordinary
golden image opens/commits, canonical serialize/deserialize and backup tests,
mixed-core process histories, and cross-core simulation image comparisons.
These existing checks are complemented by `swift-native:interop`, which inventories
all 112 frozen images and attempts Swift↔each supported core in both writer
directions, including reopened mutation and byte/checker reports. Plaintext,
raw-key/passphrase encryption, attachments, recovery, corruption and host-function
environments are explicit. Failed actors and missing phases fail the task.
See [INTERCHANGE_PLAN.md](INTERCHANGE_PLAN.md) for its 2,691-phase matrix and current
verification status. The preoptimization full 112-image, 2,691-phase interchange
matrix passed with zero failures at
`/tmp/jed-swift-interchange-20261106-996175-9832xa/report.json`; the postoptimization
refresh also passes all 2,691 phases at
[its retained report](../results/interchange-postoptimization-20261106/report.json).
The separate 24 integrity images pass 50 exact API/CLI reports and 11 CLI boundary
probes; VERIFICATION.md retains their actual actors and later checks.

## Q4 forced compaction after rejected admission

The first complete memory-prefix comparison found equal query-memory peaks but
unequal committed-storage gauges in `resource/storage_memory.test`: from record
27 onward Swift reported 212,992 bytes, while Rust, Go and TypeScript reported
188,416. Record 26 rejects `54P06`; record 27 clears the limit and retries the
same insertion. This was an engine allocator-state defect, not a comparison or
corpus expectation defect.

`spec/design/memory.md` §8.3 explicitly says a forced compaction remains in effect
when the subsequent admission rejects. The corresponding frozen implementations
are Go `shared.go:953` (`forceCompact`), Rust `shared.rs:821` (`force_compact`),
and TypeScript `persist.ts:214` (`forceCompact`): all assign the enlarged free
vector, `liveAtCompaction` and free-generation before re-planning or returning
`54P06`. Swift had incorrectly retained that reachability proof only inside the
prospective plan, so clearing the limit skipped forced compaction and appended
pages instead of using the already proven free slots.

The correction retains only these three allocator metadata fields under the
existing committed-generation check and reader-watermark gate. It writes no
bytes, changes no selected high-water, publishes no root and retains every live
page. The same rule applies when a later domain rejects group preflight. Two
old invariant assertions incorrectly requiring an unchanged free vector are
corrected while preserving their no-write/root/high-water assertions. A new
native boundary test checks exact reclaimed pages `[2,3,4]` after refusal and
that an unlimited retry consumes them before growing. Independent parent review
approved the narrow change. The subsequent 71-native-test focused gate passed
these invariants. The subsequent complete active-account comparisons agree on
committed-storage bytes; the original 446-difference prefix report remains
historical evidence.

## Oversized pending index entries and Q3 error timing

The active-account comparison of frozen `dml/index_spilled_key.test` records 15
and 16 found Swift query-memory peaks 13,692 and 0, versus Rust/Go/TypeScript
22,672 and 13,576. Both statements already returned the expected `0A000`; the
difference was the phase of rejection. They insert or update a doubled spilled
text value whose full secondary-index key cannot fit in a page.

Frozen baseline `0b76dd6e9c2675b552eb8af9e820065351ffa4b3` explicitly retains an
oversized single-record leaf or a leaf without an admissible two-way split:
Go `pmap.go:1206` (`buildLeaf`), Rust `pmap.rs:593`/`:633`
(`leaf_split_point`/`build_leaf`), and TypeScript `pmap.ts:1049` (`buildLeaf`).
The comments in all three say the oversize surfaces as `0A000` at serialization.
Their root promotion and one-separator interior also retain the pending shape;
the physical page encoder supplies the final capacity check. Thus pending table
and index versions participate in Q3 before commit serialization rejects them.
The shared format's requirement to reject before storage work is unchanged.

Swift's early `BPlusTree.insert` / `buildLeaf` checks instead threw before
`TableStore.put` staged the secondary-index version. The correction keeps dirty
nodes in packed bytes, with a private pending-leaf constructor allowed a larger
temporary capacity. The ordinary on-disk constructor still requires the exact
codec page size, and the physical page encoder still enforces physical capacity.
Commit layout and tree serialization reject unfit nodes with `0A000` before any
storage write. No decoded table map, new split shape, empty sibling, or oversized
physical page is introduced. Type, directory, ordering and checksum validation
remain active. Native tests cover Q3 retention, immutable predecessor roots,
strict physical decoding/encoding, no-write preflight and refusal before Q4.

An already-invalid pending tree whose interior has no admissible split is
retained safely until the same preflight refusal; supported implementations
consider that defensive case unreachable and panic/throw an internal invariant
error. Swift does not introduce a memory-unsafe or process-fatal path to copy that
assumption. Valid-node split selection and delete merge-abandon rules are unchanged.

Independent review and focused runtime verification were recorded separately.
The complete debug and release comparisons now pass these original oversized-key
peak cases; the later corrected full release matrix also resolves their separate
committed GiST reconstruction differences.

The subsequent focused integration gate passed 71 native tests and 29 shared SQL
files (1,056 records), including `dml/index_spilled_key.test`, Tree,
RecordDisposition, StorageDomain and DomainCommitGroup boundaries. This verifies
the new Q4 and pending-page-fit invariants and preserves the SQL error results.
The later complete debug and release matrices verify those corrected exact
peak/storage cases. The historical 376 GiST-load peak differences are resolved
in the corrected full release matrix described at the top of this audit.
