# Native Swift page-service integration plan

Baseline: `362e0798ac3a8bd04bd2da556044b6ec73bf468a`. Canonical contracts:
`spec/pageservice/README.md`, `messages.toml`, `fixtures.toml`,
`spec/design/page-service.md`, `locking.md`, `validated-cow.md`, and
`spec/fileformat/encrypted.md` §14. This is an implementation plan, not a claim
that automatic file opening or cross-process conformance currently passes.

## Native boundaries and owner fast path

The protocol codec, owner/client state machines, delta validation, cache and
reader-watermark policy remain handwritten Swift. POSIX sockets, poll, file
operations and crash-clean locks are narrow host adapters. No SQL text is sent
to an owner: the client's independent parser, type system, planner and executor
operate over its own page device and environment. Stored bytes cross the socket,
including ciphertext; a client holds its own unlocked data key.

An owner foreground transaction uses its DatabaseCore writer gate and pin
registry. It performs no socket/lock coordination syscall and no meta reread.
The server serves connections on independent execution threads; a begin_write
waiter cannot prevent later reads on the same connection. Requests and replies
use ids, with serialized frame writes and a dedicated reader/demultiplexer.

## Coordinator and storage integration

`DatabaseCore.beginRead()` becomes the throwing logical snapshot acquisition
seam. Existing `pinLatest()` remains local/internal. A local owner takes the
ordinary registry pin; a client performs begin_read, applies invalidations and
adopts the returned meta before registering the local pin. Exactly one owner pin
is retained for each version with local reader references. Duplicate begin_read
pins are released only after local registration; the last local close releases
the remaining owner pin. Page fetch selects the newest pinned version and sends
while no end_read/abort/commit_end can overtake it.

`acquireWriter` retains the process-local gate and then acquires the owner's
remote gate when a client. It accepts session lock timeout and durability mode.
The owner's remote writer holds that same local gate through validation,
publication, acknowledgement and any required reclamation maintenance. A raw
remote writer lease does not require parsing the owner's SQL catalog.

The registry stores committed structural metadata independently of its last
adopted SQL snapshot. Incoming client commits update the selected meta, free
page accounting, invalidation and resolution windows atomically with publication.
Owner sessions reload and validate their own environment at their next begin;
that local admission may fail without undoing or rejecting an acknowledged
foreign commit. CatalogExpression will cache parsing lazily, so structural
catalog load/reclamation never invokes SQLParser. Ordinary resolution observes
the same cached optional AST and existing XX003 phase.

Pager receives explicit service commit stages: validate before writing any
manifest, validate each body before writing it, verify completion, then write
meta/barrier and publish. Failed validation leaves only unreachable orphan pages;
storage failure poisons the owner. A client has no reachability reclamation,
trailing truncation or forced compaction. Its allocation obeys the owner reuse
grant and pending-floor subset. Encrypted incoming meta authenticates under one
slot; the owner reseals it for its actual selected alternate slot.

## Native role lifecycle

A process-local path claim survives client reconnections and promotion. Bundle
handles are separate from the claim. Automatic election holds arrival SH while
validating protocol-v2, tries presence EX unless ownership is never, or dials the
same-user owner record's private Unix socket. Only the owner opens database file
bytes. Creation always takes ownership. Socket paths follow the 100-byte bundle
then private-runtime fallback contract; directory mode/ownership are checked.

Role changes install fresh storage and revoke previous-role byte access. Existing
explicit transactions fail 08006 at next statement/commit; rollback succeeds.
Read/transaction begins that lost their connection before receiving a pin retry
under re-election. Lost commit acknowledgements resolve by meta digest through
the retained 256-commit window (08006 not committed, 08007 unknown). Yield,
read-only-owner handoff, grace draining, limits, idle revocation and the 2-second
handoff pause follow README §6.5, not PID or timestamp leases.

## Verification sequence

1. All frozen protocol frames encode and decode exactly; malformed lengths,
   counts, UTF-8, credentials, reserved messages and booleans reject with 08P01.
2. Delta check A/B/C and encrypted slot/reseal byte invariants; watermark and
   publication races that the SQL corpus cannot represent.
3. Native process-host adapter runs the shared process scenarios, including
   cross-core roles, read/write corpus through clients, handoff, dropped commit
   acknowledgements, encrypted key continuity and backup. No scenario is silently
   skipped or counted as a pass while unsupported.
4. Shared deterministic simulation integration models role/pin/frame counts and
   crash schedules. Remote stream clients and backup use the same protocol;
   reserved snapshot_stream/subscribe remain protocol violations at this baseline.

Parent owns integrated SwiftPM builds and checkpoint commits. Coherent draft
source sets are staged outside Sources until a publication cue.
