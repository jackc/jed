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
  for identical key, nonce, plaintext and AAD. Creation entropy (data keys, salts, and any nonce
  domain) must use the sanctioned host seam, with fixed inputs in fixtures. Production files
  need not match when these inputs differ. The full nonce/domain protocol remains undecided:
  evaluate crash-safe allocation, fresh domains for writable copies/restores, sufficiently large
  random nonces, and misuse-resistant constructions with their actual bounds. A random domain
  saved in a file is copied with that file and does not by itself solve writable forks. Record
  any necessary determinism-ledger change before implementation; never trade nonce safety for
  identical files. Recovery, retry, copying, and backup tests must establish the chosen contract.

### 3.1 Candidate protocols under investigation (not a format decision)

Prefer **one initial suite** across cores, with a versioned format for later replacement. A second
suite needs a measured platform limitation or concrete interoperability requirement. Platform
coverage includes future native packages, browser runtimes, WASM and software fallbacks; native
CPU throughput alone does not settle the choice.

The current comparison is **AES-256-GCM with derived writer keys and counter nonces** versus
**XChaCha20-Poly1305 with fresh random 192-bit nonces**. AES-GCM-SIV remains an alternative if
nonce-misuse resistance justifies its implementation-vetting and performance costs. XChaCha
reduces collision probability but is not nonce-misuse resistant. AES-GCM-SIV still needs key-use
limits and a nonce policy; it does not prevent whole-file rollback.

An AES candidate has the following lifecycle, using a vetted HKDF-SHA-256 implementation:

1. Every acquisition of write ownership creates a fresh 256-bit cryptographic salt and derives
   an encryption key from the database data key, salt, and a versioned purpose label/context.
   The salt is public; this is a cheap key derivation, distinct from the passphrase KDF.
2. Allocate a unique counter for every encryption under that derived key. Allocate independently
   of SQL transaction IDs, consume allocations even on abort, and do not rewind on retry.
   Rotate to a fresh salt/key before the chosen message/byte/security budget is exhausted,
   not merely before the counter integer overflows.
3. After crash, reopen, owner handoff, restoration, or opening an independently writable copy,
   create a fresh writer key. Never resume a persisted writer salt with a reset counter.
   Preserve old salts only as read metadata so historical pages can be decrypted.
4. Persist each page's salt/key identifier and counter in bounded, authenticated context.
   A direct encoding could store a 32-byte salt, 8-byte counter (zero-extended to a 12-byte
   GCM nonce), and 16-byte tag: 56 bytes before other context, versus XChaCha's 40-byte
   nonce/tag. An epoch table saves space but adds retention/reclamation dependencies; the
   direct encoding is the simpler starting point to evaluate. Neither encoding is ratified.

This does not require persisting a nonce reservation before the ciphertext: a recovering writer
uses a new key regardless of which writes reached disk. It could preserve validated COW's one
barrier; that is a design argument, **not a tested encrypted crash protocol**. Metadata/wrappers,
scratch data, and SQL value encryption need distinct purposes/keys or a proven shared allocator.
All encryption must have an unambiguous allocator owner, including page-service client work.
Key derivation on read needs a bounded cache; files with many historical writer keys must not
force unbounded resident metadata. Re-encrypting byte-identical ciphertext is unnecessary when
retrying a physical write; encrypting different contents always consumes a fresh allocation.

A fresh key per **commit attempt** is another candidate granularity. It simplifies abandonment
and retry, but produces more historical key domains. Benchmark derivation plus key setup before
choosing between it and writer-lifetime keys. The measurements in §8 include per-page, 16-page,
and 256-page amortization without implementing either storage protocol.

The pattern has precedent in [Tink's AES-GCM-HKDF streaming construction](https://developers.google.com/tink/streaming-aead/aes_gcm_hkdf_streaming),
which uses a random salt, derived key, and segment nonces. Tink is not a mutable database codec,
and its security analysis does not automatically cover this adaptation. Review the exact
construction, cumulative security bounds and failure cases before implementing it.

**Threat model.** Nonce reuse can expose plaintext relationships to an offline attacker holding
two backups; it does not require control of a running process. A single writer simplifies
allocation but cannot prevent historical reuse on its own. Protect ordinary consistent copies,
backup restoration and crash remnants. A live process fork/checkpoint that duplicates an active
key and counter requires codec reinitialization; a large random salt at initial open does not
protect subsequently cloned live state. Fresh cryptographic entropy is a host requirement for
both candidates; the general-purpose deterministic test source is not a production substitute.

Required future fault fixtures: aborted encryption before/after each body/meta write, failed
barriers, retry and owner promotion, restore followed by divergent writes, independently opened
copies, counter/key-budget exhaustion, mixed historical epochs, compaction, credential changes,
and domain separation between body/meta/wrapping. Check both ciphertext authenticity and pair
uniqueness across every attempted encryption, not just committed pages. Whole-file anti-rollback
remains a separate trusted-state guarantee.

## 4. What is encrypted, what is plaintext

- **Body pages (catalog, B-tree nodes, overflow): ciphertext + auth tag.** The page payload is
  encrypted; the AEAD **authentication tag** travels with it (in the page header region — the
  16-byte header already carries a CRC slot the tag can occupy/extend, format.md). Page identity,
  generation and the relevant file/domain identity must be bound as **AAD** so a page cannot be silently
  relocated; their encoding and nonce relationship remain to be specified (§3).
- **The AEAD tag supersedes the CRC for encrypted pages.** The tag detects *both* corruption and
  tampering — strictly stronger than the CRC, which detects only the former (§1). So an encrypted
  file replaces the per-page CRC with the AEAD tag rather than carrying both. This *closes* the
  end-to-end-integrity gap `format_version` 7 explicitly left open (storage.md §6).
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

## 5. Cost & determinism

- **Encryption is invisible to the deterministic cost** (CLAUDE.md §13, cost.md). Like the CRC
  and the buffer pool, encrypt/decrypt is **physical** page I/O, not a metered logical unit, so
  `# cost:` corpus contracts are byte-unchanged whether or not a file is encrypted. (Contrast the
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

Use vetted AEAD and KDF implementations, never hand-written cryptographic primitives. AES-256-GCM
and ChaCha20-Poly1305 are benchmark candidates, not selected file algorithms; Argon2id and scrypt
remain KDF candidates. Go may use its standard library where sufficient. Any new dependency,
including a benchmark-only dependency, requires explicit human approval under CLAUDE.md §14.

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

### 8.1 Browser execution and the Rust/WASM option

The follow-on [browser/Node runtime proposal](ts-runtime.md) develops an async-only public JS
API, worker scheduling, cancellation and internal page/crypto suspension. It is design work;
the implementation observations and benchmark limitations below still apply.

The browser **client already returns promises** (`impl/ts/src/browser/client.ts`). A dedicated
worker runs the synchronous TS engine over synchronous OPFS access handles. That preserves the
same synchronous pager/B-tree/executor contracts as Node, keeps promises out of resident query
work, and avoids blocking the UI thread. It is an implementation choice, not a SQL requirement.

WebCrypto needs suspension **inside the worker** at decryption/cache misses and before encrypted
commit writes. Marking the existing RPC API async changes nothing. A useful prototype would keep
resident computation synchronous, add bounded asynchronous page loading/prefetch and commit
encryption, and resume execution at explicit suspension points. Cache misses currently occur
inside synchronous tree traversal and lazy decoding, so this crosses more than the BlockStore
interface. Do not replay partially evaluated SQL to recover from a miss: entropy consumption,
costs, host functions and staged writes must occur exactly once. Do not preload the whole file
to hide asynchronous reads, which would violate bounded-residency requirements.

Awaiting crypto also introduces event-loop interleavings. The worker currently dispatches
requests without an explicit serialized queue because SQL runs synchronously. An async engine
must retain the statement's snapshot/pins and writer ownership, serialize incompatible operations
(including close/rollback), and preserve publication/barrier/error-poisoning ordering. Bounded
batches of independent dirty pages may overlap encryption; parent-dependent reads cannot all
be launched together. Promise dispatch overhead, allocation, cancellation and working memory
belong in integrated measurements. No async refactor is approved or implemented by this research.

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

## 9. Open / deferred

**Tentative direction after the measurements:** prototype one suite, AES-256-GCM with fresh
derived writer keys and counter nonces, using native crypto on Node and WebCrypto in the browser.
Its measured acceleration and platform coverage make it the leading candidate. Retain XChaCha
as the comparison with a simpler nonce protocol; neither the suite nor a production dependency
is selected. Approval depends on the exact protocol/security review, supported-platform matrix,
and integrated async browser measurements. A slower pure-JS fallback and a software-WASM backend
are material deployment costs, not reasons by themselves to maintain two file suites.

The isolated linked AEAD increments were roughly 31–49 KiB in Rust and 52–72 KiB in Go for one
algorithm; the selected Noble exports were 3.7–4.6 KiB gzipped. These exclude the passphrase KDF,
page codec and lifecycle machinery; they are not estimates of the complete feature. See the
[size methodology and exact inventory](../../bench/aead/README.md#dependency-and-binary-footprint).

- **The whole feature** remains unimplemented and unscheduled; non-foreclosure is satisfied.
- **AEAD, KDF and exact library/backend selection**, including supported fallback CPUs.
- **Nonce/domain lifecycle** under failed commits, recovery, restore, writable copies and
  compaction; encoding, lifetime limits, and any determinism-ledger revision.
- **Authenticated metadata and wrapped-key format**, including bounded KDF inputs, bootstrap
  validation, both meta slots, and atomic credential changes.
- **Data-key rotation protocol**, old-key retirement and coordination with open clients.
- **Integrated performance**, async page/commit prototype and browser portability measurements,
  and protection of spill/scratch files if
  the eventual threat model includes them; the page codec alone does not encrypt those files.

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
nonce allocation must be designed independently: the database writer-key lifecycle is not a
general-purpose nonce allocator for arbitrary caller keys.

Digest/HMAC functions can be evaluated on their own merits. Password hashing, legacy ciphers,
OpenPGP and broad pgcrypto compatibility are not implied by the page-encryption dependency.
This is an optional, unscheduled SQL feature, not a prerequisite for encryption at rest.

## References

- [Envelope encryption](https://docs.aws.amazon.com/kms/latest/developerguide/kms-cryptography.html)
- [RustCrypto AES backends](https://docs.rs/aes/latest/aes/)
- [Go AES timing qualifications](https://pkg.go.dev/crypto/aes)
- [Noble ciphers and WebCrypto API](https://github.com/paulmillr/noble-ciphers)
