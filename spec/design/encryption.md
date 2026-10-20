# Encryption at rest — design (a door, not yet built)

> A deferred capability (CLAUDE.md §9, storage.md §6, [TODO.md](../../TODO.md)). This records
> the intended design so nearer work does not foreclose it and so the build is a known shape.
> Building it needs a CLAUDE.md §14 vetted-crypto decision and explicit confirmation before any
> dependency lands; we do not roll our own crypto. When a decision here changes, update
> CLAUDE.md §9 and storage.md §6 in the same edit. Decided points are marked **Decided**;
> everything else is direction.

## 1. Threat model and guarantees

The attacker can read every copy of the file at rest (backups, snapshots, crash remnants, the
replication log) and compare them offline; can write to the file at rest, including replaying
older genuine pages and swapping the file for another; and, while the process runs, can read and
write the scratch directory. The attacker cannot read the running process's memory — on any
machine that runs an engine process, including an SSH page server, which holds the key (§8), so
encryption gives no protection against the host a remote file lives on — and cannot restore a live
VM clone with duplicated RNG state.

Against that attacker jed guarantees **confidentiality of page contents** and **detection of
tampering**: a tampered or replayed page is `XX001` or a credential error, never wrong rows. Out
of scope: confidentiality of structure (the plaintext meta slot shows live size, commit count and
the pages the last commit wrote); rollback — of the whole file, of the newest commit by corrupting
one page its manifest lists (the same fallback a plaintext file makes), or by substituting another
valid database under the same credential — all of which need trusted state the engine does not
have, and a host that holds some closes all three by remembering the meta slot's tag; and secrecy
of keys in process memory (Go and TS cannot zeroize; hosts should disable core dumps and encrypt
swap, which can hold keys and plaintext pages).

**Edge cases fail closed; they are not survived.** A crash, lost connection or owner change at a
bad moment may cost the host a reopen. It may never cost security. Wherever a resume protocol
would add state or trust, the design prefers the error.

**Decided, not criteria:** no FIPS requirement; browser performance does not decide the suite
(encrypted browser databases are expected to be rare).

## 2. Where it sits

A **page codec in the core, above the block seam** (hosts.md §6): plaintext pages above, sealed
pages below. The host stays an opaque byte device. The replication tee sits below the codec, so
a replica stores ciphertext and needs no key to apply deltas (replication.md §4). The buffer pool
sits above, so cache hits do no crypto and the cost model is untouched (§7).

Standardized AEADs produce identical bytes for identical inputs, so one vetted library per core
keeps the CLAUDE.md §8 cross-core byte contract through a dependency — the opposite of LZ4, which is
hand-rolled because its encoders are not standardized (large-values.md §6).

## 3. Construction

**Decided: one single-use AES-256-GCM key per page, derived from the data key and a fresh random
192-bit salt; nothing about nonces is allocated, counted or persisted.** Every seal, including a
retried or failed commit, a restored backup or a writable copy, draws new randomness, so nonce
uniqueness needs no protocol and survives every history fork by construction. The salt and the
16-byte tag live in the page's reserved tail (§4). The exact derivation is **direction, pending
the security review**, which turns it into a format decision.

The construction is **XAES-256-GCM** ([C2SP](https://github.com/C2SP/C2SP/blob/main/XAES-256-GCM.md)),
decided 2026-10-20 as the boring choice: a published specification whose purpose is to be the
extended-nonce AES-GCM, with test vectors, a reference implementation and a published analysis
of its derivation (ePrint 2025/878). The salt is its 192-bit nonce: the first 96 bits and the
page subkey go through a two-block CMAC-AES-256 counter-mode KDF (label `X`, 16-bit counter) to
a one-message key, and the last 96 bits are the GCM nonce. No platform library but RustCrypto
exposes CMAC, so that derivation — the CMAC subkey (one AES block, one GF(2^128) doubling,
branch-free) and two single-block CMACs — is written in Go, TS, Ruby and future cores against
the C2SP vectors. That is glue over a library AES, not a primitive, and is recorded as admissible
under CLAUDE.md §14 on that basis. (A jed-specific HKDF-then-fixed-nonce layout was weighed and
set aside: it avoided the glue by inventing a composition a reviewer would have to review rather
than recognize.) A repeated salt under one data key is the hazard; fixtures check uniqueness of
the whole salt, and the collision term for `q` seals is about `q(q−1)/2^193`.
XAES is not nonce-misuse-resistant or key-committing, and need not be: one
DEK per file and rotation by compaction mean no reader ever tries two keys against a page, and a
partitioning-oracle attack on the passphrase wrap needs an attacker who can already read the
block and guess offline. KC-XAES (ePrint 2025/758) is declined for the same reason.

**Why AES-GCM.** Platform support: AES-GCM is in every platform's standard library, the raw AES
block in every one but CryptoKit (Swift leans wrap, CLAUDE.md §2), HKDF everywhere.
XChaCha20-Poly1305 is in no platform library and needs HChaCha20 that none exposes; AES-GCM-SIV
is in almost none and costs two passes. GCM-SIV fails more gracefully under duplicated randomness
and is declined for platform support alone, a knowing trade; reconsider it if live VM clones
enter the threat model.

| Platform | AES-256-GCM | Raw AES block (XAES glue) | HKDF-SHA-256 | Argon2id |
|---|---|---|---|---|
| Go | stdlib | stdlib | stdlib (1.24+) | `golang.org/x/crypto/argon2` (pure Go) |
| Rust | RustCrypto `aes-gcm` | RustCrypto `aes` | RustCrypto `hkdf` | RustCrypto `argon2` |
| Node | `node:crypto` (sync) | `node:crypto` ECB | `node:crypto` | `@noble/hashes` (already approved) |
| Browser | WebCrypto (async) | AES-CTR over one zero block with the counter set to the input | WebCrypto | `@noble/hashes` |
| Ruby (goldens, `verify.rb`) | `openssl` gem | `openssl` gem | `openssl` gem | not needed: fixtures inject the KEK |
| Java / .NET | JCA / `AesGcm` | ECB | JDK 25+ `KDF`, .NET `HKDF` | Bouncy Castle / `Konscious` |
| Swift | CryptoKit | CommonCrypto ECB | CryptoKit HKDF | third-party (Swift leans wrap) |

**Measured** (`bench/aead/`, hot memory, 8 KiB pages; the derivation timed was an HKDF layout of the same shape, XAES itself is unmeasured):
fixed-key AES-GCM seal/open is about 0.9 µs
in Go and 2.7 µs in Rust; a per-page HKDF key derivation adds 0.5 µs in Go and 0.1–0.3 µs in Rust on
open, small beside the I/O and decode a real cache miss pays. Native Node is a few microseconds
per page and WebCrypto in a real worker 7–9 µs under a fixed key (per-page `importKey` is
unmeasured). Per-commit and writer-lifetime key granularities were measured and rejected: they
need a cache or a counter protocol for a sub-microsecond saving. Reports are under
`bench/aead/results/`.

**Entropy.** Keys and salts come from the platform CSPRNG, held in the handle's `Environment`
(environment.md) as a field separate from the seeded SQL entropy source, with an explicitly
insecure fixed-bytes constructor for fixtures. Two independent streams: one per database (pages,
meta, key block) and one per handle (scratch), so per-core scratch framing cannot desynchronize
page salts under the fixed source. A commit draws its salts in one batch, after every check that
can refuse the commit and immediately before sealing, in **format.md's commit write order** (per
table post-order, index trees, catalog chain; every chain tail first), then free-list, manifest
and meta; images draw in stream order after the DEK, KDF salt and wrap nonce. In simulation each
process's streams are seeded from the trace seed and its process id, so an owner and its clients
never share a stream. The engine keeps no entropy pool across commits. A `fork`ed handle is already invalid
(locking.md §3). Live VM clones are excluded: a clone resumes with the guest RNG duplicated and
no engine-side mixing survives it; a host that restores live snapshots reseeds before the first
write.

## 4. The encrypted file variant

An encrypted file is a **distinct format variant** with its own golden fixtures (fixed key and
salts) and cost family. A plaintext file's layout is untouched.

**Reserve (Decided).** The meta slot records `reserved_bytes`; usable capacity is
`C = page_size − 16 − reserved_bytes`. A plaintext file reserves 0; an encrypted file reserves
what its suite needs (40 bytes under either §3 construction: 24-byte salt + 16-byte tag). It is self-description,
never a setting. Record caps, split points and chunk sizes follow `C` as today.

**Body pages (Decided).** Every page but the meta slots, the key block (pages 2–3) and
manifest-overflow pages — catalog,
B-tree and GiST nodes, overflow, free-list — is encrypted whole, header included (a plaintext
`page_type` or `next_page` would leak the tree's shape). Associated data: page index and the
suite/variant identifier. The body-page CRC is dropped; the tag supersedes it, and recovery's
manifest check becomes digest-only.

**Pointer binding (Decided).** Every pointer carries the full 16-byte tag of the page it names:
meta to catalog root, free-list head and manifest chain; catalog to its next page and every
root; interior to children; leaf to overflow heads; chain pages to successors. This is the
ZFS block-pointer construction. It detects **replay** — an older genuine page copied over the
current one from a backup, a crash remnant or a freed page — which per-page AEAD alone accepts
and which would otherwise serve stale rows or walk unrelated subtrees. Consequences: a commit
seals children before parents and chains tail first (the codec's `encode` returns the tag,
`decode` takes the expected one; read-ahead holds stored bytes until the parent names them);
pointers are 16 bytes wider, so fan-out at 8 KiB roughly halves (about 510 to 254 with 8-byte
keys) and an in-record external pointer grows from 9 or 13 bytes to 25 or 29, which means the
variant can **store a different set of rows** near the record cap — a user-visible divergence
recorded in the variant's goldens; and from-scratch images allocate every chain tail first so
sealing order is index order and compaction stays an append stream. The data key binds a lineage
of copies (backups, replicas, plain compactions share it), not one file; copies between them are
caught by the binding.

**Replay of a page the newest manifest lists** makes validated-COW recovery fall back one
generation (the whole pending batch under `synchronous = off`) before any binding is checked,
exactly as for an incomplete commit, which it must keep doing. Fixtures expect that fallback for
manifest-listed pages and `XX001` for every other page; the result is always a clean older
generation or an error, never a mix.

**Meta slots and manifest-overflow pages (Decided): plaintext, authenticated whole.** Nothing in
them is secret, and keeping them readable keeps slot selection and the manifest completeness
check keyless, which a replica and recovery need. The slot — including its index —
is the associated data of an empty-plaintext AEAD under the meta subkey; the AEAD input zeroes
the CRC field and reserve, the tag goes in the reserve, and the CRC-32 is computed last over the
stored bytes. A reader checks CRC, then tag. CRC pass and tag fail is `XX001`, not a fallback: a
torn write fails the CRC and a wrong key is rejected at the key block, so the case is unreachable
by accident. Manifest identities are over **stored (ciphertext) bytes**, so a keyless replica can
check a delta is complete.

**Key block (Decided).** Pages 2 and 3 hold two **byte-identical** copies of the key block, so
the catalog root is `≥ 4` and every "body pages start at 2" convention moves to 4. A copy holds a
generation counter, the suite and KDF identifiers and parameters, the KDF salt, the wrapped DEK,
and a CRC-32. It is written at `create`
and by credential changes, never by a data commit.

## 5. Keys

**Hierarchy.** A random 256-bit **DEK** per file; HKDF-SHA-256 derives two subkeys from it at
unlock, for pages and for meta/manifest authentication. A **KEK** wraps the DEK with AES-256-GCM
under a random 96-bit nonce stored in the copy, with the copy's plaintext fields as associated
data, so no parameter can be altered or downgraded without the unwrap failing. The KEK is derived
per file from the credential over the block's KDF salt, and every rewrap draws a fresh salt and
nonce, so an unchanged passphrase still yields a new KEK:
by **Argon2id** (RFC 9106) from a passphrase, or by HKDF from a host-supplied 32-byte key (never
used directly, so one host key over many files gives distinct KEKs). Argon2id is the current
recommendation for passphrases and is memory-hard, which is what defeats the offline GPU guessing
the §1 attacker can mount against a copied file; it costs a dependency in every core but Rust
(the table in §3), accepted knowingly under CLAUDE.md §14. The parameters default to RFC 9106's
second recommendation — 64 MiB memory, 3 passes, 4 lanes — with a host override, never
time-calibrated, and the format fixes a maximum memory, pass count and salt length checked
**before** the KDF runs, so a tampered block cannot make `open` exhaust memory or CPU before it
fails. The block's KDF identifier allows a later KDF without a format break; there is no second
built-in KDF today.

**Credential input.** The credential is an argument of `create`/`open`/`attach`, never a
setting, so it cannot reach `current_setting()`, `SHOW`, EXPLAIN or an error message. Rules, each
fail-closed:

| Case | Result |
|---|---|
| Wrong or tampered credential (unwrap fails) | `28P01 invalid_password` |
| Encrypted file, no credential | `28P02 credential_required` |
| Plaintext file, credential offered | `28P03 credential_not_applicable` (otherwise a file swap would silently drop encryption) |
| A connected client whose held DEK no longer authenticates the meta slot (a rotation happened) | `28P04 stale_key` — reopen |
| Credential for an in-memory database | `55000` — nothing is at rest |

**Selecting a copy.** The reader takes the **newest CRC-valid copy by generation**, before any
credential is presented, attempts the unwrap against it alone, and never falls back to the older
copy (that would let a revoked wrapper outlive its revocation). Bit rot in one copy is harmless;
in both, `XX001`. Two CRC-valid copies at the same generation with different bytes are also
`XX001`: the protocol below never writes that, so it is tampering.

**Changing the credential** rewraps the same DEK; no page is rewritten. The process holds the
writer gate and the file (page clients cannot: a client's request is refused, `55000`, and the
host changes the credential from the owner). It derives the new KEK first (Argon2id takes hundreds
of milliseconds nobody should wait for under the gate), then: write **the copy that is not the
selected one** (the older or invalid copy; page 3 when both are equal), barrier, write the other,
barrier, acknowledge. Writing the non-selected copy first is what lets a pair stay unrepaired
safely: the only valid copy is never the first overwritten, so a second crash cannot tear both.
Before its **first** key-block write of any kind a writer issues one
stabilization barrier — of the file and, after a rename, its directory — so the copy it is about
to overwrite is never the sole durable one. If the two copies differ at open, the opener that
unwrapped the newer copy copies it over the older (barrier, copy, barrier); a read-only open, or
a process without the credential, leaves the pair alone, which is safe because selection already
prefers the newer copy. The guarantee is the ordinary commit one: after an **acknowledged**
change the old credential no longer unlocks the file; after an unacknowledged one exactly one of
the two does, never neither.

**Rotating the DEK** is compaction into a new file under a new DEK and key block
(api.md §2.6), inheriting compaction's limits. Encrypting a plaintext file and decrypting an
encrypted one are the same operation; both **rebuild** every tree under the target layout (the
node-preserving compaction cannot carry packed pages across a capacity change) and fail before
the rename, with the source untouched, when a row or an index key cannot be stored under the
target (`C` is smaller and pointers wider, so `RECORD_MAX` and the key cap both fall) or the result
exceeds `max_storage_bytes`. Plain compaction keeps the DEK and key block and re-seals every page
with fresh salts. Rotation starts a new replication timeline, so keyless replicas re-seed.

**Rewrapping is not recovery from a compromised DEK.** Any retained copy of an old key block —
a backup, a snapshot, the replication log — plus its old credential still yields the DEK.
Rotation protects the live data going forward; it cannot revoke access to old copies or promise
erasure of copy-on-write remnants.

## 6. Scratch files

Spill runs, hash-operator scratch and spilled temp domains belong to the session, not to one
database, so scratch encryption is a **handle setting** fixed at `open`: `scratch_encryption =
auto | on`, where `auto` is on when main is encrypted. Attaching an encrypted database to, or
converting one through, a handle whose scratch encryption is off is refused (`55000`) on hosts
with a scratch target, so an encrypted attachment never spills plaintext through a plaintext
main. The setting never turns off during the handle's life.

Each scratch file gets a random key, drawn from the handle's scratch stream and never persisted;
blocks are sealed by the same construction as pages with that key in the DEK's place. Associated
data is the file's random id and the block ordinal, plus a per-block write counter for mutable
scratch (positional hash scratch and temp-domain pages), so a replayed older version of a
rewritten block fails; sort runs are write-once and carry a final-block flag instead. The
counters are physical state — one `u64` per block, outside the logical memory accounts, bounded
by the limits that already bound the file — because charging them would make admission depend on
per-core block framing. A temp domain keeps the plaintext page layout, so its cost and `54P03`
limit do not move; its scratch file stores each page in a `page_size + reserve` slot. Scratch
files carry no pointer bindings; the counters give freshness for the life of the process. Hosts
with no scratch target (in-memory, OPFS, WASI) have nothing to encrypt. Scratch encryption is
unmetered physical work: results and cost do not change.

## 7. Cost, determinism and conformance

Within a variant encryption is **invisible to the deterministic cost** (CLAUDE.md §13): like the
CRC and the buffer pool it is physical page work, not a metered unit. The variant has its own
`# cost:` family because `C` and pointer width differ.

Production encrypted bytes depend on the CSPRNG, so the variant carries a **determinism-ledger
entry** (`spec/conformance/determinism_exceptions.toml`), the same class as `uuidv4`. Under the
fixed-bytes source every core writes byte-identical files; that, the salt-draw order of §3, and
cross-core encrypted round trips are the contract.

The corpus runs encrypted as a harness **mode**: rows and errors as written, cost-bearing output
(`# cost:` lines, EXPLAIN estimates, limit trip points) compared across cores rather than against
the file, and a ledger of entries whose rows or errors legitimately differ — the record-cap cases
of §4 and plan-driven error visitation where fan-out changed the chosen path. The fault fixtures
are growth of the simulator (`sim_gen.rb`, simulation-testing.md): a crash at every key-block
boundary, replay at every pointer kind, restore followed by divergent writes, independently
written copies, conversion and rotation, scratch spill of every operator, and salt uniqueness
across every attempted seal.

## 8. Multiple processes and replication

**Every process of an encrypted database holds the DEK**: owner, every page client, the SSH page
server. There is no keyless owner (a keyless *read-only* owner is conceivable, since recovery is
keyless and it writes nothing; not pursued). A page client unlocks by fetching the key block
through `read_key_block` and unwrapping it with its credential. The credential is needed only to
unwrap a key block; **the DEK alone opens the file**. So a client **promotes to owner with the DEK
it holds** (page-service.md §8.1), verifying it against the meta slot's tag, and ownership
handoff, yield and the read-only owner's deferral to a writer all work as for a plaintext file; a
promoted process without the credential leaves a differing key-block pair unrepaired, which §5
makes safe. Over SSH the credential travels in `hello` on the encrypted stream, never `argv` or
the environment, or the server is configured with it; either way it reaches the remote host (§1).
A reconnect that must start a fresh process with neither DEK nor credential — an SSH helper whose
owner is gone — fails `28P02` and the host reopens; there is no cached-DEK resume.

The owner answers `read_pages` with **stored bytes** read below its codec, never a re-sealed
page: a re-seal has a fresh tag that no parent binds. Compaction, rotation and conversion allow
idle connected clients, which at their next transaction check that the meta slot still has the
variant and reserve they opened and still authenticates under their held subkey; a failure is
`28P04` (reopen) — the client cannot tell a rotation from tampering and need not, since the
reopen with the credential surfaces tampering as `28P01` or `XX001`.

**Backups** (page-service.md §9) take the key block through `read_key_block`, which the owner
serves under a key-block mutex that a credential change holds only across its two writes (not the
writer gate, so a backup never waits behind a long transaction), and write the returned copy to
both pages; plain page reads could capture a torn pair mid-change. Taking a page-identical backup
needs the DEK, since the walk follows pointers; only applying deltas is keyless.

**Replication.** A credential change is an item in the stream, ordered after the commit it
follows; the cursor is `(txid, key_generation)`. The replica applies it with the same
two-barrier protocol, non-selected copy first, so a keyless replica that crashed mid-apply and
never repaired cannot lose its only valid copy to a second crash (fixture: two interrupted
applies across a keyless restart). The primary makes the entry durable before acknowledging the change (a
rewrap has no page-reuse pressure); a key block one generation ahead of the log is rebuilt from
pages 2–3 like a single missing commit entry. A replica resumes from a base image by applying
data commits with a greater `txid` and key-block deltas with a greater generation, as two
predicates, so an image whose key block is ahead of or behind its snapshot is harmless. A
promoted replica finishes a torn pair from its log entry.

## 9. Open

- **XAES lifetime bounds across every DEK use**, and the exact library route per core (the
  RustCrypto `xaes-256-gcm` crate or the `aes` + `cmac` + `aes-gcm` composition in Rust; the
  spec-fixed glue elsewhere).
- **The security review**: the composition as a mutable page codec (the ePrint 2025/878 analysis
  covers only the CMAC derivation; the GCM composition rests on the C2SP argument), the pointer
  chain, the key-block wrap, the lifetime bounds. With the fixtures of §7 this turns direction into
  a format decision.
- **Tag only in the parent pointer?** Would cut the reserve from 40 to 24 bytes; against it, no
  standalone page verification and a rejoined copy per library call.
- **Key-block export/import** (the LUKS header-backup model), accepted only if the imported
  block's DEK authenticates the meta slot.
- **Format specification**: the reserve and pointer layouts per page type, the key block's
  encoding, the four `28Pxx` registry entries, the scratch framings, the restated variant
  arithmetic (inline manifest capacity falls to about 8 entries at 256-byte pages and with it the
  `synchronous = off` batch cap; interior fan-out with 20-byte pointers; free-list entries per
  page; `live_pages ≤ page_count − 4`), the host-API surface (credentials on `create`/`open`/
  `attach`, change-credential, compaction with a new credential or target variant,
  `scratch_encryption`, `read_key_block`, the `Environment` CSPRNG field), and the goldens.
- **Integrated performance** of an encrypted database; nothing above is a database benchmark.
- **SQL crypto functions** are a separate, unscheduled proposal: authenticated seal/open over
  byte values with explicit application keys, never the file's DEK, through the sanctioned
  entropy seam and the cost schedule. Not a prerequisite for encryption at rest.

### Removed in the 2026-10-20 simplification

Mechanisms that existed only to survive edge cases, now handled by the fail-closed rule:
a DEK-derived tag on the key block (so a process without the credential could repair it — it now
leaves the pair alone); forwarded credential changes from page clients and the per-file KEK on
the wire (owner-only now); a cached-DEK SSH resume; budget-charged scratch counter maps; a DEK id
in the key block and meta slot with an optional pin at `open` (substitution is excluded like
rollback, and a stale client learns its DEK is stale from the meta tag alone).

## References

- [XAES-256-GCM (C2SP)](https://github.com/C2SP/C2SP/blob/main/XAES-256-GCM.md), its
  [design write-up](https://words.filippo.io/xaes-256-gcm/), and
  [Benhamouda, Halevi, Kampanakis, Krawczyk, ePrint 2025/878](https://eprint.iacr.org/2025/878)
- [RFC 8452, AES-GCM-SIV](https://www.rfc-editor.org/rfc/rfc8452) §9
- [RFC 9106, Argon2](https://www.rfc-editor.org/rfc/rfc9106) (§4 parameter recommendations)
- [Envelope encryption](https://docs.aws.amazon.com/kms/latest/developerguide/kms-cryptography.html)
- [RustCrypto AES backends](https://docs.rs/aes/latest/aes/), [Go AES timing](https://pkg.go.dev/crypto/aes),
  [Noble ciphers](https://github.com/paulmillr/noble-ciphers)
- [virtio-rng entropy-leak reporting](https://lore.kernel.org/lkml/Y9lBeymca9eFaJ33@zx2c4.com/T/):
  the VM-snapshot reseed window
- Measurements: `bench/aead/README.md` and `bench/aead/results/`
