# C# experiment cryptographic provider audit

Status: **provider selection incomplete; no new dependency approved** (2026-11-12).
This is a prerequisite inventory, not a security review or working encryption claim.
Swift's swift-crypto/reference-Argon2 approval does not authorize a C# package.

| Required operation | Candidate boundary | Remaining evidence |
|---|---|---|
| MD5, SHA-256/384/512 | System.Security.Cryptography | Shared digest vectors, platform availability |
| SHA-224 | Reviewed external provider or narrowly reviewed OS adapter | Select provider, exact pin, license/backend review, approval |
| Raw AES-256 blocks and AES-256-GCM | BCL AES and AesGcm | Required modes, nonces/tags, empty spans, authentication failure mapping |
| HKDF-SHA-256 | BCL HKDF | Shared derivation fixtures |
| Argon2id | Reviewed external provider | Select provider, exact pin, parameters/lanes, backend review, approval |
| XAES derivation | Handwritten specified glue over provider AES | Existing C2SP/shared vectors; no new primitive |
| Entropy | Injected host environment | Exact seeded draw order and failure behavior |

Microsoft's [cross-platform crypto matrix](https://learn.microsoft.com/en-us/dotnet/standard/security/cross-platform-cryptography)
documents OS-backed implementations and platform differences. Do not infer a
portable SHA-224 or Argon2id API from the presence of other SHA-2 or KDF APIs.
Prefer BCL implementations where they satisfy the existing contract; close gaps
with vetted libraries, not handwritten cryptographic primitives. A candidate must
also work under the planned NativeAOT host. New packages, including test/benchmark
packages, require human approval under CLAUDE.md §14 before adding references.

Provider vectors alone are insufficient: encrypted page goldens, bidirectional
interchange, failed-commit recovery, credential changes, rotation, backup/restore,
scratch sealing, process tests and simulations remain separate gates. Secret
erasure and constant-time behavior need explicit analysis and must not be claimed
from ordinary memory-safe API usage.
