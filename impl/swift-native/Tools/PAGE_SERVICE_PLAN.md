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

## Verified primitive milestone

The parent-owned focused run on 2026-11-05 passed all six initial page-service
tests: the complete frozen frame/owner-record/malformed-fixture adapters, plus
streamed delta admission, pending-floor identities, forbidden reuse, encrypted
meta authentication under either slot, and normalized commit-resolution digests.
The mechanical fixture fingerprint is now
`7a1bcf53fefb01e43484e760a68d9f61faabbc0c15c35d2b1da39c4c90db8043`.
This verifies codecs and validation primitives only. Automatic owner/client
activation, remote publication, handoff, backup and process scenarios remain
unimplemented at this milestone.

## Automatic host and role lifetime integration

The first native owner/client transport milestone passed the focused suite:
owner framing and multiplexed gate waits, exact shared wire fixtures, delta
validation, encrypted resealing, structural publication, client pin accounting,
client native commit planning, and encrypted refresh. These checks do not yet
claim automatic file election, process-corpus coverage, handoff, backup, or
stream transport completion.

The automatic host retains `FilePathClaim` while bundle handles and storage roles
change. `DatabaseCore` gives each role a distinct epoch: an old pin cannot reduce
a new role's count merely because both histories use the same txid, and an old
transaction cannot commit through newly installed storage. Local physical commit
and publication share a short exclusion boundary with role replacement; remote
commit-status resolution stays outside that boundary so reconnecting cannot wait
on its own commit guard. Old cached immutable leaves may finish a cursor; an old
pager fault fails `08006`.

`ServiceDatabaseHost` will supply a weak coordinator at logical begin. An owner
checks only resident role/admission state. A disconnected client re-elects under
its open timeout, retaining its unlocked data key and process-local path claim.
A local writer blocks host close with `55006`; an automatic owner's remote writers
and pins instead receive shutdown notice and the configured grace period before
final gate acquisition, flush, descriptor close, and presence release. Yield
admission pauses new begins while existing transactions drain; cancelled yield
resumes those requests. This differs from generic exclusive-host close, which
keeps its current immediate writer-gate admission.

Automatic file roles (drafted for the next isolated host gate): `PageServiceOptions`
exposes serve/preference and the five baseline grace/connection controls. One
`FilePathClaim` outlives every native owner/client role. `DatabaseCore` retains a
weak coordinator and a role epoch, and pauses local admission under its pin
registry before drain. Owner queries use only resident synchronization. A native
owner publishes its incarnation/endpoint after winning presence; a client drops
all bundle descriptors after hello/bootstrap. Reconnection retains the unlocked
DEK, invalidates the old pager and pins, and reopens structurally before SQL
adoption. Lost acknowledgements resolve through the bounded owner history.

The automatic-host tests launch the same already compiled XCTest binary as a
small host-only peer. This exercises genuine distinct process path claims and OS
locks without another build, another engine, or per-core SQL expectation copies.
It does not replace the 31 canonical process scenarios, which still need their
experimental runner.

Post-ack maintenance preserves two boundaries. First, the service lease releases
its read pin only after its commit has been acknowledged, retaining the exclusive
writer token through cap flushing and maintenance. Second, maintenance makes its
structural snapshot under the registry, prepares outside that lock, then checks
that the role, empty reader watermark, storage identity, and base txid still
match before any physical maintenance write. A reader or local environment
adoption during preparation makes maintenance abandon its plan. It cannot undo
or turn the already acknowledged client commit into an error. Serialization
failure is similarly contained; a poisoned physical commit stops the owner.
Structural adoption marks the SQL snapshot unvalidated, including when the txid
already matches, so maintenance can neither consult nor bypass the owner's
Environment. History records both the client commit and the optional next-txid
maintenance commit, and maintenance uses the owner's current durability option.

### Internal streamed backup

`Database.backup(_:form:)` is internal tooling surface until the shared API is
named. It captures ordinary read admission plus the matching resident selected
meta; a publication racing capture retries at most 1,000 times. This retains the
exact generation even if relaxed commits overwrite its physical meta slot.
The pin remains alive through destination sync and no-clobber rename. Every
emitted page and both sides of publication check its role epoch; failure removes
the owned temporary/output file.

Page-identical backup validates and copies catalog, B+tree, GiST, overflow and
free-list pages at their original addresses, checks exact live count, zeros every
other body index, and emits fresh bootstrap meta in both slots. Compacted backup
uses the ordinary bounded `DomainImage` sink at pinned txid + 1. Both encrypted
forms read the currently selected key-block pair through the pinned pager and
write that copy twice, so a credential rewrap since client open is respected.
Payload residency is bounded by pages and existing catalog/tree metadata; no
whole serialized image is retained. Byte-level host tests cover all 87 ordinary
frozen images, pin persistence across relaxed commits, key rewrap, role failure,
and native publication cleanup. These are not canonical process-corpus results.

### Held-key promotion error classification

Only an automatic promoted owner opts into Pager's held-session-key mode. It
keeps its DEK across credential rewrap, adopts the selected key-block copy, and
leaves differing valid key copies unrepaired. A valid plaintext replacement or
neither structurally valid meta authenticating the held key is `28P04`. If an
older valid slot authenticates, the selected newer slot's auth failure stays
`XX001`; recovery still does not fall back on authentication failure. Corrupt
metadata remains `XX001`, and physical read failures during classification
propagate `58030`. Ordinary maintenance-held codec reopen retains corruption
classification. Four host/byte tests cover rewrap, divergent key copies,
rekey/decryption, retained older authentication, and a classification read fault.

### Frozen process actor and explicit runner

`jed-process` is an opt-in SwiftPM executable using public native Database,
Session and Rows operations. A small `ExperimentalTools` SPI exposes the
unratified backup operation and process-test role/page-count/ack-fault hooks;
it contains no SQL evaluator. The actor implements the canonical line protocol,
16 MiB bounded UTF-8 input, hex SQL, real read/write session lifetimes, explicit
cursor draining, maintenance, backup and owner-idle setup. Unimplemented stream
and attachment operations return counted `0A000` failures.

`mise run swift-native:process` builds the actor and runs the canonical process
driver against all 31 scenarios copied verbatim from the baseline by mechanical
codegen. `--no-build` uses an already-built actor and avoids SwiftPM contention.
`JED_PROCESS_FILTER`, `JED_PROCESS_CORES` and `JED_PROCESS_JOBS` remain explicit
selection controls; the task defaults to Swift→Swift. Frozen `cores` lists
predate Swift, so this task enables strict actor selection and never silently
substitutes a supported SQL engine. Frozen golden paths point to the same pinned
byte resources used by unit fixtures. Default driver core lists, corpus paths
and behavior are unchanged when the opt-in environment is absent. No process
scenario pass is claimed merely from compiling the actor. Timed helper commands
currently use bound query/execute operations; performance acceptance still
requires the full prepared-statement benchmark path and recorded measurements.
