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

With FIPS ruled out (§1.1), the trade-off between the two candidates is:

| | AES-256-GCM, derived writer keys | XChaCha20-Poly1305, random nonces |
|---|---|---|
| Nonce protocol | Writer-key derivation per write ownership, counter allocation with a defined owner (including page-service clients), budget rotation, a salt or key identifier per page, a bounded derived-key cache, and a crash/restore/copy argument still to be proven | None: each encryption draws a fresh 192-bit nonce, so crash, retry, restore, writable copies, compaction and owner change are nonce-safe by construction |
| Per-page fields | 56 bytes in the direct encoding (32-byte salt, 8-byte counter, 16-byte tag) | 40 bytes (24-byte nonce, 16-byte tag) |
| Native speed (8 KiB, §8) | Faster: Go 0.87 µs, Rust 2.60 µs, plus about 0.8 µs per writer-key setup | Go 2.00 µs, Rust 4.65 µs |
| Node | Native synchronous `node:crypto`, about 4 µs | No native XChaCha (OpenSSL has only the 96-bit-nonce construction); Noble, about 30 µs |
| Browser | WebCrypto, about 8 µs, asynchronous (§8.1) | Synchronous Rust/WASM about 18 µs or Noble about 40 µs; WebCrypto has no ChaCha |
| Dependencies | Go standard library (`crypto/cipher`, `crypto/hkdf`); Rust `aes-gcm` + `hkdf` | Go needs `golang.org/x/crypto` (pure Go plus assembly, no cgo); Rust `chacha20poly1305` |

AES's protocol cost is design and verification work paid once; XChaCha's cost is slower crypto
on Node and in the browser, paid on every cache miss and commit. Because the dataset is
expected to be resident, those operations are not the steady-state hot path. The TS core's
commit, open/recovery and maintenance paths now await an asynchronous page codec (§8.1), which
removes the main browser obstacle to WebCrypto AES; the choice therefore reduces to protocol
complexity versus Node/browser crypto speed. One interaction with that write side: an encode
failure after the commit guard is armed poisons the handle, so a counter-nonce design must
reserve its allocations for a commit before the guard. Random nonces need no reservation.

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
protect subsequently cloned live state.

**Key material uses a dedicated cryptographic entropy source (decided).** Data keys, KDF salts,
writer salts, nonces and scratch-file keys are drawn from a source separate from the
general-purpose entropy seam that SQL generators use ([entropy.md](entropy.md)). A host may
install a seeded source there to make `uuidv4` reproducible; if that source also fed key
material, the data key would be predictable, and with either candidate two runs would repeat
nonces (the same writer salt yields the same derived key and counter sequence; the same random
nonces repeat directly). In production the cryptographic source reads the OS CSPRNG directly,
not a buffered userspace generator whose state a VM snapshot or process fork can duplicate.
Fixtures and simulation inject fixed bytes only through an explicitly insecure test option,
which never applies by default.

Required future fault fixtures: aborted encryption before/after each body/meta write, failed
barriers, retry and owner promotion, restore followed by divergent writes, independently opened
copies, counter/key-budget exhaustion, mixed historical epochs, compaction, credential changes,
and domain separation between body/meta/wrapping. Check both ciphertext authenticity and pair
uniqueness across every attempted encryption, not just committed pages. Whole-file anti-rollback
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
  parent, so a parent pointer (and the meta root pointer) could carry the child's write
  generation or tag, chaining authentication from the authenticated meta down the tree as ZFS
  block pointers do. The cost is wider pointers and so lower interior fan-out (another input
  to `C`, §4.1). The alternative is to exclude page-level replay from the threat model
  explicitly, as SQLCipher effectively does. Either is a decision to record before building;
  silently inheriting the gap is not.
- **Manifest page identities (open).** The validated-COW manifest hashes logical (decrypted)
  page bytes ([validated-cow.md](validated-cow.md) §5), so recovery's completeness check needs
  the key, as it does at a page-service owner ([page-service.md](page-service.md)). Defining the
  identity over on-disk bytes instead would let a keyless replica or backup check that an
  applied delta is complete, without authenticating it. Note the manifest also covers a case
  AEAD alone cannot: a page write that never reached disk leaves the older, still-valid
  ciphertext in place. Decide which bytes the manifest names when the format is specified.
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
overflow chunk size. Per-page crypto fields (40–56 bytes plus any binding, §3.1) would shrink
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
sizes must keep `C` above the structural minimum; at the 256-byte golden page size a 40–56-byte
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
pager's `PageTransform` seam, above the byte device and below the page cache, accepts
asynchronous `encodeAsync`/`decodeAsync` forms, and commit, open/recovery and maintenance await
them as retained continuations. Decryption completes before a page enters the cache; page CRCs,
manifest digests and the live count stay over plaintext logical pages. Commit encryption overlaps
in a bounded window of 16 pages while writes keep validated COW's body, free-list, manifest,
alternate-meta and one-barrier order. The commit after its last safe cancellation point is
uncancelable, and an encode failure after the commit guard is armed poisons the handle like an
I/O error, so per-commit nonce reservation must happen before the guard (§3). Only an identity
test transform exists; tests prove every write-side page operation awaits it with unchanged bytes,
results and cost. Remaining synchronous paths that an encrypted browser database would still need
to settle: `create`'s bootstrap image (the encoding of bootstrap pages is part of §6) and
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

## 9. Open / deferred

**Suite: open between two candidates** (§3.1): AES-256-GCM with fresh derived writer keys and
counter nonces (faster natively, native on Node, WebCrypto in the browser through the TS core's
asynchronous page codec) and XChaCha20-Poly1305 with random nonces (no nonce protocol to build or prove,
synchronous everywhere, slower on Node and in the browser). With FIPS ruled out, the decision is
protocol complexity versus Node/browser crypto speed. Neither the suite nor a production
dependency is selected. Approval depends on the exact protocol/security review, supported-platform
matrix, and integrated measurements. A slower pure-JS fallback and a software-WASM backend are
material deployment costs, not reasons by themselves to maintain two file suites.

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
- **Page-level replay** — parent-pointer binding versus an explicit threat-model exclusion (§4).
- **Manifest identity bytes** — logical versus on-disk, and so whether a keyless replica can
  check delta completeness (§4).
- **Reserve sizing** — the reserve per suite and binding choice, and whether plaintext files
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
