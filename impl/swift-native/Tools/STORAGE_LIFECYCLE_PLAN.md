# Proposed database storage and state seam

This is an implementation plan for the frozen Swift experiment, not a new
shared storage design. The contracts are `transactions.md` §§2–3, 6–8, 11;
`pager.md` §§2–5; `validated-cow.md` §§1–4; `memory.md` §§7–8; `large-values.md`
§15; `gist.md` §4.1; and the v50 plain/encrypted file formats. Work remains
unimplemented until the corresponding source and verification evidence exist.

## State and ownership

| Proposed type | Responsibility |
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

The lifecycle owner must expose full persisted free entries separately from
safe reusable entries; Pager's current `allocationState(reuseAllowed:)` exposes
only the latter. Losing the filtered remainder would change allocation and
reclamation behavior. Cache invalidation is not a replacement for reader pins.

From-scratch export/create/compaction retain their distinct allocation rules:
children before parents, a leaf page ID before its overflow chain IDs, catalog
last. Incremental writes allocate fresh overflow before its leaf. Existing
stored overflow bytes copy without decompression/recompression on export.
Create initializes meta slots/key blocks through the normal format and entropy
seams; maintenance must not alias a source cache into a new page-address space.

## Integration boundary

The storage slice proposes owning `TableStore.swift`, `DomainSnapshot.swift`,
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
| Snapshot safety | Pinned roots/cursors and savepoint restores keep old records, rowids, catalog facts, and pending accounting. Shared concurrency corpus drives session behavior later. |
| Dirty-region accounting | Incremental live counts equal an independent reachability recount, including kept overflow chains, drops, rewrites, and GiST. |
| Reuse safety | Fallback readers, manifest-only dependencies, deferred reuse, relaxed batches, and cache flights cannot observe overwritten pages. |
| Limits and failure | Q3 net/savepoint rules and Q4 file/memory/repair/multi-root cases; no storage writes on preparation rejection; I/O failures poison and recovery selects a valid generation. |
| Shared execution | Full memory/disk SQL corpus, concurrency, simulation, process scenarios, integrity fixtures, and interop remain required beyond these internal storage checks. |

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

From-scratch export, atomic file compaction/rekey, process ownership, public
sessions/cursors, and shared SQL adapter acceptance remain separate work. The
current incremental source must not be described as completing those host APIs.

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
rejects such a request with `0A000` before output. Full migration/rekey remains
required by the experiment's scope and is not claimed by this rejection path.

Image boundary tests compare 85 canonical frozen images byte-for-byte, including
six encrypted images under the canonical seeded entropy stream. Two additional
torn-meta fixtures compare every body byte and require both output meta slots
to equal the selected valid source generation, repairing the intentional CRC damage. They
also cover output failure and retry, source immutability, metadata preflight,
and callback ordering. Results await the parent's integrated test run.

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

## Verified checkpoint

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
This is internal storage/host evidence, not acceptance of the pending full SQL,
process service, host maintenance, or page-layout migration surface.
