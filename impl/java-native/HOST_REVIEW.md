# Host and cryptography boundary review

Reviewed locally on 2026-11-14 against the pinned binary/source artifacts in
`toolchains.json`. This records a scoped implementation review and test evidence,
not an independent security audit or a production certification.

## Dependency scope and resource admission

The only external engine dependency is `bcprov-jdk18on` 1.86. Production imports
are confined to `Crypto.java`: `Argon2BytesGenerator` and `Argon2Parameters`.
The JAR is never registered as a JCA provider. Its source JAR SHA-256 is
`34a8b7dcb030a08ddd5356d3d5be962f9299ca5275e1460273186f8fdbe44c34`;
the binary SHA-256 is
`2af190b300cbb0b35e248ccf5f4a06b6072030aeb3da7a98ec73abe5b4cb371f`.

The reviewed Argon2 path uses Java arrays, BLAKE2b and checked utility packing;
it introduces no native engine, C library or FFI. Parameters explicitly select
Argon2id version 0x13 and a 32-byte result. The byte-array API avoids a provider
character-conversion default. Jed rejects malformed UTF-16 before UTF-8 passphrase
encoding. Neither SQL nor SQL host functions obtain filesystem/network access
through this crypto integration.

`Credential.ArgonParameters` bounds lanes and iterations to 1..16, and memory to
`8 * lanes .. 1,048,576` KiB. `KeyBlock.valid` applies the same bounds to untrusted
persisted u32 fields before deriving a key. These match the shared file contract;
the upper bound can still require substantial heap. It is not a JVM heap cap.
Key-block CRC/shape checks and newest-valid-generation selection precede unlock;
a wrong current credential never falls back to an older credential generation.

The provider's default block pool clears returned blocks on normal completion.
Its `generateBytes` cleanup is not in a `finally`, so exceptional termination
must not be described as guaranteed secret erasure. Its memory parameter builder
also consults Bouncy Castle's host-configurable maximum-memory property; an
application that restricts this below jed's admitted parameters may prevent
credential derivation. This is host configuration, not a SQL setting.

## Primitives, timing and secret lifetime

JDK providers supply MD5, SHA-224/256/384/512, HKDF-SHA256, AES block encryption
and AES-GCM. XAES implements the shared derivation glue over AES. Authentication
is completed before decoded page bytes are admitted. Pointer-tag comparisons
use `MessageDigest.isEqual`, and key blocks use GCM authentication.

XAES doubling uses a mask rather than a secret-dependent branch. Argon2id's
later memory accesses are data-dependent by design; it is not a constant-time
password computation. JCA/JIT/native provider timing is outside the Java language
guarantees. This review makes no whole-engine constant-time claim.

Key wrapping wipes its temporary KEK in `finally`. XAES now also wipes the
temporary AES-derived subkey on both successful and failed operations and the
initial AES block after deriving its doubled block. Credentials, active data
keys and provider copies still live in managed memory. String passphrases,
moving GC, JIT elimination and provider-internal copies prevent guaranteed
erasure; no API or documentation promises it.

The existing host suite passes after the wiping change at source
`9120893e6701317b857fb718f86513c49495e7dae7273dbee5bb277a5c6ae13d`,
actor `stable-20261114-1785002-sfwaam`, log
`.dev/java-native/crypto-review-host.log`. It includes shared HKDF, Argon2id,
XAES and key-block vectors, 309 exact encrypted body pages across six fixtures,
wrong-tag/tampered-meta rejection, credential changes and recovery tests.

## Linux native seam

`host/LinuxNative.java` contains the explicit libc downcalls: `open`, `close`,
nonblocking `flock`, directory `fsync`, and `renameat2(RENAME_NOREPLACE)`.
It accepts host paths and owned descriptors, not pointers to engine values.
Each call uses a confined arena and captured `errno`; no native segment outlives
its call. OS/architecture checks constrain this ABI to Linux x86-64.

Lock descriptors are synchronized, close-on-exec and opened without following
the final path symlink. Closing clears Java ownership before calling libc and
does not retry Linux `close` after EINTR, avoiding descriptor-reuse races.
Lock acquisition and fsync retry EINTR; lock contention reports EAGAIN to the
host policy. Unsupported host operations fail closed. Page-service safety uses
the stable crash-clean lock bundle, never a PID file or expiry lease. Normal
owner foreground work does not reacquire the bundle locks per transaction.

The FFM initializer is lazy, so memory databases do not require native access.
Native host calls require the launcher's explicit native-access configuration.
Java code does not use `Unsafe`. JDK file/socket/provider implementations may
contain native code; those standard host internals do not implement SQL logic.
Host checks cover lock exclusion, descriptor lifecycle and atomic publication;
mixed-process and deterministic fault evidence is tracked separately in
`VERIFICATION.md`.
