# Encryption at rest — design (a door, not yet built)

> How file-level encryption plays into the storage seam. This is a *design* doc for a
> **deferred, not-yet-built** capability (CLAUDE.md §9, storage.md §6, [TODO.md](../../TODO.md)
> "Maybes"). It records the intended approach so nearer-term work — the storage-host seam
> ([hosts.md](hosts.md)), the on-disk format ([../fileformat/format.md](../fileformat/format.md)),
> replication ([replication.md](replication.md)) — does not foreclose it, and so the eventual
> build is a known shape rather than a fresh investigation. **Building it requires a §14
> vetted-crypto-library decision and explicit confirmation before any dependency lands** — we do
> **not** roll our own crypto (CLAUDE.md §14). When a decision here changes, update
> [CLAUDE.md](../../CLAUDE.md) §9 and [storage.md](storage.md) §6 in the same edit.

## 1. What we are protecting, and the one present requirement

Encryption at rest protects a **quiescent database file** against an attacker who can read (or
tamper with) the bytes on disk but does not have the key. It is distinct from the two integrity
layers already present:

- The per-page **CRC** (`format_version` 7, format.md *Page header*) detects accidental bit-rot,
  but explicitly **not** tampering — "a malicious rewriter can recompute the CRC; that is the
  encryption-at-rest / authenticated-page door" (storage.md §6). Encryption is that door.
- The commit model protects the *commit boundary* (crash atomicity, storage.md §7); encryption
  protects the *bytes at rest*. Orthogonal.

**The only present requirement is non-foreclosure** (CLAUDE.md §9): the format and the seam must
not bake in an assumption that page bytes are plaintext-comparable on disk. That requirement is
**already met** — page roles are reached by following pointers, not by byte-comparing on-disk
content; and the page header already grew once (12→16 bytes for the CRC, format.md), establishing
that carrying a per-page authentication tag is within the format's existing flex. The rest of this
doc is the *intended* design, not a commitment to build.

### 1.1 Settled decisions (2026-10-19)

These bind the eventual build; everything else in this doc that says "open" still is.

- **No FIPS requirement.** jed does not target FIPS 140 deployments, so FIPS approval is not
  a criterion for the suite (§3.1). (It could not be met uniformly anyway: only Go ships a
  validated module; the RustCrypto and Noble backends are not validated.)
- **Usable page capacity excludes a reserved region; guarantees are stated in terms of it**
  (§4.1). Tree shape, record caps and cost are functions of the usable capacity `C`, not of
  whether the file is encrypted.
- **Every page type is authenticated**, including free-list and manifest-overflow pages, and
  the meta pages' security-relevant content (§4).
- **Scratch files of an encrypted database are encrypted** with a random per-file key that is
  never persisted (§4.2).
- **Key material and nonces come from a dedicated cryptographic entropy source**, never the
  general-purpose seeded entropy seam (§3.1, *Threat model*).
- **Browser performance is not a deciding factor for the suite.** Encrypted browser databases
  are expected to be uncommon.
- **Suite direction: AES-256-GCM with one key derived per page from a fresh random salt**, chosen
  for standard-library support across current and future native cores (§3.1). XChaCha20-Poly1305 is
  dropped. **XAES-256-GCM** (C2SP) is the primary candidate for the derivation, with a jed-specific
  HKDF-SHA-256 layout as the fallback if a backend finding rules XAES out (§3.1, *Construction*).
  AES-GCM-SIV is declined for platform support alone, a knowing trade (§3.1, *Why not
  AES-GCM-SIV*). This becomes a format decision after a security review and the fault fixtures.
- **One key per page (2026-10-20).** Every page encryption draws its own random salt (the
  XAES-256-GCM nonce, or the fallback layout's HKDF salt) and derives a single-use key;
  per-commit-attempt keys are not pursued (§3.1, *Key granularity*).

## 2. Where it sits: a page codec in the core, above the seam

Encryption is a **page codec layer inside the core, just above the block seam** (hosts.md §6) —
**not** a duty of each storage host:

```
core (pager: buffer pool, page math)
  ↓ plaintext page
encryption codec   ← HERE: encrypt on write / decrypt on read, per core
  ↓ ciphertext page
block seam (BlockStore) → [replication tee] → host (file | OPFS | memory)
```

Two reasons it belongs in the core and not the host:

1. **Cross-core byte-identity stays tractable (§3).** The §8 contract — every core writes
   byte-identical files — must survive encryption, or the golden round-trip breaks. That is far
   easier to pin with **one codec per core plus shared input→bytes fixtures** (the model
   [../fileformat/lz4.md](../fileformat/lz4.md) + `lz4_vectors.toml` set for compression) than
   with crypto buried in every host (Rust file, Go file, Node `fs`, OPFS — four+ places to keep
   byte-aligned).
2. **The host stays minimal and opaque.** The host's job is opaque growable bytes (hosts.md §2);
   adding "encrypt" to it would violate that minimalism and re-implement the codec per host. An
   opaque-byte host is *also* exactly the "don't assume plaintext on disk" requirement (§1).

The codec sits **above** the replication tee so the tee captures ciphertext — the keyless-replica
property (replication.md §4). It sits **below** the buffer pool so cached pages are held decoded
(plaintext) in RAM, and the cost model is unaffected (§5).

## 3. The cross-core byte-identity problem — and why crypto solves it where LZ4 didn't

The §8 goal is **byte-identical encrypted files for identical crypto inputs** across Rust, Go,
TS and the Ruby fixture generator. Compression could *not* meet this with a library — LZ4 *encoders* are not
standardized, so two libraries produce different (both-valid) compressed bytes, which is why the
codec is hand-rolled (large-values.md §6). **Encryption is the opposite case:**

- **Standardized AEADs agree byte-for-byte.** AES-256-GCM and ChaCha20-Poly1305 are specified
  down to the byte and validated against NIST/RFC test vectors *precisely so* all conforming
  implementations produce identical output for identical `(key, nonce, plaintext, AAD)`. So a
  **vetted library per core** (the §14 crypto exception) yields byte-identical ciphertext — the
  §8 contract holds **through** a dependency, exactly the clause-1 case in CLAUDE.md §14 ("all
  cores can be made to match"). This is the key asymmetry: we hand-roll LZ4 to *get*
  byte-identity; we use a library for AEAD *because* it already guarantees it (and §14 forbids
  hand-rolled crypto regardless).

- **Nonce uniqueness is an unresolved storage-protocol obligation.** `(page_index, txid)`
  alone is insufficient: an interrupted commit can leave ciphertext on disk without advancing
  the committed `txid`; a retry can write different plaintext at the same coordinates. Restoring
  a backup, independently writable copies, and compaction also need explicit treatment. A
  committed transaction counter is not a durable allocation of encryption nonces.
- **Security precedes unconditional byte identity.** Each core must produce the same ciphertext
  for identical key, nonce, plaintext and AAD. Key material (data keys and
  salts) comes from the dedicated cryptographic entropy source (§3.1), with fixed inputs in
  fixtures. Production files need not match when these inputs differ. §3.1 sets the direction:
  every key is derived from a fresh random salt, so nothing is allocated across commits and a
  restored or copied file draws new keys on its next write. (A random domain saved once in a
  file would be copied with that file and would not by itself solve writable forks.) Record
  any necessary determinism-ledger change before implementation; never trade nonce safety for
  identical files. Recovery, retry, copying, and backup tests must establish the chosen contract.

### 3.1 Suite direction: AES-256-GCM with per-page derived keys (not yet a format decision)

Prefer **one initial suite** across cores, with a versioned format for later replacement. A second
suite needs a measured platform limitation or concrete interoperability requirement.

**Direction (2026-10-19; construction revised 2026-10-20):** AES-256-GCM, where every page
encryption derives a single-use key from the data key and a **fresh random salt**, one key per
page (below). Nothing about key or nonce use is persisted or allocated across commits. The
derivation is **XAES-256-GCM** (C2SP) as the primary candidate, with a jed-specific HKDF-SHA-256
layout as the fallback (*Construction* below). This becomes a format decision only after a
security review of the exact construction and the fault fixtures below. XChaCha20-Poly1305 is no
longer a candidate. AES-GCM-SIV is not pursued either, for platform reasons alone (*Why not
AES-GCM-SIV* below): its failure behavior under repeated randomness is strictly better, and
choosing AES-GCM over it is a knowing trade.

Three findings settle the direction:

1. **Platform support decides it.** AES-256-GCM and HKDF are in every platform's standard
   library or platform crypto, usually hardware-accelerated. XChaCha20-Poly1305 is in none of
   them, so every native core but Rust and TS would need a third-party crypto dependency, and in
   .NET and Swift the mature options largely wrap libsodium (C) through native interop, against
   CLAUDE.md §13/§14. Building XChaCha from the 96-bit-nonce ChaCha20-Poly1305 needs HChaCha20,
   which no standard library exposes, so it would mean hand-writing a primitive (§14 forbids it).
   Wrapped cores (CLAUDE.md §2) inherit Rust's backend and are indifferent; this binds the native
   ones.
2. **Random salts give XChaCha's statelessness.** The property that made XChaCha attractive was
   having no nonce protocol: crash, retry, restore, writable copies, compaction and owner change
   are safe by construction. A fresh random salt per derived key has the same property with AES,
   so the choice no longer trades simplicity for platform support.
3. **The browser is not the deciding platform.** Encrypted browser databases are expected to be
   uncommon, so browser speed is a deployment detail, not a selection criterion. (The integrated
   Chromium measurement favored AES there anyway, §8.1.)

Support for the primitives, by platform (late 2026; check each platform's runtime guards, such as
.NET's `IsSupported`, before relying on a row):

| Platform | AES-256-GCM | Raw AES block (the XAES-256-GCM derivation) | HKDF-SHA-256 (fallback layout; key hierarchy) | XChaCha20-Poly1305, for comparison |
|---|---|---|---|---|
| Go | standard library (`crypto/cipher`) | standard library (`crypto/aes`) | standard library (`crypto/hkdf`) | `golang.org/x/crypto` |
| Rust | RustCrypto `aes-gcm` | RustCrypto `aes` | RustCrypto `hkdf` | RustCrypto `chacha20poly1305` |
| Node | `node:crypto`, synchronous | `node:crypto` (`aes-256-ecb`) | `node:crypto` | none native; Noble (pure JS) |
| Browser | WebCrypto, asynchronous | not in WebCrypto (no ECB/CMAC); Noble (pure JS) | WebCrypto | none native; Noble or Rust/WASM |
| Java | JCA, built in and accelerated | JCA `AES/ECB/NoPadding` | JDK 25+ `javax.crypto.KDF` ([JEP 510](https://openjdk.org/jeps/510)); Bouncy Castle or Tink before | not in the JDK ([JDK-8256529](https://bugs.openjdk.org/browse/JDK-8256529) open); Bouncy Castle or Tink |
| .NET | `AesGcm`; macOS via CryptoKit since .NET 8; not in browser WASM ([platform table](https://learn.microsoft.com/dotnet/standard/security/cross-platform-cryptography)) | `Aes.EncryptEcb` (.NET 6+) | `HKDF` (.NET 5+) | not in the base library; third-party only |
| Swift | CryptoKit / swift-crypto `AES.GCM` | **not in CryptoKit** (no raw block or CMAC API); Swift leans wrap (CLAUDE.md §2) | CryptoKit `HKDF` | not in CryptoKit or swift-crypto (96-bit `ChaChaPoly` only) |
| Python (reach) | `cryptography` `AESGCM` | `cryptography` (ECB, CMAC) | `cryptography` | not in `cryptography`; PyNaCl (libsodium) |

Measured speed (§8, 8 KiB pages): AES-GCM seal is 0.87 µs in Go and 2.60 µs in Rust, about 4 µs
through Node's native API, and 7–9 µs per page in situ through WebCrypto. Deriving a fresh key
(random 32-byte salt, HKDF, AES key setup) costs about 0.8 µs in Go, so a fresh key for **every**
page sealed at 1.69 µs, still faster than XChaCha's 2.07 µs in the same harness; through
WebCrypto, salt plus HKDF plus key import measured 4.3 µs. The derivation cost recurs on every
cache-miss read under per-page keys.

#### Key granularity: one key per page (decided 2026-10-20)

Two granularities were measured; both draw the salt from the cryptographic entropy source (*Threat
model* below), bind a versioned purpose label in the HKDF context (pages, meta authentication, key
wrapping and scratch files are distinct purposes), and persist nothing between commits. A crashed,
failed or retried commit attempt simply never uses its salt again, so neither needs a nonce
reservation before the commit guard (§8.1) or an allocator owner across page-service clients: each
committer derives its own keys.

- **Per page (chosen).** Each page encryption draws a salt and derives a single-use key; with each
  key sealing one message, the GCM nonce can be fixed (under XAES-256-GCM the salt *is* the
  192-bit nonce: half derives the key, half is the GCM nonce — *Construction* below). Per-page
  fields: the salt (24 bytes under XAES, 24–32 under the HKDF fallback) and the 16-byte tag, 40 or
  40–48 bytes. Reads derive the page's key on every cache miss; there is no key cache. This is
  the simplest form.
- **Per commit attempt (not pursued).** Each commit attempt draws one salt and seals its pages with
  nonces that count up within the attempt, held only in memory. Per-page fields: the salt, a 4-byte
  page ordinal and the tag, 52 bytes with a 32-byte salt. Readers cache derived keys by salt in a
  bounded cache, deriving once per distinct commit rather than once per page.

The choice weighed the read-side derivation cost on each native core against the reserve size
(§4.1). **Measured (2026-10-19, native Go and Rust, §8):** a full derive (salt, HKDF, AES-GCM key
setup) costs 0.57–0.81 µs in Go and 0.18–0.34 µs in Rust, depending on the HKDF layout below.
Per-page keys therefore raise an 8 KiB cache-miss open by 56–95% of the crypto time in Go (whose
AES is fast) and by 3–10% in Rust. Per-commit keys help reads only when consecutive misses come
from the same commit: read cold, with every page from a different commit, they cost slightly more
than per-page keys (the cache work); from 16 pages per commit, opens and seals come within 7% of
a single fixed key. The per-page reserve is 48 bytes with a 32-byte salt (40 with 24), against
52 per commit.

**Per-page keys are chosen.** Their read-side cost is at most about 0.5–0.65 µs per cache miss
with the cheaper layout, small beside the I/O and decoding a real miss pays. They need the smaller
reserve and no key cache, and per-commit keys help only when consecutive misses come from the same
commit, being no better on cold reads. The salt length (24 or 32 bytes) and the HKDF layout remain
for the security review; the expand-per-key layout measured cheaper (below).

The earlier **writer-lifetime** candidate, one derived key per acquisition of write ownership with
a counter for every encryption under it, is not pursued: its counter allocation across
page-service clients, key-budget rotation and recovery argument are exactly the protocol the
random-salt granularities avoid, for a derivation saving that measured below 1 µs per page. The
same objection covers any persisted write counter: two byte-identical copies of a file share the
data key and the counter's high-water mark, so both resume at the same value and collide. Only
per-write randomness (or a misuse-resistant AEAD) survives a writable fork.

#### Construction: XAES-256-GCM first, a jed HKDF layout as fallback (2026-10-20)

The random salt **is the construction's external nonce**. Its repetition under one data key is the
hazard, not the fixed GCM nonce: a repeated salt repeats the derived key, and the fixed nonce then
reuses catastrophically (GCM exposes the keystream XOR and its authentication subkey, so forgery
follows for the whole derived key). For a uniformly random `s`-bit salt over `q` encryptions under
one data key, the collision term is about `q(q-1)/2^(s+1)` — negligible for a 192-bit salt across
any realistic number of pages, though only one term of the full security argument. Each derived
key seals a single page, far below GCM's per-key limits.

Two constructions realize this derive-key design with AES-256-GCM:

- **XAES-256-GCM (primary candidate).** Specified by
  [C2SP](https://github.com/C2SP/C2SP/blob/main/XAES-256-GCM.md) with test vectors. A 192-bit
  nonce is split: the first 96 bits and the 256-bit key go through the NIST SP 800-108 KDF in
  counter mode with CMAC-AES-256 (label `"X"`, a 16-bit counter) to derive a one-message
  AES-256-GCM key; the remaining 96 bits are the GCM nonce. The spec's stated bound is about
  2^80 messages per key at a 2^-32 collision risk, and it states what the construction is not:
  **not nonce-misuse-resistant and not key-committing**, matching the assumptions above. Its
  derivation is two AES block encryptions plus an AES-256 key schedule (one further block per
  long-term key, amortized), so by operation count it is cheaper than an HMAC-SHA-256 expand and
  it removes SHA-256 from the page path. It needs only **raw AES block encryption** beyond GCM,
  which Go's standard library, Node, JCA, .NET and Noble expose and CryptoKit does not (table
  above); Go has no CMAC, so there the two-block derivation is written in the layout the spec
  fixes and checked against its vectors — a composition of a vetted primitive, the same category
  as the fallback layout, whose admissibility under CLAUDE.md §14 is part of the review. Caveats:
  the RustCrypto `xaes-256-gcm` crate states it has had no security audit, and an independent
  published cryptanalysis of the construction was not confirmed when this was written, so a review
  of the composition is still owed; it is a smaller obligation than reviewing a layout only jed
  specifies. Per-page reserve: **40 bytes** (24-byte nonce + 16-byte tag). Not yet measured in
  the §8 harness.
- **HKDF-SHA-256 + AES-256-GCM under a fixed nonce (fallback).** Our own composition of standard
  primitives, close to [Tink's AES-GCM-HKDF streaming
  AEAD](https://developers.google.com/tink/streaming-aead/aes_gcm_hkdf_streaming), but Tink's
  construction includes a separate random nonce prefix and segment framing, so its analysis
  cannot be inherited by dropping those elements; neither it nor the XAES analysis covers a
  mutable page codec automatically. The layout is frozen only after the four conditions in §9
  are met: one exact construction (random salt as the extract salt, or as expand context under
  one key extracted at unlock; domain separation; 96-bit nonce; full tag; authenticated page
  context), quantitative lifetime bounds, review of the complete composition, and per-backend API
  feasibility (WebCrypto's HKDF always runs Extract then Expand, so the expand-only layout needs
  an HMAC path there). The expand-context layout measured 0.24 µs per key cheaper in Go and
  0.16 µs in Rust (§8), since it extracts once at unlock. Per-page reserve: 40–48 bytes (24- or
  32-byte salt + tag).

HKDF stays regardless for the **once-per-unlock key hierarchy**: unwrapping the data key and
separating purposes (pages, meta authentication, key wrapping, scratch files — §6), where it is
the standard tool and its cost is paid once.

| Construction | Derivation per page | Reserve per page | On repeated randomness | Platform fit | Specification |
|---|---|---|---|---|---|
| HKDF-SHA-256 + AES-256-GCM, fixed nonce (fallback) | one HMAC-SHA-256 expand + AES key schedule | 40–48 bytes | catastrophic: keystream and GHASH key exposed | every platform library | jed-owned composition; Tink/XAES analyses do not transfer |
| **XAES-256-GCM (primary)** | two AES blocks + AES key schedule | 40 bytes | catastrophic, as GCM | every platform but CryptoKit (raw AES block) | C2SP, with vectors; ~2^80 messages/key at 2^-32 collision; not misuse-resistant, not key-committing |
| XChaCha20-Poly1305 | HChaCha20 | 40 bytes | keystream reuse exposed; forgery limited to that nonce (per-nonce Poly1305 key) | third-party outside Rust and TS; dropped | IETF draft; libsodium implementations |
| AES-256-GCM-SIV | per-nonce key derivation; two passes over the plaintext | 28 bytes | discloses only whether the two plaintexts were equal | absent from Go stdlib and `x/crypto`, WebCrypto, and the Java/.NET/Swift platform libraries | RFC 8452 (CFRG informational) |

**Why not AES-GCM-SIV.** Its advantage is exactly the failure mode in the table:
[RFC 8452 §9](https://www.rfc-editor.org/rfc/rfc8452#section-9) states that encrypting two
messages under one nonce discloses only whether they were equal, and it recommends random nonces.
Needing a nonce policy is not a mark against it — every AEAD here needs one, and GCM-SIV's usage
bounds still apply. It is not pursued for **platform reasons alone**: it is absent from Go's
standard library and `golang.org/x/crypto`, from WebCrypto, and from the Java, .NET and Swift
platform libraries, so every native core would carry a third-party implementation, and its
two-pass encryption costs more than GCM. Choosing AES-GCM over it is a **knowing trade** of
graceful failure under duplicated randomness for platform support. The trade is recorded here so
the live-clone discussion below is read with it; reconsider GCM-SIV if the tolerance for RNG
failure and live snapshots changes.

#### Threat model, entropy and fault fixtures

**Threat model.** Nonce reuse can expose plaintext relationships to an offline attacker holding
two backups; it does not require control of a running process. A single writer cannot prevent
historical reuse on its own. Protect ordinary consistent copies, backup restoration and crash
remnants. Per-page keys draw fresh randomness for every encryption, so a live process fork or
virtual-machine snapshot repeats a key only if it duplicates the random-number generator state —
which a restored clone can do (below).

**Key material uses a dedicated cryptographic entropy source (decided).** Data keys, KDF salts,
page and commit salts and scratch-file keys are drawn from a source separate from the
general-purpose entropy seam that SQL generators use ([entropy.md](entropy.md)). A host may
install a seeded source there to make `uuidv4` reproducible; if that source also fed key
material, the data key would be predictable, and two runs would repeat salts, so the same derived
keys would seal different pages under the same nonces. In production the cryptographic source
reads the OS CSPRNG directly, never a buffered userspace generator, so a process fork cannot
duplicate engine-held generator state. Fixtures and simulation inject fixed bytes only through an
explicitly insecure test option, which never applies by default.

**Live VM clones can duplicate the OS CSPRNG too (open).** Reading the kernel generator does not
cover a restored virtual-machine snapshot: the clone resumes with the guest kernel's RNG state
duplicated. Linux reseeds from the hypervisor's VM generation ID, but the [virtio-rng
entropy-leak proposal](https://lore.kernel.org/lkml/Y9lBeymca9eFaJ33@zx2c4.com/T/) documents a
window between resume and that reseed, and the generation ID never notifies userspace. A salt
drawn in that window repeats in two clones, the derived key repeats with it, and the fixed GCM
nonce then reuses catastrophically. This is not specific to random salts: every nonce discipline
that takes no fresh entropy after restore fails identically (a persisted or in-memory counter
resumes at the same value in both clones), and only a misuse-resistant AEAD degrades to an
equality leak instead (*Why not AES-GCM-SIV*). Ordinary database-file restoration is the
well-handled case: a restored file draws new salts on its next write. A live clone is the hazard.
The decision to record: either **exclude live VM clones from the threat model** explicitly, or
**specify the host guarantee** the engine relies on (the guest is reseeded, or the host injects
fresh entropy into the cryptographic source, before any write after restore). The lean is to
exclude and document: the platform criterion already rules out GCM-SIV, and an attacker who needs
two clone images is a narrower threat than a crash retry. Open for the user's decision (§9).

Required future fault fixtures: aborted encryption before/after each body/meta write, failed
barriers, retry and owner promotion, restore followed by divergent writes, independently opened
copies, mixed historical keys, compaction, credential changes, and domain separation between
body/meta/wrapping/scratch. Check both ciphertext authenticity and salt/key uniqueness across
every attempted encryption, not just committed pages. Whole-file anti-rollback
remains a separate trusted-state guarantee.

## 4. What is encrypted, what is plaintext

- **Every body page: ciphertext + auth tag.** That is every page type except the meta slots:
  catalog-chain pages, B-tree leaf and interior nodes, GiST leaf and interior nodes, overflow
  pages, free-list pages (`page_type 7`) and manifest-overflow pages (`page_type 8`). Free-list
  and manifest pages are not just bookkeeping: a tampered free list could steer the allocator
  into overwriting live pages, so authenticating only data pages would leave tampering
  undetected. The page payload is encrypted; the AEAD **authentication tag** and nonce fields
  live in the page's reserved region (§4.1). Page identity, generation and the relevant
  file/domain identity must be bound as **AAD** so a page cannot be silently relocated; their
  encoding and nonce relationship remain to be specified (§3).
- **Replaying an older version of a page is not detected by per-page AEAD (open).** An attacker
  with write access can replace page `P` with an older valid ciphertext of `P` from a backup or
  crash remnant: the tag verifies and the page index in the AAD matches. A "generation" in the
  AAD helps only if the reader learns the expected generation from somewhere trusted, not from
  the page itself. Copy-on-write makes a fix natural: every child rewrite already rewrites its
  parent, so a parent pointer (and the meta root pointer) could carry a binding to the child,
  chaining authentication from the authenticated meta down the tree as ZFS block pointers do.
  That binding must be a **unique write identity, not a transaction number**: `txid` repeats
  across divergent histories (a restored copy and the original both write `txid + 1`), so a
  parent carrying the child's `txid` would accept either history's page. With per-page random
  salts the child's salt or tag is unique per write by construction; the recommendation is to
  bind the child's **tag** (16 bytes per pointer), which binds the exact ciphertext rather than
  only its identity. The cost is wider pointers and so lower interior fan-out (another input to
  `C`, §4.1), which is why this decision precedes reserve sizing. The alternative is to exclude
  page-level replay from the threat model explicitly, as SQLCipher effectively does. Either is a
  decision to record before building; silently inheriting the gap is not.
- **Manifest page identities (open; recommendation: on-disk bytes).** The validated-COW manifest
  hashes logical (decrypted) page bytes ([validated-cow.md](validated-cow.md) §5), so recovery's
  completeness check needs the key, as it does at a page-service owner
  ([page-service.md](page-service.md)). Defining the identity over the **on-disk (ciphertext)
  bytes** instead lets a keyless replica or backup check that an applied delta is complete,
  without authenticating it, and keeps recovery's candidate selection keyless; that is the
  recommended layout, with authentication left to the page tags and the authenticated meta. Note
  the manifest also covers a case AEAD alone cannot: a page write that never reached disk leaves
  the older, still-valid ciphertext in place — a ciphertext digest detects that as a logical one
  does. The choice is recorded when the format is specified.
- **The AEAD tag supersedes the CRC for encrypted pages.** The tag detects *both* corruption and
  tampering — strictly stronger than the CRC, which detects only the former (§1). So an encrypted
  file relies on the AEAD tag rather than the CRC for stored-byte integrity. This *closes* the
  end-to-end-integrity gap `format_version` 7 explicitly left open (storage.md §6). The TS page
  codec seam computes page CRCs on the logical page before encoding (ts-runtime.md §11.5), so the
  CRC ends up inside the ciphertext, covered by the tag. Whether the encrypted format keeps or
  drops that inner CRC is a format detail; it no longer guards the stored bytes either way.
- **The meta pages (slots 0/1): readable bootstrap, authenticated metadata.** Page size,
  algorithm identifiers, KDF salt and bounded KDF parameters must be readable before unlock.
  The file stores a **wrapped data key**, never a plaintext data key, wrapping key or passphrase.
  Authentication must cover the wrapped-key context and security-relevant metadata; a CRC alone
  is not sufficient. Exact tag placement and the crash-safe credential-update protocol across
  both meta slots remain open. Page authentication alone does not detect rollback of an entire
  valid older file; anti-rollback would require an additional trusted state contract.
- **A new `format_version`** (or a meta flag within one) marks an encrypted file; an encrypted
  file is a distinct on-disk variant with its own golden fixtures, generated with a **fixed test
  key** so the bytes are deterministic and checkable (the encryption analogue of the seeded
  entropy source, determinism.md §5).

### 4.1 Reserved bytes and usable page capacity (decided)

Today every page-capacity rule derives from `C = page_size - 16` (format.md *Page model*):
`RECORD_MAX`, leaf and interior fan-out, split points, large-value externalization and the
overflow chunk size. Per-page crypto fields (40–48 bytes plus any binding, §3.1) would shrink
that capacity, so an encrypted file would build a differently shaped tree from the same rows,
with different page counts, page-read costs and possibly plans.

The format will therefore follow SQLite's *reserved bytes per page*. SQLite records it at
database-header offset 20, computes all B-tree capacity from the *usable size* (page size
minus reserved bytes), and never reads the reserved tail, which is where its encryption
extensions (SEE, SQLCipher) keep their nonce and MAC and where `cksumvfs` keeps its checksum.
For jed:

- The meta page records `reserved_bytes`; `C = page_size - 16 - reserved_bytes`. The page
  codec owns the reserved region; nothing above the codec reads it. This is also what lets the
  codec keep pages at `page_size`: the TS `PageTransform` maps one page to one stored page at
  the same index (ts-runtime.md §11.5), so the nonce and tag must fit inside the page, in the
  reserved tail that the logical page leaves unused.
- **Tree shape, record caps, cost and plan identity are functions of `C`.** The cross-core
  contract binds for a given `(query, database state, C)`; encryption itself is invisible.
- A plaintext file may reserve bytes too. That gives a differential test: an encrypted file
  and a plaintext file with the same `reserved_bytes` and the same history must produce
  identical tree shapes, page counts, plans and costs.
- A plaintext file created with a sufficient reserve can later be encrypted in place, and
  data-key rotation never changes the layout. Without the reserve, encrypting is a full
  rebuild (as with SQLCipher's export to a fresh file).

The field arrives with the encryption format version, not before. Open: whether plaintext
files reserve by default (about 0.5–0.7% of an 8 KiB page) so they can be encrypted in place,
and the reserve's size once the suite and any parent-pointer binding (§4) are chosen. Page
sizes must keep `C` above the structural minimum; at the 256-byte golden page size a 40–48-byte
reserve is a large fraction, which fixtures must account for.

### 4.2 Scratch files (decided)

When the database is encrypted, its spill and temporary-table spill files are encrypted too
([spill.md](spill.md), [temp-tables.md](temp-tables.md) §6). Each scratch file gets a random
key from the cryptographic entropy source (§3.1), held only in memory and never persisted. The
key never outlives the process or the unlinked file, so none of §3's crash, restore or copy
hazards apply: any nonce scheme unique under that key is sufficient, including a simple
counter. Scratch encryption uses the same AEAD suite as pages, so each core keeps one crypto
path. A scratch block that fails authentication is an error, never data. Like spill I/O
itself, scratch encryption is unmetered physical work, so query results and cost do not change.
Hosts with no scratch target (in-memory, OPFS, WASI) have nothing to encrypt. A compaction's
`<path>.jedtmp` is an ordinary database image written through the page codec, not a scratch
file.

## 5. Cost & determinism

- **Encryption is invisible to the deterministic cost at a given usable capacity `C`**
  (CLAUDE.md §13, cost.md, §4.1). Like the CRC and the buffer pool, encrypt/decrypt is
  **physical** page I/O, not a metered logical unit. What encryption does change is the page's
  reserved region, and therefore `C`; cost is a function of `C`, so `# cost:` contracts hold
  unchanged for an encrypted file and a plaintext file with the same reserve. (Contrast the
  *compression* codec, which **is** metered — `value_compress`/`value_decompress`, cost.md §3 —
  because it is a per-value transform the query can statically attribute; whole-page
  encrypt/decrypt is per-page physical I/O, like the CRC.) If a future requirement wants
  encryption metered, it would be a new physical-vs-logical decision recorded here.
- **Byte identity is conditional on identical crypto inputs** (§3). Fixed-input vectors and
  cross-core encrypted-file round trips remain required. Production entropy and writable-copy
  behavior must be specified before claiming an exception-free deterministic file contract.

## 6. Envelope encryption and key lifecycle

Use a random per-database **data-encryption key (DEK)** for page encryption. A separate
**key-encryption key (KEK)** wraps the DEK, and only the wrapped DEK is persisted. At create/open,
accept either a host-supplied wrapping key or a passphrase from which the KDF derives the wrapping
key using the file's salt and bounded parameters. Raw-key input means a wrapping key, not direct
page-key input. Key generation uses the sanctioned entropy seam; fixtures inject fixed inputs.

Two operations have deliberately different semantics:

| Operation | Work and guarantee |
|---|---|
| Change passphrase / wrapping key | Unwrap and rewrap the same DEK; update bounded metadata, without rewriting data pages. |
| Rotate the data key | Encrypt every live page under a fresh DEK, potentially through a compacting rewrite. Old-key retirement completes only after all live pages migrate. |

The host owns credential storage, rotation policy, escrow, and OS-keychain integration. The
engine owns wrapping and crash-safe file transitions. A credential change must specify both meta
slots, interrupted updates, old wrapper retirement, and existing open handles/page-service
clients; a single meta overwrite is not yet a protocol. An already unlocked process retains its
DEK unless a separate close/reopen policy is enforced.

**Rewrapping is not recovery from a compromised DEK.** An old wrapper plus its old credential
still reveals the unchanged DEK. A retained backup/header copy can therefore defeat credential
revocation for that DEK. Full data-key rotation protects the newly encrypted live data; it cannot
revoke access to old copies or promise secure erasure of COW remnants, filesystem snapshots or
backups. Incremental multi-key rotation remains a possible later design, not an initial promise.

## 7. Crypto implementation and the §14 gate

Use vetted AEAD and KDF implementations, never hand-written cryptographic primitives. The suite
direction is AES-256-GCM with one derived key per page — XAES-256-GCM first, a jed HKDF-SHA-256
layout as fallback (§1.1, §3.1); no production library or backend is selected, and
ChaCha20-Poly1305 remains only a measured comparison (§8). The passphrase KDF for the wrapping key
(§6) is an open selection, made with the wrapped-key format (§9). Go may use its standard library
where sufficient. Any new dependency, including a benchmark-only dependency, requires explicit
human approval under CLAUDE.md §14.

**Safe engine code and safe library APIs are the boundary.** No cgo or external C-library FFI is
introduced into the Go core. Reviewed cryptographic implementations may use assembly or hardware
intrinsics behind safe APIs, including narrowly approved Rust `unsafe` internals. Assembly does
not imply cgo, and `unsafe` does not by itself imply a memory-safety defect. The exact dependency,
backend, target coverage and low-level safety obligations still require review and approval;
this is not a blanket exception for engine code or arbitrary native dependencies. The approved
`sha2` intrinsics are an existing precedent (CLAUDE.md §14).

Evaluate constant-time behavior separately from memory safety, including software fallbacks and
supported CPUs. Hardware acceleration may improve both timing behavior and speed. Cross-core
output must remain byte-identical for identical crypto inputs regardless of backend. Browser/JS
runtime constraints and key-lifetime limitations must be documented rather than hidden behind a
blanket memory-safety claim.

## 8. Performance and measurement

There is no measured whole-database encryption overhead yet. The codec is below the plaintext
buffer pool: cache hits do no page crypto, misses decrypt/authenticate, and commits encrypt each
changed physical page (including COW metadata). An OS-cache hit still needs decryption when the
engine's cache misses. Passphrase unlock pays the KDF once per unlock, not once per page; rewrapping
pays KDF and metadata-update work. Tags and nonce/generation fields reduce page payload capacity.

Measure seal and authenticated open separately at 4, 8 (the default), 16 and 64 KiB. Record
algorithm, library/runtime versions, CPU, acceleration policy, nonce/AAD/tag sizes, allocation and
copy behavior, warmup, repeated samples, and throughput/latency. Validate identical ciphertext
and tags across implementations, round trips, and rejection of altered data, tag, AAD and key
before timing. Fixed public benchmark inputs are never a production nonce policy.

The first experiment measures hot-memory AEAD operations, excluding disk, SQL, KDF, file-format
integration and key rotation. Native reusable-output buffers and allocating runtime APIs must be
labelled separately. TypeScript needs both a synchronous pure-JS candidate and native Node and
WebCrypto comparisons: the current engine storage path is synchronous, while WebCrypto is async.
Node WebCrypto results are not browser/OPFS measurements. Follow with integrated warm reads,
scans and durable writes once the codec exists; primitive throughput is not a database slowdown
percentage. See [the benchmark methodology and recorded results](../../bench/aead/README.md).

**First measurement (2026-10-17).** On an Intel Core Ultra 9 285K, 8 KiB AES-GCM seal/open
medians were 2.60/2.67 µs in Rust and 0.87/0.78 µs in Go with reusable buffers including the
input copy. Allocating pure-JS AES was 112.10/111.26 µs; pure-JS ChaCha20-Poly1305 was
30.51/29.97 µs. Native Node AES was 4.11/3.88 µs and sequential Node WebCrypto AES was
11.55/11.64 µs. All five backends agreed on full ciphertext/tag fixtures and rejected corruption.
Native AES wins on this CPU; ChaCha is about 3.7× faster in pure JS. These API-level results
make the browser/JS path important to algorithm selection, but choose no production algorithm.
The linked report retains all 104 cases, samples, versions, allocation modes and limitations.

**Key granularity (2026-10-19).** `mise run bench:aead_granularity` measures the two §3.1
granularities in native Go (standard library) and Rust (RustCrypto `aes-gcm` and `hkdf`) at 4, 8
and 16 KiB, under both HKDF layouts: per-page keys derived on every seal and every open with no
cache, and per-commit keys for commits of 1, 4, 16 and 256 pages, read through a bounded
salt-keyed cache that misses once per commit. Both cores pass RFC 5869 vectors and tamper
rejection, and agree byte for byte on derived keys and ciphertext for fixed salts. Median µs per
8 KiB page (extract-per-key / expand-per-key layout):

| | Go seal | Go open | Rust seal | Rust open |
|---|---:|---:|---:|---:|
| Fixed key, no derivation | 0.98 | 0.90 | 2.68 | 2.72 |
| Per page | 1.88 / 1.62 | 1.76 / 1.41 | 3.08 / 2.94 | 2.98 / 2.81 |
| Per commit, 1 page (cold) | 1.86 / 1.62 | 1.84 / 1.54 | 3.07 / 2.94 | 3.07 / 2.92 |
| Per commit, 16 pages | 1.04 / 1.02 | 0.97 / 0.95 | 2.72 / 2.72 | 2.79 / 2.75 |
| Per commit, all-hit cache | — | 0.92 | — | 2.78 |

The derivation is a fixed cost per key, so its share falls with page size (Go's per-page open
overhead is 36–49% at 16 KiB). These are hot-memory crypto costs; a real cache miss also pays
I/O and decoding, which they exclude. One CPU, single-threaded; no ARM or WebCrypto figures.
The [report](../../bench/aead/results/2026-10-19-granularity.md) has every case and its method.

### 8.1 Browser execution and the Rust/WASM option

The [browser/Node runtime](ts-runtime.md) implements async-only public JS entry points,
dedicated execution workers, bounded typed transport, serialized operation scheduling and
cooperative cancellation. Retained page and expression continuations let execution await data
reads without replaying SQL. Common resident expressions still return immediate values. Node
data-page reads can await I/O; browser OPFS sync access handles remain synchronous inside the
worker.

The asynchronous-crypto problem is browser-only: Node's native `node:crypto` AEAD calls are
synchronous, and browsers have no scratch target, so spill is unaffected.

WebCrypto needs suspension **inside the worker** at decryption/cache misses and before encrypted
commit writes. The write side is now ready for that ([ts-runtime.md](ts-runtime.md) §11): the
pager's `PageTransform` seam, above the byte device and below the page cache, accepts asynchronous
`encodeAsync`/`decodeAsync` forms, and commit, open/recovery and maintenance await them as retained
continuations. Decryption completes before a page enters the cache; page CRCs, manifest digests and
the live count stay over plaintext logical pages. Commit encryption overlaps in a bounded window of
16 pages while writes keep validated COW's body, free-list, manifest, alternate-meta and one-barrier
order. The commit after its last safe cancellation point is uncancelable, and an encode failure
after the commit guard is armed poisons the handle like an I/O error. The random-salt keys of §3.1
need no reservation before the guard: a poisoned or retried commit never reuses its salt. Only an
identity test transform exists; tests prove every write-side page operation awaits it with unchanged
bytes, results and cost. Remaining synchronous paths that an encrypted browser database would still
need to settle: `create`'s bootstrap image (the encoding of bootstrap pages is part of §6) and
compaction, which is already unsupported on OPFS. Spill and temporary domains have no browser
storage. Do not replay partially evaluated SQL to recover from a miss: entropy consumption, costs,
host functions and staged writes must occur exactly once. Do not preload the whole file to hide
asynchronous reads, which would violate bounded-residency requirements.

Awaiting crypto also introduces event-loop interleavings. The runtime retains the active
operation's snapshot/pins and writer ownership across suspension, processes cancellation/control
messages, and queues other engine operations; the write side preserves that ownership model and
validated-COW publication/barrier/error-poisoning ordering (ts-runtime.md §11.2). Bounded batches
of independent dirty pages overlap encryption; parent-dependent reads (catalog, skeleton and chain
walks) are awaited one page at a time, and only recovery's listed dependencies are read ahead.
Promise dispatch overhead, allocation, cancellation and working memory belong in integrated
measurements. The runtime implementation selects no encryption suite, nonce protocol or production
provider; the primitive measurements below do not establish encrypted-database performance.

The existing **full Rust/WASM wrapper** (`impl/wasm`) targets WASI and is driven through Node;
it is not yet a browser/OPFS package. It keeps Rust execution synchronous and adds parameter/result
marshalling. Its current ABI has a limited typed-value surface, and browser storage/entropy/clock
adapters still need work. A production wrapper is permitted by CLAUDE.md §2, but cannot replace
the independent TS conformance voice. RustCrypto compiled to WASM uses software backends on this
target; native x86 AES timings do not transfer to it, even with WASM SIMD enabled.

For a Rust/WASM browser package, platform WebCrypto is also an option: host imports can suspend
WASM through [JavaScript Promise Integration](https://v8.dev/blog/jspi), keeping Rust's internal
code synchronous. This needs a measured bridge and a browser-support/fallback policy. JSPI cannot
suspend arbitrary synchronous JavaScript execution, so it does not solve TS's pager directly.
A helper worker with shared-memory waits is another possible bridge, but adds coordination and
deployment requirements; neither bridge is implemented in these probes.

Real Chromium-worker AEAD, native writer-key setup, and existing whole-engine WASM SQL controls
are recorded in [bench/aead/README.md](../../bench/aead/README.md). The crypto-only WASM probe runs
in the browser; the whole-engine SQL control runs under Node/WASI. Neither is an encrypted OPFS
database benchmark. Compare those layers separately when choosing a suite and browser architecture.

**Integrated async-codec measurement (2026-10-19).** `mise run bench:aead_codec_browser` installs
measurement-only `PageTransform` codecs that pay a real AEAD cost per page and return the plaintext.
File bytes are verified unchanged, and the keys and nonces are fixed benchmark inputs. Over real
OPFS in five fresh headless-Chromium 149 runs on the x86 host above, WebCrypto AES-256-GCM cost
about 7–9 µs per 8 KiB page in situ. That is about 2% of a 461-page commit, about 5–8% of a
single-row commit, and about 3.4 ms on a cold open or scan of about 470 pages, below those
operations' process-run variation. Synchronous pure-JS XChaCha20-Poly1305 cost about 40–46 µs per
page: about 10% of a large commit, about 25% of a small commit, and 1.8–2.0× on cold scans and
opens. Chromium's worker WebCrypto ran each operation inside the call and resolved without a task
boundary, so encode windows 1/4/16 performed alike and awaiting AES left commit and open as
unresponsive as no codec. A task turn every 16 operations restored control-message admission
during open; long commits still blocked on synchronous serialization. These are single-browser,
single-machine observations: no per-writer key derivation, nonce allocation, ciphertext layout or
crash behavior. They **select no suite**. See the
[report](../../bench/aead/results/2026-10-19-async-codec.md).

## 9. Open / deferred

**Suite direction: AES-256-GCM with one derived key per page — XAES-256-GCM first, a jed
HKDF-SHA-256 layout as fallback** (§3.1). Platform support across current and future native cores
decides the AEAD: AES-GCM and the raw AES block are in every standard library but CryptoKit, while
XChaCha20-Poly1305 is in none and AES-GCM-SIV in almost none. Browser performance is not a deciding
factor. The direction becomes a format decision after a security review of the exact construction
and the §3.1 fault fixtures; no production dependency is selected. Still to decide in that review:
XAES versus the HKDF fallback (and, for the fallback, the salt length and HKDF layout), the
RNG-duplication decision (§3.1, *Live VM clones*), the replay binding and the manifest bytes (§4).
A pure-JS fallback and a software-WASM backend are material deployment costs, not reasons by
themselves to maintain two file suites.

The isolated linked AEAD increments were roughly 31–49 KiB in Rust and 52–72 KiB in Go for one
algorithm; the selected Noble exports were 3.7–4.6 KiB gzipped. These exclude the passphrase KDF,
page codec and lifecycle machinery; they are not estimates of the complete feature. See the
[size methodology and exact inventory](../../bench/aead/README.md#dependency-and-binary-footprint).

- **The whole feature** remains unimplemented and unscheduled; non-foreclosure is satisfied.
- **Construction, KDF and exact library/backend selection** — XAES-256-GCM versus the HKDF
  fallback (§3.1), the passphrase KDF for the wrapping key, backends, and supported fallback CPUs.
- **Freezing the HKDF fallback layout**, if XAES is ruled out, requires all four: (1) one exact
  construction — derivation layout, framing, purpose separation, the 96-bit GCM nonce, the full
  tag and the authenticated page context; (2) quantitative lifetime bounds across all uses of a
  DEK, including failed attempts and writable copies (the collision term `q(q-1)/2^(s+1)` for an
  `s`-bit salt is one term of that argument, not the whole of it); (3) review of the complete
  composition — primitive vectors and cross-core agreement establish implementation consistency,
  not cryptographic security; (4) exact API feasibility per backend (WebCrypto's HKDF always runs
  Extract then Expand, so an expand-only layout needs a vetted HMAC path there).
- **RNG duplication under live VM clones** — exclude from the threat model, or specify the host
  reseed guarantee the engine relies on (§3.1); lean: exclude and document.
- **Nonce/domain lifecycle** under failed commits, recovery, restore, writable copies and
  compaction; encoding, lifetime limits, and any determinism-ledger revision.
- **Authenticated metadata and wrapped-key format**, including bounded KDF inputs, bootstrap
  validation, both meta slots, and atomic credential changes.
- **Data-key rotation protocol**, old-key retirement and coordination with open clients.
- **Page-level replay** — binding the child's tag in parent pointers (recommended) versus an
  explicit threat-model exclusion; a `txid` is not a usable binding; decided before reserve
  sizing (§4).
- **Manifest identity bytes** — on-disk (ciphertext) bytes recommended, so a keyless replica can
  check delta completeness and recovery's candidate selection stays keyless (§4).
- **Reserve sizing** — the reserve per construction (40 bytes under XAES-256-GCM, 40–48 under the
  HKDF fallback, plus 16 per child pointer if tags are bound), and whether plaintext files
  reserve by default (§4.1).
- **Integrated performance**, async page/commit prototype and browser portability measurements.

### 9.1 SQL crypto functions are a separate proposal

Bundling vetted primitives makes a small SQL crypto surface plausible, but does not make the
whole `pgcrypto` surface free. Evaluate authenticated byte-value seal/open operations with
explicit application keys and AAD, a versioned ciphertext envelope, and internally managed
nonces. Names and signatures remain undecided. Never implicitly expose or reuse the database
DEK for application encryption; file encryption and value encryption have different key owners
and lifetimes. SQL callers should bind secrets as parameters rather than put them in SQL text;
parameter/logging policy still belongs to the host.

Such built-ins need shared byte fixtures, bounded allocations and logical cost charges, uniform
authentication-failure behavior, and explicit entropy/dependency classification. Randomized
sealing uses the sanctioned entropy seam and cannot be admitted into persisted expression keys
or partial-index predicates. Decryption authenticates before releasing plaintext. Application-key
nonce allocation must be designed independently: the database page-key derivation is not a
general-purpose nonce allocator for arbitrary caller keys.

Digest/HMAC functions can be evaluated on their own merits. Password hashing, legacy ciphers,
OpenPGP and broad pgcrypto compatibility are not implied by the page-encryption dependency.
This is an optional, unscheduled SQL feature, not a prerequisite for encryption at rest.

## References

- [Envelope encryption](https://docs.aws.amazon.com/kms/latest/developerguide/kms-cryptography.html)
- [RustCrypto AES backends](https://docs.rs/aes/latest/aes/)
- [Go AES timing qualifications](https://pkg.go.dev/crypto/aes)
- [Noble ciphers and WebCrypto API](https://github.com/paulmillr/noble-ciphers)
- [XAES-256-GCM (C2SP)](https://github.com/C2SP/C2SP/blob/main/XAES-256-GCM.md) and its
  [design write-up](https://words.filippo.io/xaes-256-gcm/)
- [RFC 8452, AES-GCM-SIV](https://www.rfc-editor.org/rfc/rfc8452) (§9: nonce-reuse behavior)
- [virtio-rng entropy-leak reporting (RFC patch)](https://lore.kernel.org/lkml/Y9lBeymca9eFaJ33@zx2c4.com/T/):
  the VM-snapshot reseed window
