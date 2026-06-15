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

The hard constraint is §8: Rust, Go, TS (and the Ruby reference) must produce **byte-identical**
encrypted files. Compression could *not* meet this with a library — LZ4 *encoders* are not
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

- **The nonce must be deterministic — and must fold in `txid`.** Byte-identity requires a
  *deterministic* nonce (a random nonce would differ per write, breaking the golden round-trip).
  But a deterministic nonce must **never repeat for a given key over different plaintext**, or
  AEAD security collapses. A page-index-only nonce would do exactly that: copy-on-write reuses
  freed page slots (storage.md §6 free-list), so the same `page_index` is rewritten with
  *different* content. The fix is to derive the nonce from **`(page_index, txid)`** — the
  monotonic commit counter (transactions.md) guarantees every physical write of a page slot gets
  a fresh nonce, because a reused slot is always written at a strictly later `txid`. The `txid`
  is already in the meta and already part of the commit, so this costs nothing new.

- **Open sub-decision (when built): keep byte-identity, or ledger an exception.** The above keeps
  encrypted files inside the §8 contract (preferred — it preserves the cross-core/cross-host
  round-trip and the keyless-replica interop). The alternative is to declare encrypted files a
  determinism-ledger exception (like `float64` and the UUID generators, determinism.md §5) and
  permit a random nonce. **Recommendation: keep byte-identity via the deterministic
  `(page_index, txid)` nonce** — the interop is worth more than the marginal security of a random
  nonce, and AEAD with a never-repeated deterministic nonce is the standard, safe construction.

## 4. What is encrypted, what is plaintext

- **Body pages (catalog, B-tree nodes, overflow): ciphertext + auth tag.** The page payload is
  encrypted; the AEAD **authentication tag** travels with it (in the page header region — the
  16-byte header already carries a CRC slot the tag can occupy/extend, format.md). The page index
  (and `txid`) are the nonce inputs and are bound as **AAD** so a page cannot be silently
  relocated.
- **The AEAD tag supersedes the CRC for encrypted pages.** The tag detects *both* corruption and
  tampering — strictly stronger than the CRC, which detects only the former (§1). So an encrypted
  file replaces the per-page CRC with the AEAD tag rather than carrying both. This *closes* the
  end-to-end-integrity gap `format_version` 7 explicitly left open (storage.md §6).
- **The meta page (slots 0/1): plaintext header, with crypto parameters.** The meta must be
  readable before any key work to learn the page size and that the file is encrypted, so its
  fixed header stays plaintext and gains **non-secret** crypto parameters: an "encrypted" marker
  / algorithm id, the **KDF salt**, and KDF cost parameters. (The meta's *own* integrity can use
  an AEAD over its body keyed by the derived key, or keep its CRC — decided at build time.) The
  key itself is **never** stored in the file.
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
- **No determinism-ledger exception** under the §3 deterministic-nonce design — the ciphertext is
  a deterministic function of `(key, page_index, txid, plaintext)`, so two cores encrypting the
  same database with the same key produce the same bytes (§3). (The random-nonce alternative
  *would* need a ledger entry; it is not the recommendation.)

## 6. Key management is the host's job

The engine takes a **key or passphrase as a handle setting** at `create`/`open` (alongside
`cache_bytes`, `work_mem`, `read_only`, `max_cost` — api.md §2.1, §8): a passphrase is run
through the KDF with the meta's salt to derive the page key; a raw key is used directly. The
engine **never persists the key** and never manages key storage, rotation policy, or escrow —
those are the host's / OS keychain's concern. This keeps the engine out of the key-management
business, which is correctly the embedder's. Key **rotation** (re-encrypt under a new key) is a
later concern; the copy-on-write model makes it expressible as a rewrite pass, not foreclosed.

## 7. The §14 gate

Building this **requires a third-party crypto dependency** in each core (AES-GCM / ChaCha20-
Poly1305 + a KDF — Argon2id or scrypt). That is governed by CLAUDE.md §14:

- It is the **crypto clause** (§14.3): we do not hand-roll crypto; primitives come from a vetted,
  well-reviewed library — *and* the **clause-1** case (a per-language equivalent exists in every
  core such that behavior stays byte-identical and deterministic, §3).
- It must satisfy every §14 guardrail: **memory-safe** (no `unsafe`/cgo/FFI — the Go core stays
  pure Go, which constrains the library choice to a pure-Go AEAD), **deterministic + cross-core
  byte-identical** (§3), and **bounded surface** (the codec edge, never the parser/planner/
  executor core).
- Per §14, **the dependency is never added on an agent's initiative** — it is proposed, the
  justifying clause named, and a human says yes. So the *design* is recordable now; the *build*
  starts with that confirmation.

## 8. Open / deferred

- **The whole feature** — ⏳ a door, not scheduled. Non-foreclosure (§1) is the only present
  requirement and is already satisfied.
- **AEAD + KDF selection** — ⏳ at build time, gated on §14 confirmation (§7); pure-Go
  availability is the binding constraint for the Go core.
- **Byte-identity vs. ledger-exception nonce** — ⏳ recommendation recorded (deterministic
  `(page_index, txid)` nonce, keep §8 byte-identity — §3); finalized at build.
- **Meta integrity (AEAD vs. retained CRC) and tag placement in the header** — ⏳ at build,
  within the format's existing header flex (§4).
- **Key rotation / re-encryption** — ⏳ not foreclosed (§6); a copy-on-write rewrite pass.
