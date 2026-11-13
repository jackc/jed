# C# experiment cryptographic provider audit

Status: **provider proposed; human approval pending** (2026-11-12).
This is an integration audit, not a completed security review or working encryption claim.
Swift's swift-crypto/reference-Argon2 approval does not authorize a C# package.

| Required operation | Candidate boundary | Remaining evidence |
|---|---|---|
| MD5, SHA-256/384/512 | System.Security.Cryptography | Shared digest vectors, platform availability |
| SHA-224 | Proposed BouncyCastle.Cryptography 2.7.0 | Approval, shared vectors, JIT/NativeAOT proof |
| Raw AES-256 blocks and AES-256-GCM | BCL AES and AesGcm | Required modes, nonces/tags, empty spans, authentication failure mapping |
| HKDF-SHA-256 | BCL HKDF | Shared derivation fixtures |
| Argon2id | Proposed BouncyCastle.Cryptography 2.7.0 | Approval, exact parameters/lanes, backend and erasure review, JIT/NativeAOT proof |
| XAES derivation | Handwritten specified glue over provider AES | Existing C2SP/shared vectors; no new primitive |
| Entropy | Injected host environment | Exact seeded draw order and failure behavior |

Microsoft's [cross-platform crypto matrix](https://learn.microsoft.com/en-us/dotnet/standard/security/cross-platform-cryptography)
documents OS-backed implementations and platform differences. Do not infer a
portable SHA-224 or Argon2id API from the presence of other SHA-2 or KDF APIs.
Prefer BCL implementations where they satisfy the existing contract; close gaps
with vetted libraries, not handwritten cryptographic primitives. A candidate must
also work under the planned NativeAOT host. New packages, including test/benchmark
packages, require human approval under CLAUDE.md §14 before adding references.

## Concrete dependency proposal

Approve exact-pinned **BouncyCastle.Cryptography 2.7.0**, exclusively for SHA-224
and Argon2id plus their internal helpers. This falls under CLAUDE.md §14 clause 3
(vetted cryptography). The [official package](https://www.nuget.org/packages/BouncyCastle.Cryptography/2.7.0)
has no transitive NuGet dependencies and ships a net6.0 assembly usable from net11.0.
Its license is MIT, with bundled notices retained. Other package functionality is
outside the proposed boundary. Keep AES/GCM, HKDF and other required digests in
System.Security.Cryptography. Do not add a test or benchmark framework package.

The source inventory for upstream `release-2.7.0` resolved to
`4007498b13582d90ee1eda5d9920c324428b98b3` during this audit. Inspected paths under
`crypto/src/crypto/` were `digests/Sha224Digest.cs`, `digests/Blake2bDigest.cs`,
`digests/Blake2b_X86.cs` and `generators/Argon2BytesGenerator.cs`. These establish
the following integration requirements, not an audit of every reachable helper:

- SHA-224 uses managed digest state. Argon2 uses managed blocks and BLAKE2b;
  BLAKE2b can select the package's AVX2 backend, with a managed scalar fallback.
  Review and test both paths. The proposed approval includes these bounded crypto
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

After approval, pin the package with a committed NuGet lockfile/content hash,
restrict the opt-in package feed, and record the actual restored artifact. Run
the existing encryption vectors and digest corpus, then plain/encrypted peer
interchange and failure/recovery checks as storage becomes available. No package
has been added by this proposal. A substitute provider or extra package requires
another approval; ordinary implementation choices do not.

Provider vectors alone are insufficient: encrypted page goldens, bidirectional
interchange, failed-commit recovery, credential changes, rotation, backup/restore,
scratch sealing, process tests and simulations remain separate gates. Secret
erasure and constant-time behavior need explicit analysis and must not be claimed
from ordinary memory-safe API usage.
