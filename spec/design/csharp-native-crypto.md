# C# experiment cryptographic provider audit

Status: **provider approved; experimental encrypted file and page-service integration verified on
Linux x86-64; acceptance and security review incomplete** (2026-11-12).
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

## Storage and host integration evidence

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

## Remaining acceptance gates

Public credential-change orchestration, data-key rotation, compaction and backup/restore workflows,
encrypted scratch storage, relaxed durability batches, the complete shared process suite and
simulation/fault-injection coverage remain incomplete. Existing key-wrap/rewrap vectors prove the
codec, not a durable public credential-change operation. Selected recovery and acknowledgement tests
do not substitute for the full crash matrix. Full SQL parity and performance measurements remain
separate experiment gates, even though the current host executable passes NativeAOT.

Secret erasure and constant-time behavior still require explicit security analysis. Owned temporary
secrets are cleared on cleanup paths, but managed copies, provider scratch state and exceptional
provider exits prevent a complete erasure claim. Serial Argon2 lanes and safe API use do not establish
constant-time behavior. Other operating systems and architectures remain unverified.
