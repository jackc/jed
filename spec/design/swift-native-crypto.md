# Native Swift experiment: cryptographic integration

This is the dependency/API audit for the opt-in native Swift experiment, against
jed revision `362e0798ac3a8bd04bd2da556044b6ec73bf468a`. It does not change the
encrypted file suite or select a production Swift delivery model. The maintainer
approved Apple's swift-crypto, CryptoExtras when needed, the reference Argon2
implementation, and their necessary cryptographic backends on 2026-11-05. The
approval includes native cryptographic code; it does not extend to engine
algorithms or unrelated utilities.

## Selected upstream pins

| Source | Release | Commit | Purpose |
|---|---|---|---|
| [apple/swift-crypto](https://github.com/apple/swift-crypto/releases/tag/5.1.1) | `5.1.1` | `f8fad2f95ad67b45f6340c0960e2396094d67e07` | `Crypto` and `CryptoExtras` products |
| [apple/swift-asn1](https://github.com/apple/swift-asn1/releases/tag/1.7.4) | `1.7.4` | `917b472dbe53ba699cbb0495f9a95a084bc6364d` | CryptoExtras' required cryptographic key serialization dependency |
| [P-H-C/phc-winner-argon2](https://github.com/P-H-C/phc-winner-argon2/releases/tag/20190702) | `20190702` | `62358ba2123abd17fccf2a108a301d4b52c01a7c` | Vendored reference Argon2id, version `0x13` |

The first two are exact SwiftPM versions, with their resolved revisions retained
in `Package.resolved`. Pinning swift-asn1 explicitly prevents the upstream
`from: "1.2.0"` constraint from selecting a different dependency on a fresh
resolution. The engine does not import or use its ASN.1 parser. It is required
by the approved CryptoExtras product; it supplies no SQL or storage behavior.
The [swift-asn1 manifest](https://github.com/apple/swift-asn1/blob/1.7.4/Package.swift)
declares no further packages.

The [swift-crypto manifest](https://github.com/apple/swift-crypto/blob/5.1.1/Package.swift)
requires Swift tools 6.2 and vendors BoringSSL at
`0226f30467f540a3f62ef48d453f93927da199b6`, plus XKCP. These are parts of the pinned
package, not system-library prerequisites. CryptoExtras depends on BoringSSL on
every platform. `Crypto` delegates to CryptoKit where available and uses the
vendored implementations on Linux. There is no OpenSSL package substitution or
system `libcrypto` dependency. The [upstream implementation
description](https://github.com/apple/swift-crypto/blob/5.1.1/README.md#implementation)
explains this platform split. Jed only uses the algorithms listed below; the
dependency's additional algorithms do not become SQL functions.

Reference Argon2 is packaged as a local C target built from pinned upstream
sources, with upstream license notices, source provenance, and file checksums.
Use the portable `ref.c` path initially: `argon2.c`, `core.c`, `encoding.c`,
`ref.c`, and `blake2/blake2b.c`, with their upstream headers. Define
`ARGON2_NO_THREADS`; it preserves the requested lane count while evaluating lanes
serially, and avoids a separate thread host. Upstream's
[build definition](https://github.com/P-H-C/phc-winner-argon2/blob/20190702/Makefile)
supports this configuration. No `-march=native`, dynamic system Argon2 lookup,
or altered cryptographic source is required.

## API map and boundaries

| jed requirement | Selected API | Contract |
|---|---|---|
| SQL `md5` | `Insecure.MD5.hash(data:)` in `Crypto` | 16 digest bytes, rendered as lowercase hexadecimal by Swift |
| SQL `sha256`, `sha384`, `sha512` | `SHA256.hash(data:)`, `SHA384.hash(data:)`, `SHA512.hash(data:)` | Raw digest bytes, not description strings |
| SQL `sha224` | Bundled backend `CCryptoBoringSSL_SHA224` | Exactly 28 output bytes; narrow backend bridge described below |
| XAES block encryption | `AES.permute(&block, key:)` from `CryptoExtras` | Exactly 16 bytes per block and a 32-byte key |
| AES-256-GCM | `AES.GCM.seal(_:using:nonce:authenticating:)`, `AES.GCM.open(_:using:authenticating:)` | Explicit 12-byte nonce, 16-byte tag, authenticated data unchanged |
| HKDF-SHA-256 | `HKDF<SHA256>.deriveKey(inputKeyMaterial:salt:info:outputByteCount:)` | 32 output bytes; format-defined salt and info |
| Passphrase KEK | `argon2id_hash_raw(t, m, p, pwd, pwdlen, salt, saltlen, out, 32)` | Argon2id `0x13`, no secret or associated data |

The public Swift digest definitions are in
[Insecure_HashFunctions.swift](https://github.com/apple/swift-crypto/blob/5.1.1/Sources/Crypto/Insecure/Insecure_HashFunctions.swift)
and [HashFunctions_SHA2.swift](https://github.com/apple/swift-crypto/blob/5.1.1/Sources/Crypto/Digests/HashFunctions_SHA2.swift).
The [block API](https://github.com/apple/swift-crypto/blob/5.1.1/Sources/CryptoExtras/AES/Block%20Function.swift)
checks block and key sizes and invokes bundled AES; it does not pad or supply a
cipher mode. This is the raw AES operation required by XAES without CommonCrypto
or a new primitive.

[AES-GCM.swift](https://github.com/apple/swift-crypto/blob/5.1.1/Sources/Crypto/AEADs/AES/GCM/AES-GCM.swift)
exposes ciphertext and tag separately. Its `combined` representation includes
the nonce; it is **not** jed's ciphertext-and-tag encoding. Always provide the
nonce from jed's dedicated key-entropy source. Never use an API overload that
chooses its own random nonce. Authentication failure is mapped at the file-format
boundary: key unwrap to `28P01`, a body/meta page to `XX001`, and a held page-service
key invalidated by rotation to `28P04` where specified.

[HKDF.swift](https://github.com/apple/swift-crypto/blob/5.1.1/Sources/Crypto/Key%20Derivation/HKDF.swift)
provides the full extract-and-expand operation. The page and meta subkeys use
empty salt and the exact UTF-8 info bytes from [encrypted.md](../fileformat/encrypted.md)
§3; the host KEK uses the key-block salt and `jed/v1/kek`. Credentials are bytes;
no normalization, locale conversion, or C-string termination is applied.

### SHA-224 is a backend bridge, not a public Swift digest

The selected release has no public `SHA224` digest in `Crypto` or `CryptoExtras`.
Its PBKDF2 `insecureSHA224` selector is a KDF choice and cannot supply the SQL
digest. Truncating SHA-256 would be incorrect.

The approved bundled BoringSSL backend does expose SHA-224 in its
[public C header](https://github.com/apple/swift-crypto/blob/5.1.1/Sources/CCryptoBoringSSL/include/CCryptoBoringSSL_sha2.h).
The package's [symbol prefix map](https://github.com/apple/swift-crypto/blob/5.1.1/Sources/CCryptoBoringSSL/include/CCryptoBoringSSL_boringssl_prefix_symbols.h)
renames it to `CCryptoBoringSSL_SHA224`. A dedicated Swift adapter can call this
function through `CCryptoBoringSSL`, with a scoped immutable input buffer and a
28-byte output buffer. This accesses the explicitly approved crypto backend,
not a second crypto implementation. `CCryptoBoringSSL` is not a public SwiftPM
product: successful transitive module import and static linkage must be proven
with the exact selected release; the Linux compile/link and digest execution
checks below establish that proof on the inspected host. If another host rejects
it, an audited narrow C shim
to the same exported, pinned symbol is the fallback; no engine FFI or handwritten
hash implementation is admitted. Do not describe this bridge as a stable upstream
Swift API.

### Argon2 and pointer lifetime

The [upstream header](https://github.com/P-H-C/phc-winner-argon2/blob/20190702/include/argon2.h)
declares `argon2id_hash_raw` and fixes `ARGON2_VERSION_NUMBER` to `0x13`.
The [implementation](https://github.com/P-H-C/phc-winner-argon2/blob/20190702/src/argon2.c)
sets secret/associated-data lengths to zero, copies the raw result, and returns
an error code. Lengths are explicit; empty and embedded-zero passphrases retain
their bytes.

Keep the C import in the crypto adapter. Validate the file-format bounds before
calling it: `1 ≤ p ≤ 16`, `1 ≤ t ≤ 16`, `8p ≤ m ≤ 1048576` KiB, fixed key-block
salt length and output length. Check any `Int`/`size_t`/`UInt32` conversions before
conversion. A failed allocation or other nonzero return is an error, never a
partially initialized key. Borrow buffers only for the duration of the call;
return owned Swift bytes or a `SymmetricKey`, never pointers. No pointer escapes,
callback registration, or shared mutable context is needed.

Upstream [core.c](https://github.com/P-H-C/phc-winner-argon2/blob/20190702/src/core.c)
wipes its internal working memory by default; keep that behavior and never
mutate its global clearing flag. This does not guarantee erasure of all Swift
value copies or application-owned passphrases. Memory erasure and constant-time
behavior are distinct from memory safety. The experiment inherits the existing
encryption threat model and does not claim a new security review.

## Verification obligations and audit status

On 2026-11-05, inspection confirmed the host toolchain is Swift 6.4.2 targeting
`x86_64-unknown-linux-gnu`. The opt-in integrated SwiftPM test run compiled and
linked the pinned dependencies and passed all six `CryptoTests`, including the
transitive `CCryptoBoringSSL` SHA-224 import and digest vectors. Apple-platform
builds have not been performed. The direct swift-asn1 exact constraint emits an
unused direct dependency warning because the target consumes it transitively
through CryptoExtras; the constraint deliberately pins that required dependency.

The primitive tests reproduce
[encryption_vectors.toml](../fileformat/encryption_vectors.toml): three XAES cases
(two C2SP cases and one page-associated-data case), one subkey fixture containing
two outputs, one host KEK, one key-block wrapping payload, one rewrap payload,
and one Argon2id case. The key-block codec tests additionally reproduce each
complete 128-byte key-block/rewrap record and exact seeded entropy draw order.
TOML field counts are not vector counts. XAES is only the already-specified doubling/XOR/two-block
derivation over library AES and AES-GCM. SQL digest cases come from the shared
corpus. Adapter tests cover invalid sizes, authentication failures, embedded
zero input, lifetime-safe empty input, and reference Argon2 with multiple lanes.
Storage integration additionally requires encrypted goldens, bidirectional
interchange, rekey/credential changes, scratch sealing, process tests and encrypted
simulations. Those later Linux gates are recorded below. Passing primitive vectors
alone does not establish file-format, durability, or security correctness.

No dependency approval blocker has been identified. The SHA-224 bridge's
compile/link and execution proof is established on the inspected Linux host.
The dated primitive results above establish cryptographic and page-codec
foundations; alone they do not establish database open/create, validated-COW
commits, credential repair or rotation.

### Subsequent Linux integration acceptance

The frozen snapshot at `cec8170bfc33f087fa98949ae661b1eaa5490218` passes all 515
native tests including all 749 frozen SQL files. Postoptimization verification
also passes the full 77,612-record four-core encrypted comparison, all 2,691
bidirectional interchange phases, 241,412 exact-account measurements and the
18-trace four-core saved-simulation comparison. Earlier complete service,
process and 176-trace-per-core generated-simulation gates retain their own actors.
[VERIFICATION.md](../../impl/swift-native/VERIFICATION.md) records commands, hashes,
credential/maintenance coverage, failures and later corrections. The measured
comparison is separate in [BENCHMARKS.md](../../impl/swift-native/BENCHMARKS.md).

These checks reproduce the implemented encryption contract; they do not certify
constant-time behavior, complete memory erasure, a new security review or Apple
support. Dependency pins and approved native boundaries above remain unchanged.
Swift remains opt-in, with production delivery and promotion undecided.
