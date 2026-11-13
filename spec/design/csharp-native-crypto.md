# C# experiment cryptographic provider audit

Status: **provider approved; experimental encrypted file and page-service integration verified on
Linux x86-64; scoped implementation review complete, final simulation acceptance in progress** (2026-11-13).
This records the implemented boundary and its evidence, not production approval or full-core parity.
Swift's swift-crypto/reference-Argon2 approval does not authorize a C# package.

| Required operation | Implemented boundary | Current evidence |
|---|---|---|
| MD5, SHA-256/384/512 | System.Security.Cryptography | Shared digest vectors pass JIT and NativeAOT |
| SHA-224 | BouncyCastle.Cryptography 2.7.0 (pinned) | Shared digest vectors pass JIT and NativeAOT |
| Raw AES-256 blocks and AES-256-GCM | BCL AES and AesGcm | Shared XAES/key-wrap/page fixtures and authentication rejection pass |
| HKDF-SHA-256 | BCL HKDF | Shared derivation fixtures pass |
| Argon2id | BouncyCastle.Cryptography 2.7.0 (pinned) | Exact parameters and scalar/AVX2 vectors pass; erasure limits remain |
| XAES derivation | Handwritten specified glue over provider AES | C2SP/shared vectors and exact encrypted page resealing pass; no new primitive |
| Entropy | Separate injected key and coordination seams | Exact encrypted-create draw order and shared seeded codec bytes pass |

Microsoft's [cross-platform crypto matrix](https://learn.microsoft.com/en-us/dotnet/standard/security/cross-platform-cryptography)
documents OS-backed implementations and platform differences. Do not infer a
portable SHA-224 or Argon2id API from the presence of other SHA-2 or KDF APIs.
Prefer BCL implementations where they satisfy the existing contract; close gaps
with vetted libraries, not handwritten cryptographic primitives. A candidate must
also work under the planned NativeAOT host. New packages, including test/benchmark
packages, require human approval under CLAUDE.md §14 before adding references.

## Approved dependency boundary

The maintainer approved exact-pinned **BouncyCastle.Cryptography 2.7.0**, exclusively for SHA-224
and Argon2id plus their internal helpers. This falls under CLAUDE.md §14 clause 3
(vetted cryptography). The [official package](https://www.nuget.org/packages/BouncyCastle.Cryptography/2.7.0)
has no transitive NuGet dependencies and ships a net6.0 assembly usable from net11.0.
Its license is MIT, with bundled notices retained. Other package functionality is
outside the approved boundary. Keep AES/GCM, HKDF and other required digests in
System.Security.Cryptography. Do not add a test or benchmark framework package.

The source inventory for upstream `release-2.7.0` resolved to
`4007498b13582d90ee1eda5d9920c324428b98b3` during this audit. Inspected paths under
`crypto/src/crypto/` were `digests/Sha224Digest.cs`, `digests/Blake2bDigest.cs`,
`digests/Blake2b_X86.cs` and `generators/Argon2BytesGenerator.cs`. These establish
the following integration requirements, not an audit of every reachable helper:

- SHA-224 uses managed digest state. Argon2 uses managed blocks and BLAKE2b;
  BLAKE2b can select the package's AVX2 backend, with a managed scalar fallback.
  Review and test both paths. The approval includes these bounded crypto
  helpers and intrinsics, never unsafe engine memory access or native engine code.
- Construct the Argon2 generator without its optional TaskFactory. Lanes remain
  a cryptographic parameter; the serial lane schedule avoids its internal
  Task.WaitAll path. CPU-bound derivation does not require a Task.Run wrapper.
- Pass explicit UTF-8 bytes and all file-format parameters: Argon2id, version
  0x13, memory KiB, passes, lanes, no secret/associated data and 32-byte output.
  Validate the format's parameter bounds before the provider allocates memory.
- Its normal completion clears the main block array, but this is insufficient
  evidence for complete secret erasure, especially on exceptions. Clear owned
  secrets in finally paths and document provider-managed retention limits.
- Directly instantiate the two algorithms. Package framework compatibility does
  not prove trimming or NativeAOT correctness. Require clean publishes and shared
  vectors under JIT and NativeAOT before admitting the provider into file storage.

The package is pinned with committed normal/AOT NuGet lockfiles and feed source
mapping. Its signed nupkg archive SHA-512 is
`2f2b3e749b61a43f2ab8e35a024455f647bc0e0113d02c015bdd4d7898cc547646bd144d3bc0e475179815f08138272f329cc9782dc3b8bae946e9ed627abd23`.
NuGet's normalized content hash is recorded separately in the lockfile. Provider
vectors passed JIT and clean NativeAOT builds with scalar and AVX2 paths. All six
shared encrypted files (309 body pages) decrypt and reseal exactly, including
meta and key-block handling. A substitute provider or extra package requires another
approval; ordinary implementation choices do not.

## Historical storage and host integration evidence

The following records preserve their original source hashes. Later complete acceptance
results in the current review section supersede the dated coverage gaps; they do not
retroactively attribute later code to an earlier run.

The experimental file API creates and opens encrypted databases with a host key or passphrase
credential. The page codec is integrated through catalog chains, table and index root pointers,
interior children, overflow chains, the free list and validated-COW publication. The implementation
authenticates bound pointers on cache hits as well as device reads. Key-block selection precedes
unwrap; authentication failure on the selected complete meta does not fall back to another
generation. Writable open repairs unequal key-block copies. Held data keys survive caller credential
disposal and role promotion.

Host checks cover credential applicability and errors, exact create entropy order, durable encrypted
reopen, split-tree commits and independent recovery, cached-tag rejection, torn-meta fallback, and
the prohibition on authentication fallback. The shared overflow fixtures additionally check exact
LZ4/disposition bytes, encrypted successor tags, fresh chains, unchanged-chain retention through
re-keying a SQL row, exact live-page counts, and malformed chains. Re-keying a row here means changing
its SQL primary key, not rotating the database encryption key.

The real-process interoperability task uses the Go test actor from semantic baseline
`b1a6affca7725e0b07a4f734eddb1c569ef76bcf`. Both owner directions pass encrypted SQL reads and writes,
including 6,000-byte incompressible overflow values and compressed values. The task also covers
plaintext ownership yield and read-only handoff in both directions, retained snapshots, refresh,
promotion, and lost/crashed-owner commit acknowledgements. These selected scenarios are not the
complete shared process or simulation suite.

Frozen source SHA-256
`8d5b2a68950da35d5b73653205a281f9afb30c0b1e17bd6574e1d9db6a047d8c`
passed the following with .NET SDK 11.0.100, target `net11.0`, Linux x86-64:

| Command | Result |
|---|---|
| `mise run csharp-native:test` | Full current host/invariant executable passes under JIT; AVX2 available |
| `mise run csharp-native:crypto --portable` | Provider, encrypted page fixtures and encrypted recovery pass with runtime intrinsics disabled; AVX2 unavailable |
| `mise run csharp-native:aot` | Clean NativeAOT publish and full host/invariant executable pass, SDK-default instruction set; AVX2 unavailable |
| `mise run csharp-native:aot --runtime-async --native-instructions` | Clean NativeAOT publish and full host/invariant executable pass, runtime-native async enabled and host-specific instruction set; AVX2 available |

The expanded real-process interchange task passed at source SHA-256
`e3140dd7f61f56a203f32e8125ea7fded2c8068e8862420a9d2a16cfd9950c6a`.
Do not attribute that run to the later frozen source digest. AVX2 availability documents the package
backend selection boundary; it does not assert that OS-backed AES uses no hardware acceleration in
the default-instruction builds. The host-specific AOT artifact is not a portable distribution.

The subsequent public `ChangeCredentialAsync` operation holds the owner role, derives the new
wrapper before attempting the nonwaiting writer gate, and performs stabilization, alternate-copy
write/barrier, then selected-copy write/barrier. I/O failure poisons the handle. Existing readers
retain the same data key. Its JIT host checks passed at source SHA-256
`94210e5d854c47b689ba6b84bd68192e95286c2be4823339916387875b5090ea`: host-key to
passphrase replacement and reopen, unchanged meta/data bytes, active-writer refusal, retained
reader, read-only preservation of divergent copies and writable repair, and injected failures before
and after all five storage stages. The durability model selects exactly the old or new credential
after each injected crash. This is a focused key-block failure model, not the complete engine crash
matrix. Real-process interoperability passed again at
`082bee42081c1b5388096a0cd5e64ca9f84ad982eff631cc247a9db3c4218ebe`, including a Go
reader surviving the C# owner's rewrap, a fresh Go client unlocking with the replacement credential,
and refusal of credential changes from a C# page client. These later additions have not yet repeated
NativeAOT verification.

The subsequent image/maintenance checks passed under JIT at source SHA-256
`a40fd3b49a7df7eb539504ed0b8ff0c9ff66bff830cd97a470bea2a48d26ad13` via
`mise run csharp-native:maintenance`: 28 complete byte-exact plaintext exports, memory/plaintext/
encrypted compaction with replacement-stage fault injection, and both backup forms across 13 shared
fixtures (including all six encrypted fixtures). Page-identical backup preserves ciphertext body
bytes; compacted backup reseals against destination page identities. The backup staging file uses
exclusive creation and cleanup only after this invocation has created it. Backup remains an internal
host-test API pending the public API contract. Deserialize rejects encrypted buffers; file restore
uses the ordinary authenticated file-open seam.

Real-process maintenance interchange passed under JIT at source SHA-256
`532b1bcb46c2ab40b999d7f2e63715e6f7419cfb0ee1cb5f450a79ac551162f5`: a retained Go
client blocks compaction while pinned, reloads after C# owner replacement, then commits through the
same connection; C# page-client copies in both backup forms reopen in Go with intact overflow values.
These cases run for plaintext and encrypted files. One pinned Go limitation is isolated explicitly:
when a Go page client appends to a root with inherited overflow references, its post-commit leaf
refault can poison its writer, so its next write reports `58030`. The same sequence reproduces with
two unmodified Go actors, without C#. The interoperability test closes that overflow-writing peer
and uses the separate healthy retained client for maintenance checks; it does not change protocol
pin requirements or claim the peer defect is fixed. These maintenance additions have not yet repeated
NativeAOT verification.

The full host suite passed under JIT at source SHA-256
`0a1948d5fc34077f3ed07274305ad295a35028a436d06f83c35d99b85402fb45`.
It includes immutable retained tree shapes (height, node count and range overlap after cache
eviction without page reads), COW/reopen shape preservation, and multi-domain prepare/stage
failure checks before publication. The scratch host now provides bounded memory-to-file migration
and per-file XAES protection using independent injected entropy. Its tests cover partial blocks,
truncate/regrow, replay/file/ordinal rejection, private creation permissions and cleanup. File-host
temporary databases use that spillable byte device. This verifies the scratch storage foundation;
SQL operator partition/spool integration and full `work_mem` parity remain separate work.
These additions have not repeated the frozen checkpoint's NativeAOT verification.

The native process adapter now executes the pinned 31-scenario process inventory with strict C#
actor identity (no fallback to a supported core). The second complete run passed 24 scenarios at
`58997c3fb90c4664f45e8cf084f7fe6a1d710a313af860cffc44e79f35a61cae`; subsequent
focused runs passed both stream scenarios and ownership-never at
`cba4e7dd312bd1ffe70993202af9726644ea3fc93247f148459c6cf7412a1a64`. The stream
helper relays page frames to the live owner or acquires protocol-v2 ownership as a yielding owner;
credentials travel only in the hello, are stripped before forwarding, and owned retained credential
copies are cleared when the remote handle closes. These runs include encrypted and plaintext
relay, owner exit, transport ownership, preferred-application handoff and client reconnection.

`mise run csharp-native:rekey` passed at source SHA-256
`91f4678cbd29bdee8d103d3ca278882ee840c9d1071aebc5f8c2ff2c25f17d81`, including the
complete byte-exact `encrypted_rekey_index_table.jed` golden from reverse-order source insertion.
Tables and B-tree/GIN records are reinserted in ascending key order into target-layout scratch trees;
every insertion settles into the spillable pager, whose residency follows the handle's `CacheBytes`.
GiST leaf keys use the canonical rebuild. The test forces scratch spill and covers encryption,
decryption, rotation, target storage admission, retained-reader refusal and pre-rename failure
recovery. The full encrypted-service process scenario also passes, including the idle client's
`28P04` after rotation. GiST's semantic builder still retains its leaf entries/node objects; the
scratch pager bound does not establish a whole-maintenance memory cap. These additions have not yet
repeated NativeAOT verification.

The internal filesystem adapter now awaits file creation/opening, rename, unlink, permissions and
directory barriers through the same engine paths used by native maintenance. The first simulated
host check passed at `6523fb837e05a09ae19bcb46dabf03cda5d34154827050943dc91cf17e10c168`:
content-keyed write-order independence, delayed removal of unissued writes, directory durability,
and twelve real-engine recovery histories with torn pending writes. That initial run covered the host model only. The later trace-driver evidence below adds structural
and differential checks; service scheduling and the complete fault matrix remain acceptance gates.

The remaining backup and storage-limit process layout checks passed at
`1d81da34b6931d8fda62b6aa616fa724296625928630426b30265ebc1dbc504c`, after periodic
owner reclamation, safe truncation, the created-handle reclamation seed and canonical single-leaf
resident-chain handling. These focused runs include exact generation, live-page, high-water and
free-page counts. Owner maintenance retains the writer lease after acknowledging a client commit;
the old write pin is released before reclamation, and lost acknowledgements still take this cleanup
path after successful publication.

The full host suite passed at
`c696f1094f649a3a566b882aa6b85203a46445f02ba40c80c57625a814cbd5f9`, including
nullable command-result counts, deferred writable stabilization, key maintenance, page service,
large-value storage, scratch and simulation host invariants. A later full host run at
`733d94bb574880c35a78af040efc57d4003e05e431cf9170bd39d796d9a18e75` also covered the relaxed-batch allocator's cumulative manifests, reuse floor, deadline and
cap-failure rules. The actual owner/client host path then passed **all 31 complete pinned process
scenarios** at `ae6fe58fb541f58e21c35dc3e5e11df87b6db4cb61b4811742388d3f3fc1d8bf`.
That run includes mixed synchronous/relaxed clients, ownership transfer, encrypted maintenance,
backup and exact storage accounting. Its captured actor SHA-256 was
`6a7dec65ead35fa3b0336f1d07694ad803096e31821a933560e5857abc0eb65d`; engine SHA-256 was
`3b8b729033d2e8864bcc34cb1a7b91f59691d16ae626b907710651ee2e70b972`.

The native simulator now executes complete single-process traces twice and checks identical
transcripts and byte-device operations, live/reopened reachability, exact live counts, key atomicity,
reader snapshots and recovery history. Five complete plaintext regressions matched the pinned Go
transcript exactly at `6f310d2ae3868c77e2dd7b17dc1a8128841a175a7d1667a8c15a9231f51e1788`:
`analyze-then-drop-table`, `compaction-crash-before-dir-sync`, `delete-empty-unary-subtree`,
`delete-merge-separator-overflow` and `stale-root-leaf-chains`. The complete
`relaxed-close-truncation-floor` trace matched exactly at
`e092f119fda7fcdb0e874692373a7050b9123c12b81b1b94c8f2a72c76df4f41`. At that same revision,
the host-clock test checked one read per relaxed commit, microsecond precision at a millisecond
interval boundary, flush-before-planning and close durability. The simulator disables the idle
flusher as required by its deterministic clock contract. The complete 402-step encrypted rekey trace
executes and passes internal oracles, but its peer comparison still exposes a reclamation-history
mismatch; it is **not** a passing differential trace.

The corrected disk corpus harness reopens the file and session before every eligible record, honors
explicit mode exclusions and opens fixture copies as real files. At `ae6fe58f…`, its first full run
selected 763 files: 543 passed (15,246 records), 134 failed and 86 had declared disk exclusions.
The encrypted variant passed row/type/error checks for 610 files (16,845 records), failed 66 and
excluded 87. It emits the canonical cost/EXPLAIN transcript instead of comparing those fields with
plaintext expectations. A subsequent independent pinned Go run passed all 676 eligible encrypted
files and produced the comparison transcript. Of C#'s 610 complete row/type/error-passing files,
542 also matched the full cost/EXPLAIN transcript exactly (1,568 events); 68 had differences.
Consequently **542 complete encrypted files passed every checked field**, with 134 total file
failures and 87 declared exclusions. This is partial coverage, not full encrypted conformance.
The comparison is reproducible with `csharp-native:conformance-peer`; its diagnostic log is
`/tmp/jed-csharp-encrypted-peer-first.log`. Logs for the underlying C# runs are
`/tmp/jed-csharp-process-all-latest.log`, `/tmp/jed-csharp-full-disk-first.log` and
`/tmp/jed-csharp-full-encrypted-first.log`; rerun through the corresponding opt-in mise tasks.

## Later integration evidence

The full host suite passed again at
`3b092099519185655eac42c9e9527bcc05cdc50e9f9b77854d9bbc2338afdfa9`,
including deterministic stream-backed page-service ownership, real independent client
engines, disconnect/reconnect, owner death and client promotion. The complete encrypted
rekey regression now matches the pinned Go transcript exactly through all 403 steps
(indices 0–402): SQL costs, rows/errors, credential events, recovered generations,
physical image hashes and live/page/free counts. The earlier ANALYZE and ALTER cost
mismatches were duplicated overflow-read charges and are fixed. The diagnostic log is
`/tmp/jed-csharp-sim-encrypted-complete-final.log`.

All three complete relaxed attachment regressions also match the pinned peer exactly:
`relaxed-attachments-close-order`, `relaxed-floor-unsynced-compaction-rename` and
`relaxed-deadline-flush`. Their checks cover independent domain histories and durability
floors, named attachment close order, deadline rollover, and rename recovery. The first
two passed at `64a2f99b975f6c6ce6497ad72aa78f2bfd62b88560c68924da7ffcf98ee1dfef`;
the deadline trace passed at
`ba940a7942942b8ae7d4ba5ea9a95c9c9e17565e32ed3fb9ee311286f83107bd`.

The injected service adapter uses the production protocol-v2 server, client and role
driver over deterministic byte streams. Complete exact peer passes include
`encryption-service-reconnect-keeps-held-key`, `encryption-service-promote-after-rekey`,
and `encryption-service-backup-current-key-block`: held-key reconnection, stale-key
promotion rejection, and byte-identical backup using the owner's current key block.
The relaxed `service-backup-after-owner-lost` trace also passes exactly, including
recovery below the previous visible generation while respecting the durable floor.
These passes do not establish the full service scheduling/fault matrix. Larger service
and attachment traces, corruption scenarios and every crash profile remain in progress.

At `8ab08af3506006c732f6a9482270c307b3d073051aab1eeb903b297c8396e5ff`,
the complete 304-step `service-owner-and-clients` trace matches the pinned Go transcript
exactly, including owner maintenance generations, frame disconnects, promotion, physical
image hashes and SQL costs. `service-owner-lost-unstabilized-generation` also passes
exactly: an adopted generation can be lost back to the durable floor when its next
stabilization barrier fails. Logs are `/tmp/jed-csharp-sim-service-owner-final.log` and
`/tmp/jed-csharp-sim-service-unstable-first.log`. Full host tests subsequently pass at
`18fab9fa08242d9375cf510a5fe94e5f561ba5ef0f32a7d56ed8c312b9a1df2f`,
including failed promotion cleanup: a stale held key releases ownership on each failed
attempt, allowing a fresh credential to become owner. These checks still leave the
larger service-backup and service-attachment histories open.

Full SQL parity, complete operator spill integration, current full-engine NativeAOT
verification and performance measurements remain separate experiment gates; the earlier
frozen checkpoint's NativeAOT pass does not cover these later changes.

Secret erasure and constant-time behavior still require explicit security analysis. Owned temporary
secrets are cleared on cleanup paths, but managed copies, provider scratch state and exceptional
provider exits prevent a complete erasure claim. Serial Argon2 lanes and safe API use do not establish
constant-time behavior. Other operating systems and architectures remain unverified.

## Current scoped security review and acceptance boundary

The implementation review covers `CryptoProvider.cs`, `StorageCryptoKeys.cs`,
`StorageCryptoPages.cs`, `StorageCryptoRecovery.cs`, encrypted scratch, credential
maintenance, recovery and the injected entropy/environment ownership paths. It finds:

- AES block operations, GCM and HKDF remain behind BCL safe APIs. The only package
  calls are SHA-224 and Argon2id. XAES is the specified derivation and framing glue;
  there is no handwritten block cipher, authentication primitive or unsafe engine code.
- Bound authentication tags use `CryptographicOperations.FixedTimeEquals`, including
  cached-page admission. GCM tag failure returns no plaintext; temporary output is
  cleared on failure. Metadata authentication failure cannot silently select an older
  generation after a complete candidate was selected.
- Argon2id untrusted parameters are range-checked before provider allocation. No
  TaskFactory is supplied. The managed and AVX2 helper paths have identical shared
  vector evidence; that does not establish identical execution time.
- Owned credentials, data keys, derived keys and decrypt scratch are cleared in
  disposal/finally paths. Caller credentials are copied, and a handle's retained key
  lifetime is independent of the caller disposing its credential. Failed role
  promotion releases the exclusive claim and owned opening resources.
- The immutable `DatabaseEnvironment` separates SQL randomness from key entropy.
  Each open/create obtains its own key stream from the environment factory. The
  explicitly insecure seeded factory exists for reproducible fixtures only; default
  sources use operating-system randomness. Attachments and temp domains share the
  handle's environment. Scratch encryption has independently owned ephemeral keys.

This is a scoped source and failure-path review, not an independent cryptographic
certification. BCL/provider internals, managed copies, JIT/AOT register or stack
spills, and exceptional Argon2 provider exits prevent a complete secret-erasure
claim. Argon2id includes data-dependent accesses by design; safe APIs, serial lanes
and fixed-time tag comparison do not justify a blanket constant-time claim. No
whole-engine memory bound or production approval follows from this review.

The complete real-process matrix subsequently passes **496/496** ordered pairings
across C#, Go, Rust and Node, including plaintext/encrypted service and backup image
comparison. Threaded stress passes C# memory, plaintext and encrypted file hosts plus
Go with 16,000 reader checks and identical final checksum. Artifact hashes and logs
are retained in `impl/csharp-native/VERIFICATION.md`.

Current whole-engine JIT and NativeAOT host checks pass at the parent-coordinated
capture18, including the immutable environment and native host API. The same capture's
NativeAOT memory corpus passes all 761 eligible files (21,954 records; two declared
exclusions), and all 676 eligible encrypted files pass exact independent Go
cost/EXPLAIN transcript comparison (2,761 events). The experiment verification
record owns the full capture hash and commands. These results supersede earlier
statements that current NativeAOT or full encrypted SQL coverage had not been run.

Final deterministic simulation acceptance remains open: capture20 saved coverage is
19/19 complete exact traces. Generated seed-one coverage is 19/22 profiles
(16/19 exact and 3/3 intrinsic); the remaining encrypted attachment, replay and
service cases are under correction. Exact peer comparison and intrinsic per-core
fault oracles are reported separately under the canonical policy.
The tooling records and checks its own policy hashes so one accepted sweep cannot
silently span a policy change. Other operating systems and architectures remain
unverified; promotion and production delivery still require a separate decision.
