# Approved third-party dependencies

The ledger of dependencies the maintainer has approved under the policy in CLAUDE.md §14. Every
version is an exact pin. An approval covers only the listed packages and purpose; nothing else
inherits it. Adding or changing an entry needs explicit human confirmation first.

## Engine cores

### Digests — 2026-10-13, clause 3 (cryptography)

For `md5` / `sha224` / `sha256` / `sha384` / `sha512` ([string-functions.md](string-functions.md)).

| Core | Dependency |
|---|---|
| Go | standard library (`crypto/md5`, `crypto/sha256`, `crypto/sha512`) |
| Rust | RustCrypto `md-5 = 0.11.0`, `sha2 = 0.11.0` |
| TS | `@noble/hashes = 2.4.0` — the TS core's first runtime dependency; pure JS, no transitive dependencies, synchronous in Node and the browser |

`sha2`'s SHA-NI/SIMD backends use `unsafe` intrinsics chosen at runtime by `cpufeatures` (the
portable fallback is safe Rust). That use was accepted knowingly; nothing else inherits it.

### Encryption at rest — 2026-10-20, clause 3

For the encrypted file variant ([encryption.md](encryption.md),
[../fileformat/encrypted.md](../fileformat/encrypted.md)).

| Core | Dependency |
|---|---|
| Rust | RustCrypto `aes = 0.9.3` (raw block under the XAES-256-GCM derivation), `aes-gcm = 0.11.1` (default features off), `hkdf = 0.13.0` (with `hmac = 0.13.0` and the approved `sha2`), `argon2 = 0.6.0` (default features off) |
| Go | standard library (`crypto/aes`, `crypto/cipher`, `crypto/hkdf`) plus `golang.org/x/crypto v0.58.0` for `argon2` only — the Go core's first module dependency, pure Go |
| TS | `node:crypto` in a Node-only module; WebCrypto in the browser; the approved `@noble/hashes` for Argon2id |

The XAES-256-GCM derivation glue over library AES is hand-written per core against the C2SP
vectors — admitted as glue, not a primitive. The RustCrypto AES/GHASH backends' reviewed intrinsics
are accepted as for `sha2`.

### Node file-lock host — 2026-07-16

`impl/ts/native-lock` may use the safe-Rust Node-API graph `napi = 3.10.5`, `napi-derive = 3.5.10`,
and build-only `napi-build = 2.3.2`, solely to expose whole-file try-lock/unlock, which Node lacks.
It performs no database I/O or SQL work, keeps the TS engine independent, and is excluded from
browser bundles ([locking.md](locking.md)). No broader native surface inherits this.

## Benchmarks

- **AEAD experiment** — 2026-10-17, extended 2026-10-19: `bench/aead/` only, for measurement;
  selects no production cipher. Versions and methodology: [bench/aead/README.md](../../bench/aead/README.md).
- Other `bench/` driver dependencies: [benchmarks.md](benchmarks.md) §7.

## Frozen experiments

Each experiment's approvals (cryptography only; none authorizes a native engine component) are
recorded in its own docs:

- Ruby — OpenSSL digests/AES/HKDF, stdlib Zlib CRC32, `argon2-kdf`:
  [ruby-native-experiment.md](ruby-native-experiment.md)
- Swift — `swift-crypto` + CryptoExtras, pinned reference Argon2:
  [swift-native-crypto.md](swift-native-crypto.md)
- C# — `BouncyCastle.Cryptography = 2.7.0` for SHA-224 and Argon2id only:
  [csharp-native-crypto.md](csharp-native-crypto.md)
- Java — `org.bouncycastle:bcprov-jdk18on:1.86` for Argon2id only; JDK FFM for host OS calls:
  [java-native-experiment.md](java-native-experiment.md)
