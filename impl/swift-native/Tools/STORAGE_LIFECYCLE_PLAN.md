# Database storage and state seam

Durable preoptimization evidence is preserved in the ignored ext4
[verification archive](../results/verification-preoptimization-20261106/manifest.json):
50 successful report/log/TSV files, 11,386,296 bytes, with original paths and
SHA-256 hashes in its manifest. Originals remain intact; binaries and simulation
images are excluded. This archive remains preoptimization evidence. Final
actor-specific verification and the measured comparison are recorded separately
in [VERIFICATION.md](../VERIFICATION.md) and [BENCHMARKS.md](../BENCHMARKS.md).

This records the implemented storage architecture at
`cec8170bfc33f087fa98949ae661b1eaa5490218` in the frozen Swift experiment, not a new
shared storage design. The contracts are `transactions.md` §§2–3, 6–8,
11; `pager.md` §§2–5; `validated-cow.md` §§1–4; `memory.md` §§7–8;
`large-values.md` §15; `gist.md` §4.1; and the v50 plain/encrypted formats.
The dated foundation checkpoints below are history, not current omissions.

Final optimized-source checks pass 515 native tests including all 749 frozen SQL
files, 241,412 exact-account measurements, 77,612 encrypted records and all
2,691 interchange phases over 112 fixtures plus generated histories. The saved
simulation refresh passes all 18 traces per core, 54 transcript comparisons and
153 independently checked Swift images. VERIFICATION.md maps their actual actors;
BENCHMARKS.md records the measured optimization separately. The packed aggregate
and slice-decoder changes preserve file format and storage policy.

## Preserved preoptimization evidence (2026-11-06)

Refreshed interchange passes all 112 frozen images
and 2,691 phases; the process matrix passes 496 ordered pairs; saved simulations
pass 18 traces/54 comparisons/153 checked Swift images; generated simulations
pass 176 traces per core/459 comparisons/7,049 checked Swift images. Reports:
`/tmp/jed-swift-interchange-20261106-996175-9832xa/report.json` and
`/tmp/jed-swift-sim-sweep-20261106-980401-9e3v3g/summary.json`.

The corrected full release V04 matrix passes all 241,412 measurements with zero
failures and complete membership (21,477 memory and 19,438 disk/disk-spill
records per core). Peaks and storage bytes agree:
`/tmp/jed-swift-query-memory-20261106-1996511-bxwolu/report.json`. Earlier debug/release reports
retain 376 GiST-load differences; the shared loader correction, six host cases
and reopen regression resolve them without altering the comparator. The failed
private Rust manifest/cache attempt also remains preserved in RESOURCE_ACCEPTANCE.
Service SQL passes all six directions and 116,628 frozen records:
`/tmp/jed-swift-service-corpus-20261106-1040687-2wheah/report.json`.
The fourth release refresh passes all 509 native tests and all 749 SQL files
in `/tmp/jed-swift-native-release-all749-eof-fourth.log`. Prior handoff/client and
old-pin assertion failures are resolved and retained as historical evidence.
Both Swift-client/Rust-pages stream scenarios pass in
`/tmp/jed-swift-native-process-rust-stream.log`; canonical default CI retry passes
18/18 in `/tmp/jed-swift-native-default-ci-gist-canonical-retry.log`. Fresh full
release mixed process passes 496/496; saved simulations pass 18 per core,
54 comparisons and 153 checked Swift images at
`/tmp/jed-swift-sim-sweep-20261106-2057112-ozakk7/summary.json`. The final generated release sweep
also passes 176 traces per core, 459 comparisons and 7,049 checked Swift images
(/tmp/jed-swift-sim-sweep-20261106-2104641-awof5y/summary.json). Performance was still
outstanding at that checkpoint; the completed comparison is recorded in BENCHMARKS.md.

## State and ownership

| Implemented type | Responsibility |
|---|---|
| `TableStore` | A value-semantic B+tree, synthetic next-rowid, and Q3 pending-version ledger. Returns packed records and lazy cells. |
| `DomainSnapshot` | Immutable published catalog plus independent table/index roots, nominal type identities, txid, catalog/data revisions, and estimator revision tokens. |
| `StorageDomain` | One Pager and NodePool, storage identity, committed snapshot, exact live/catalog/GiST page counts, and free-list generation. |
| `DomainCommitPlanner` | Prepare replacement roots, deterministic page allocation, live-count delta, free-list/manifest plan, and resource admission. Performs no device writes. |
| `PreparedDomainCommit` | Sealed bytes plus the corresponding cleaned snapshot and accounting. No intermediate state is published. |
| `DatabaseCore` | Single-writer gate and live-reader registry; pin-latest and publication share a lock. Sessions hold private working roots. |

Every domain, including in-memory main, attachments, and session temp, binds
stores created after open to its own paging identity. Metadata maps/arrays may
be resident. Table values remain in packed leaves with a resident interior
skeleton and bounded clean-leaf pool. An expression row slot holds either a
value or a record/column reference; obtaining a value materializes only that
column. There is no decoded row dictionary and no commit-time scan that rebuilds
the database from decoded rows.

GiST retains the baseline's explicit exception: a resident canonical R-tree
and maintained leaf keys, rebuilt after a mutating statement and rewritten
whole at commit. Ordinary indexes and GIN are zero-value-column B+trees with
their own roots.

## Open, fork, and mutations

Open uses Pager's selected validated meta and free list, reads the catalog
chain, resolves the complete nominal type graph and persisted dependencies,
and opens each table and B+tree/GIN index skeleton. It records catalog and GiST
page counts while those structures are already being read. It does not recount
all live pages; meta offset 56 supplies the exact count. A keyless table can
recover its next rowid with a descending cursor's first key, bounded by tree
height. No row values need to be decoded for that operation.

A transaction forks immutable root references. Each statement first builds a
candidate working snapshot and adopts it only after successful validation;
failures discard that candidate and follow the session's abort rules. Catalog,
sequence, statistics, synthetic rowid, table roots, and index roots travel
together. Persisted catalog root fields are patched from the commit plan,
never treated as independent mutable authoritative state.

Changed-domain detection uses revisions/root identity/rowid state, not row
counts or image equality. A zero-row DML statement without side effects does
not dirty statistics or a domain. A real rewrite can dirty a domain even when
its resulting SQL values compare equal. An unchanged domain never enters the
Pager commit path or performs recovered-generation stabilization.

Q3 records the stored bytes of staged versions, including overflow payloads.
Under a finite budget the per-key ledger permits net replacement/deletion;
unlimited writes still add bytes but do not require the ledger. Savepoint
creation/restoration forgets per-key release eligibility while preserving the
captured total. Temp is excluded from Q3. Store replacement/drop discards its
staged balance. The executor owns write-phase and end-of-statement admission
against Q1/Q2 beside the pending balance, with cost errors taking precedence.

## Commit preparation and publication

1. Identify changed domains and enforce the existing one-durable-writer rule.
   Apply top-level temp actions. Precheck every limited changed domain before
   packing any domain when a multi-root commit requires that rule.
2. Validate catalog metadata before sealing. Obtain the full persisted free
   list, the separately filtered safe allocation set, pending-batch protection,
   and current high-water. The reader watermark and free-list generation gate
   govern reuse; relaxed batches additionally allow only floor-free pages.
3. In raw UTF-8 table-name order, serialize a table's dirty B+tree paths followed
   by its indexes in name order. GiST emits its whole canonical tree. Keep clean
   subtree references and inherited overflow heads. Allocate/encode the new
   catalog chain last. Preserve encrypted nonce draw order independently from
   page-number order.
4. Compute `live_after = live_before + new_body_pages - old_catalog_pages -
   old_gist_pages - orphaned_btree_and_overflow_pages`. Walk old skeletons while
   skipping kept subtrees. Only orphaned variable-column leaves need faulting;
   their stored pointer lengths count overflow slabs without reading chains.
   Exclude inherited chain heads retained by rewritten records.
5. Reclaim at the specified threshold, respecting all current manifest
   dependencies and fresh writes. Plan free-list pages and reserved manifest
   pages with the existing monotone algorithm. Preserve persisted entries
   excluded from allocation by reader/batch gates. Apply Q4's file live-page or
   memory high-water rule, forced-compaction and repair clauses as applicable.
6. Construct Pager's immutable prepared commit and the replacement snapshot.
   Serialization or admission failure discards working state and leaves the
   handle usable. Encrypted failures after sealing may consume nonces as the
   shared format allows; a successful commit must draw exactly the prescribed
   stream, so dry multi-root admission must not consume extra draws.
7. Under the writer gate, invalidate reused page IDs in NodePool before writing.
   Pager writes bodies, manifest, alternate meta, and the required barrier.
   I/O failures poison the writer according to Pager's existing policy. Adopt
   clean replacement roots/accounting and publish under the same registry lock
   used by pin-latest. A reader in the persist/publication interval pins the old
   version and therefore prevents later unsafe reuse.

The lifecycle owner exposes full persisted free entries separately from safe
reusable entries through `Pager.allocationSnapshot(reuseAllowed:)`. The older
`allocationState(reuseAllowed:)` helper exposes only the reusable subset. Keeping
both sets preserves allocation/reclamation behavior. Cache invalidation does
not replace reader pins.

From-scratch export/create/compaction retain their distinct allocation rules:
children before parents, a leaf page ID before its overflow chain IDs, catalog
last. Incremental writes allocate fresh overflow before its leaf. Existing
stored overflow bytes copy without decompression/recompression on export.
Create initializes meta slots/key blocks through the normal format and entropy
seams; maintenance must not alias a source cache into a new page-address space.

## Integration boundary

The storage slice owns `TableStore.swift`, `DomainSnapshot.swift`,
`StorageDomain.swift`, `DomainCommit.swift`, and `PageReachability.swift`.
Root integration retains session APIs, transaction/savepoint control, attachment
routing, the writer gate/pin coordinator, and SQL constraint/index maintenance.
Pager/allocation API changes are coordinated with their owner. The storage
slice exposes prepare/install operations rather than silently publishing a
domain on behalf of a multi-domain coordinator.

## Acceptance gates

| Gate | Required evidence |
|---|---|
| Lazy open | Instrumented host proves catalog/skeleton open avoids unrelated table leaves and overflow reads; new tables bind the pool. |
| Round trip | All frozen table/index/large-value/continued-catalog images open and rebuild byte exactly, including seeded encrypted images. |
| Independent roots | Editing one table/index retains other roots and their pages; no-op and temp-only transactions issue no main writes/barrier/txid increment. |
| Snapshot safety | Pinned roots/cursors and savepoint restores keep old records, rowids, catalog facts, and pending accounting. The seven shared concurrency schedules exercise session behavior. |
| Dirty-region accounting | Incremental live counts equal an independent reachability recount, including kept overflow chains, drops, rewrites, and GiST. |
| Reuse safety | Fallback readers, manifest-only dependencies, deferred reuse, relaxed batches, and cache flights cannot observe overwritten pages. |
| Limits and failure | Q3 net/savepoint rules and Q4 file/memory/repair/multi-root cases; no storage writes on preparation rejection; I/O failures poison and recovery selects a valid generation. |
| Shared execution | Whole SQL, concurrency, simulation, process, integrity and interop gates have separate retained evidence. Optimized release passes 515 native tests/all 749 SQL files, complete V04, encrypted/interchange and saved-simulation refreshes. Earlier process/service/generated-simulation gates retain their actors. Final evidence and measured results are in VERIFICATION.md and BENCHMARKS.md. |

## Implemented integration interfaces

The native slice now exposes these internal Swift interfaces; their existence
is not a claim of SQL/session or full host acceptance.

- `StorageDomain.open(store:kind:catalogID:cacheBytes:credential:readOnly:collatedKey:validateCatalog:syncDirectory:)`
  opens one address space. The required `validateCatalog` callback checks loaded
  timezone, collation, and persisted-expression dependencies in the caller's
  environment before a domain is returned. `create` initializes an empty device.
- `domain.committed` is storage bookkeeping. Readers use `DatabaseCore.pinLatest`
  and its registry-published snapshot; reading this property does not pin pages.
- `domain.precheck(working, policy:)` performs structural and Q4 admission without
  sealing bytes or writing storage. It may retain a larger in-memory free list
  after a forced reachability pass, even if admission subsequently rejects.
- `domain.prepare(working, policy:entropy:verifyLiveCount:)` returns an immutable
  `PreparedDomainCommit?`; `nil` means no changed domain and performs no I/O.
  The verification option compares the exact incremental live count against an
  independent reachability traversal and is a boundary-test tool.
- `domain.persist(plan, synchronous:syncDirectory:)` writes through Pager and
  returns clean roots. The coordinator publishes them under the same lock that
  registers pins, retaining the writer lease until publication is complete.
- `DomainCommitPolicy` carries `oldestPinnedVersion`, `canCompactCommitted`,
  `reclaimAllowed`, `storageLimit`, and `domainName`. With no pins the watermark
  is the prospective new txid. Page clients disable reclamation. Forced memory
  compaction requires no pin older than the currently committed version.
- A working snapshot mutates through `updateCatalog(_:schemaChanged:)`,
  `setTableStore`, `setIndexStore`, and `setGistTree`. `emptyStore(types:)`
  binds a new packed tree to the domain's pool. Each actual mutation gets an
  opaque revision identity. Catalog and store changes must be adopted together
  after statement validation, including rollback of any staged revisions.
- `TableStore.put`, `remove`, `editGIN`, and `allocateRowID` preserve snapshot
  copies. `pendingBytes`, `forgetPendingKeys`, and `clearPending` expose Q3
  accounting; the session owns aggregate budget checks and excludes temp.

Memory commits use Pager's ephemeral metadata path: no manifest/free-list pages,
no persisted live-page count, and a RAM free list. The ordinary device is not
reset or truncated during forced compaction. Reclamation preserves all pages
reachable from the currently committed roots plus the last commit's writes.
Consequently a reader that pins that version after policy capture remains safe.
The free-generation gate prevents subsequent reuse while any older pin survives.
The repair exemption separately records the last *successful commit's* catalog
page count, initially zero after memory create/open; bootstrap catalog pages do
not qualify an initial empty-object mutation for repair. File admission uses
the loaded live catalog page count, as specified by the separate Q4c rule.

From-scratch export, atomic file compaction/rekey, process ownership and public
sessions/cursors are now connected through their dedicated modules. Their host
and cross-core gates remain distinct from the incremental-storage checks; see
the current acceptance summary and the host matrix.

## Streamed image implementation

`DomainImage.write(_:txid:codec:keyBlock:entropy:sink:)` writes a from-scratch
image as `StoredPage` callbacks and returns `DomainImageSummary` with its root,
page count, and live count. The sink receives each page once in sealing order,
not necessarily ascending page-number order, with both meta slots last. The
caller must hold the source pin and owns destination durability, atomic rename,
and cleanup after a partial destination failure. Export does not write or
poison the source Pager. `Pager.selectedKeyBlock` supplies the validated wrapped
key-block record for a same-key encrypted export.

The B+tree walk retains routing references and one packed leaf's records. Fresh
chains stream directly from their already-staged stored bytes. An inherited
chain first validates and retains its source page references, then rereads and
reseals one slab at a time backwards. This keeps the exact compressed or plain
stored bytes without decoding or recompressing a SQL value, and preserves the
canonical leaf-before-chain allocation and chain-before-leaf nonce ordering.
Its extra reference vector is proportional to one chain's page count; this is
not a claim of constant metadata space. GiST retains its baseline resident-tree
exception, while its output pages stream. Catalog metadata remains resident and
its pages emit in reverse chain sealing order.

The serializer supports the existing page layout, including a fresh encryption
key with the same layout. Changing page size or plain/encrypted layout requires
a separate ascending-record rebuild before invoking it; the current writer
rejects such a request with `0A000` before output. `DomainRebuilder` and
`DatabaseMaintenance.rekey` provide the required rebuild before this sink. The
public compaction contract keeps page size unchanged; the sink guard is not a
missing rekey implementation.

Image boundary tests compare 85 canonical frozen images byte-for-byte, including
six encrypted images under the canonical seeded entropy stream. Two additional
torn-meta fixtures compare every body byte and require both output meta slots
to equal the selected valid source generation, repairing the intentional CRC damage. They
also cover output failure and retry, source immutability, metadata preflight,
and callback ordering. The historical 197-test checkpoint below passed these
cases; the current 112-image interchange refresh provides broader file evidence.

## Memory attachment staging

`StorageDomain.stageMemory(_:)` writes a prepared memory domain's safe body
pages without meta writes, syncs, root publication, or high-water/free-list
adoption. It returns `StagedDomainCommit`. The writer lease stays held until the
durable domain has succeeded or the transaction has been abandoned.
`adoptMemory(_:)` installs the staged snapshot and accounting without I/O, after
which the coordinator can publish it. Do not expose the staged snapshot to a
reader before adoption. A later stage supersedes an earlier token; attempting
to adopt stale or already-adopted bytes fails `55006`.

Dropping a stage leaves unused bytes in slots that were already free or beyond
the published high-water. The next transaction may overwrite them; published
readers still see their old roots. Physical capacity reservation can remain,
and a forced free-list compaction from precheck can remain, as allowed by Q4.
The stage does not adopt a free list computed from unpublished roots. A boundary
test injects main-file failure after attachment staging, checks the attachment's
old roots and metadata/accounting, then overwrites the abandoned slots and
verifies token rejection and zero-I/O adoption.

## Historical verified checkpoint (2026-11-05)

The parent's `mise run swift-native:test` run completed on 2026-11-05 at
18:20:42 local with **197 tests, zero failures** (64.269 seconds). It includes
all eight `StorageDomainBoundaryTests`, six `DatabaseCoreBoundaryTests`, and
three `DomainImageBoundaryTests`. The storage cases opened and incrementally
committed 87 frozen images, checked exact live deltas against independent
reachability, preserved old cursor/overflow roots, exercised Q3/Q4 and
multi-domain precheck, and verified attachment staging after main I/O failure.
Image cases covered 85 canonical byte-exact images (six encrypted), two torn-meta
recovery images, bounded leaf caching, destination failure/retry, and metadata
refusal before output. Coordinator cases exercised actual reclaimed-page reuse
across the persist/publication pin window, cancellation, pin lifetimes,
maintenance exclusion, read-only boundaries, and current-versus-older Q4 pins.

Mechanical codegen verification also passed with the complete frozen corpus,
112 database byte fixtures, Unicode source/artifact fixtures, and PRNG vectors:
`7024bdb84a3dc257a81d965161303df5ae427f0870a4c5750dcc99432d6d7f83`.
That checkpoint established internal storage/host behavior only. The later SQL,
process, maintenance and migration gates are recorded separately; the acceptance
summary above records their later outcomes and actor-specific scope.

## Scratch record rebuild implementation

`ScratchDomain.create(store:codec:cacheBytes:entropy:)` owns a private, non-reopenable
page address space over a caller-owned byte device. It emits one empty catalog
body and keeps synthetic metadata in the Pager; staging emits body pages only,
with no meta-slot writes or durability barrier. It uses ordinary incremental
B+tree serialization, demotes completed leaves into the bounded NodePool, and
periodically rebuilds its private free vector by reachability. GiST leaf-key
stores are ordinary scratch B-trees until the canonical GiST is built at the
end. The source's caches, page IDs, roots, and device are never adopted here.

`DomainRebuilder.rebuild(source, scratch: &scratch, stageBytes:)` starts empty
stores under the target codec, retaining synthetic rowid counters and all
catalog objects/statistics. It scans tables first, then indexes, each in raw
UTF-8 name order and ascending key order. It fully materializes one row and
replans its cells with RecordDisposition under the target capacity and pointer
width. Staging occurs after pending stored bytes reach the threshold, so the
batch can exceed the threshold by one record; large single values and the
existing interior/GiST metadata contracts remain explicit memory costs.
Rebuilding does not collect a table's decoded rows or a whole output image.

An encrypted scratch codec must have a throwaway key and an independent entropy
source. The caller separately draws the final image's DEK, key-block salt/wrap
nonce, and page nonces. The rebuilt snapshot's txid belongs to scratch paging;
`DomainImage.write` receives the logical output txid explicitly. Source pins,
exclusive maintenance admission, final Q4 admission before replacement,
destination barriers/atomic rename, and cleanup remain the coordinator/host's
responsibility. Rejection or failed scratch I/O can discard the scratch device
without writing or poisoning the source.

The primitive accepts another page size internally, but the frozen public
compaction contract keeps page size unchanged (`api.md` §2.6). No public page-size
change is introduced. Plain/encrypted conversion and DEK rotation use ascending
rebuilding even when the nominal page size is unchanged (`encrypted.md` §12).

The byte-device injection is the native spill-host boundary. A MemoryBlockStore
alone still holds its entire scratch image in memory. The native host must
provide migration to an exclusively created, unlinked scratch file after the
page-cache budget, with no grow barriers and sealed host scratch when required.
The native spill/migration adapter and atomic file maintenance are implemented
in the following sections. Bounded leaf/record residency is not a whole-engine
heap limit. The three rebuild boundary tests passed in the parent-owned integrated run at
18:31:30 on 2026-11-05 (1.0 seconds).

## Native scratch host follow-on

The native spill boundary is now implemented as `ScratchFileHost` and its
`NativeScratchFileHost` adapter. `SpillableScratchStore` starts in memory and,
when a completed stage's logical page bytes exceed its positive budget, copies
its backing in at most 64 KiB chunks to a private scratch file. It installs the
new device only after every copy succeeds; failure closes the candidate and
retains the original device. The bounded NodePool and all page references remain
valid. `ScratchDomain.stage` evaluates this threshold before adopting its staged
generation. No spill host, or a zero threshold, keeps the original memory backing.

POSIX scratch files are created with `O_EXCL | O_CLOEXEC`, mode 0600, and unlinked
immediately. Their `sync` and grow barriers are disabled; durable UnixBlockStore
behavior remains the default. An explicit idempotent close operation prevents
future device accesses, and inherited fork handles fail closed.

`SealedScratchStore` implements encrypted.md §11 using the existing vetted
XAES-256-GCM adapter: 4096 logical bytes become ciphertext, 24 nonce bytes and a
16-byte tag. Associated data binds the scratch role, per-file random ID, block
ordinal and monotonically increasing write counter. Counters survive truncation;
removed blocks become logical zeros, and a partially retained block has its cut
tail zeroed and resealed. Each block write seals immediately (the optional
single-block plaintext cache is not used). Authentication failures are `58030`.
File-name and scratch-key entropy sources are independent of database key entropy.

The four new ScratchBoundaryTests cover byte-preserving migration, threshold
admission, zeroed holes/truncation, stale-version and cross-file/ordinal replay,
private-file unlinking, and actual native sealed spill during a 150-row indexed
rebuild. All four passed in the parent-owned integrated run at 18:43:59 on
2026-11-05 (0.182 seconds). File coordination, public lifecycle, page service,
atomic replacement and bounded SQL spill were connected later and have their
own gates in the current acceptance reports.

## Native lifecycle and protocol-v2 exclusive host

The first native file slice implements public `CreateOptions`, `OpenOptions`,
`DeserializeOptions` and the `Database.create/open/deserialize/serialize` entry
points. Creation writes a private same-directory file, syncs it, publishes using
an atomic no-replace rename, then syncs its directory. The descriptor remains
open across rename. File open preserves lazy bounded leaf paging, read-only
access, encryption credentials and loaded environment data. The same key stream
used by encrypted creation continues into later commits; OS file-name/owner and
scratch entropy do not consume it. Parent directory sync is retained by the
StorageDomain and performed on the first post-open stabilization, including when
commits originate through the ordinary Database/Session coordinator.

The exclusive host normalizes symlink identity, rejects coordinated hard links,
claims a process-local path registry, and holds the stable protocol-v2 presence
inode with a crash-clean kernel flock. Arrival SH covers final marker validation.
Unknown protocol markers and symlinked coordination entries fail closed. A
non-serving owner record uses the shared wire contract; it is diagnostic only.
The byte device is closed before owner removal and presence release. The bundle
itself remains in place. Inherited handles refuse operations across fork.

`locking: .none` is the explicit external-coordination escape hatch. The default
`.automatic` is routed by public `Database.create/open` through
`ServiceDatabaseHost` on Linux, providing owner/client election, handoff, remote
pins and commit deltas. The direct `FileDatabaseHost` automatic-mode guard is
intercepted before this path; it is not a reachable Linux capability omission
or an exclusive fallback. File attachments use the same host routing. Apple
platform support remains unverified.

Public serialization pins the committed snapshot and collects the canonical
page-sink output into its explicit host buffer. It rejects encrypted sources
with `0A000`; deserialize rejects encrypted images with `55000`, as required by
the frozen baseline. Internal maintenance retains encrypted DomainImage support.
Named attachments resolve through `DatabaseDomains.pinCommitted`; only the
selected committed domain is pinned. Unknown names and `temp` return `42704`.

Catalog environment admission rejects a completely absent referenced collation
with `XX002`, preserves version-skewed collation pins and stored definitions, and
provides per-table write gates plus planner collation-match checks. Index host
and timezone pins are checked when maintaining or selecting that index; opening
and reading stored rows do not require those dependencies. Missing host functions
retain ordinary resolution errors; changed component/version pins are `XX002`.
CHECK dependencies are checked only by statements that evaluate the check.

Seven FileHostBoundaryTests cover import/export bytes, caller-buffer isolation,
graded collation admission, real exclusive ownership and lifetime, marker and
symlink rejection, process-claim cleanup after failures, hard-link refusal,
read-only/cache configuration, retained first-commit directory barriers, and key-stream continuity. All seven passed in the parent-owned integrated run at 19:04:07 on 2026-11-05; these are host/byte invariants, not SQL or
multi-process conformance claims.

The first-party `CJedHost` publication adapter contains the atomic no-replace
rename OS call absent from Swift's Linux Glibc module. Stream transport adds only
ordinary process/descriptor adapters, documented in `PAGE_SERVICE_PLAN.md`. The safe Swift wrapper borrows its
owned path strings synchronously and checks errno; the shim retains no pointer.
This is the approved ordinary-OS host seam, not an engine or library dependency.

Statistics use the same graded collation verdict before key validation. A skewed
pin preserves canonical typed MCV/histogram bytes and all structural checks but
skips old comparison-key validation, duplicate-key/order checks and histogram
ordering under the newly loaded bundle (statistics.md §2; baseline Go
`decodeStatisticsValue`/`decodeStatisticsEntry`). The facts remain stored and
must stay hidden from estimation until rebuilding. The new catalog boundary
case verifies old-order preservation plus malformed typed bytes/count rejection.

## Atomic maintenance and credential replacement

`Database.compact`, `changeCredential` and `rekey` now have handwritten native
implementations for main and exact named attachments. Each operation obtains the
root logical writer gate without waiting and rechecks the preflight target's
identity under the attachment publication boundary. Compaction and rekey also
acquire the target's writer gate and hold its pin registry through serialization
and publication. An existing target pin refuses replacement with `55006`; pins
on another domain do not. Incoming bundled pins wait until publication completes.
Memory compaction writes a fresh chunked byte device. File replacement streams to
`<path>.jedtmp`, preserves permission bits, syncs the completed file, closes the
old descriptor, atomically renames, syncs the directory and opens a cold pool at
the same cache capacity. Before rename, failure leaves the original file intact;
a rename failure reopens the original byte device, and failure after replacement
revokes the old address space and requires reopening the handle. The lock bundle
is retained across the entire operation. Explicit compaction does not enforce a
new Q4 trip point after an application lowers its limit.

Rekey uses the bounded scratch rebuild and then the same replacement sink. It
checks the new image's exact live-page bytes against Q4 before rename. Target
layout/key errors likewise leave the original file unchanged. Encrypted scratch
uses a throwaway key and OS entropy separate from the final image stream; target
key material is prepared before obtaining the gate. Encrypting a plaintext
source begins a fresh environment key stream; rotating an encrypted source
continues its current stream. On successful replacement that stream becomes the
one used by subsequent commits. All target trees are rebuilt by ascending
insertion, including canonical GiST reconstruction; ordinary compact retains the
source tree shape.

Credential change retains the data key privately inside EncryptedPageCodec and
runs rewrapping/KDF work before admission. Its local-maintenance gate excludes
local writers but allows an idle remote writer grant; a streamed remote delta
refuses credential writes with `58030`. Commit-boundary serialization and exact
lease cleanup preserve this distinction. It checks both storage identity
and selected key-block generation again under the gate. Its storage phase
flushes any pending relaxed batch, performs stabilization and the retained
first-directory sync, writes the unselected copy, barriers, writes the other
copy, and barriers. Existing readers remain valid. Failure revokes the handle;
new opens use structural key-block selection and repair before writing. An
internal already-unlocked codec argument permits maintenance to reopen its own
newly authenticated encrypted image without retaining the user's credential or
repeating Argon2. It is not part of the public open API.

`Database.close` releases the native descriptor and lock even when flushing a
poisoned pager fails, preserving the first failure. Refusal to acquire the
maintenance gate leaves the handle open. Five MaintenanceBoundaryTests cover
these host/byte invariants, including a frozen encrypted GiST fixture rebuilt
through plaintext, encryption and key rotation. All five passed in the
parent-owned integrated selected run at 19:18:54 on 2026-11-05 (0.861 seconds).
The later named-maintenance tests cover target-only pin exclusion, root writer
exclusion, attachment identity after preflight, credential readers and root
scratch policy. Native maintenance, refreshed process and generated simulation
gates now pass; the refreshed service SQL matrix also passes all six directions.

## Grouped domain commit integration

`DomainCommitParticipant` carries one exact SQLText domain name, held WriterLease,
working DomainSnapshot, policy, entropy source and synchronous setting.
`DomainCommitGroup.prepare` rejects duplicate names/storage identities and a
second actually changed file domain, omits unchanged domains, checks every
changed domain's Q4/catalog layout before sealing any pages, then prepares in
attachment-name order with main last. A statement that attempts writes in two
durable domains is rejected by statement admission. Separate statements may
stage those changes, but COMMIT rejects the transaction before any page writes,
as asserted by the frozen attachment gate-order process scenario.

The root database owns writer acquisition order and a publication boundary
shared by bundled read admission. It retains that boundary and every writer
lease across `PreparedDomainCommitGroup.commit()` and installation of the
returned changed-root map. Commit stages all memory bodies first. Each memory
core's commit boundary remains held, and staged-token validation plus adoption
accounting construction finish before the sole file persist. Once that file
reports known success, memory accounting and per-core root adoption are
nonthrowing; a remote file role replacement keeps the existing known-commit
resolution behavior. No unchanged file gets a txid, meta write or barrier.

Failure before file success leaves memory roots and selected page high-water
unchanged. Staged bytes occupy only safe slots; an abandoned group is discarded
and a later transaction prepares and stages again. A memory device I/O failure
poisons that domain only. Forced memory Q4 compaction retains its reader-safe
free-list, compaction baseline and free-generation update even if this or a
later domain refuses, as memory.md §8.3 explicitly requires. No bytes or roots
change during that preflight. The initial group implementation mistakenly kept
this proof private; the all-corpus storage gauge comparison exposed the error,
and the correction/regression evidence is recorded in RESOURCE_ACCEPTANCE.md.
Old pins continue to protect their own domain's pages. The original five host
invariants passed their earlier gate. The corrected compaction assertions and
new refusal/retry test passed subsequent focused and full native gates; complete
debug/release V04 now agrees on committed-storage bytes. A sixth test
covers the pre-durable memory staging hook used by private temp spill.

The frozen process backup prefix also exposed the explicit large-values.md
§15.2 root-leaf exception: newly written resident overflow values continue to
allocate fresh chains while their single root leaf stays resident. Swift now
retains those original pending payloads alongside its packed root leaf and
demotes other leaves. Reloaded values keep their stored chain pointers. This
preserves the baseline's allocation/free-list behavior without decoding table
maps or changing the shared golden expectations.

A committed root with an inherited external reference follows the separate
mixed-root rule: after persistence the root re-faults through the page pool,
converting neighboring fresh caches to stored references too. Pure-fresh roots
retain their caches and continue rewriting chains. The generated attachment and
reuse-under-pins regressions distinguish these cases; their refreshed byte/image
comparisons pass. This does not change GiST accounting: reconstruction of
committed GiST keys creates no pending transaction versions. The shared
supported-core loader correction discussed at the top preserves that contract.
