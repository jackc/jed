# Encrypted file variant — byte spec

> The bytes of an encrypted database file ([../design/encryption.md](../design/encryption.md)). Read
> [format.md](format.md) first: this document states only where an encrypted file differs from the
> plaintext layout, which is untouched. Shared vectors: [encryption_vectors.toml](encryption_vectors.toml);
> goldens: `fixtures/encrypted_*.jed`, generated and checked by [verify.rb](verify.rb). When a decision
> here changes, update encryption.md and [CLAUDE.md](../../CLAUDE.md) §9 in the same change.

An encrypted file is the same page-structured copy-on-write B+tree as a plaintext file, at the same
`format_version` (49). A meta slot whose `suite` byte is `0` describes a plaintext file and every other
byte rule of format.md applies unchanged; a reader that predates this variant rejects a nonzero
`suite` as a nonzero reserved field (`XX001`), so no version bump was needed.

## 1. Constants and the suite

| name | value | meaning |
|---|---|---|
| `suite` | `1` | XAES-256-GCM pages, HKDF-SHA-256 subkeys, AES-256-GCM key wrap (§3) |
| `R` | `40` | reserved bytes at the end of every sealed page: 24-byte nonce ‖ 16-byte tag |
| `T` | `16` | tag bytes a bound pointer carries |
| `C` | `page_size − 16 − R` | the usable payload of every body page |
| `FIRST_BODY` | `4` | pages 0–1 are meta slots, 2–3 the key block (§5) |
| minimum `page_size` | `1024` | `create` with a credential below it is `0A000`; a file below it is `XX001` |

Every capacity of format.md that is stated in terms of `C` keeps its formula with this `C`
(`RECORD_MAX`, the split rules, `leafOverhead`). Every rule that names "page 2" as the first body page
names page 4: a body page index is in `[4, page_count)`, a from-scratch image starts allocating at 4,
`live_pages ≤ page_count − 4`, and a from-scratch image's `live_pages` is `page_count − 4`.

## 2. Bound pointers

A **bound pointer** is `u32 page ‖ tag[16]`: the page index, then the 16-byte AEAD tag of the stored
bytes of that page (§4). A pointer to no page (`0`) carries sixteen zero bytes. Every pointer of an
encrypted file is bound, so a reader that follows it checks the target's stored tag against the
pointer's before it decrypts, and a replayed page — older genuine bytes at the same index — fails as
`XX001` (encryption.md §4, the ZFS block-pointer construction).

Where each pointer lives:

| page / entry | plaintext layout | encrypted layout |
|---|---|---|
| meta | `root_page` (20), `free_list_head` (28) | unchanged, plus `root_tag` at 64 and `free_list_tag` at 80 (§6) |
| table catalog entry | `root_data_page u32`, `index_root_page u32` | each followed by its 16-byte tag |
| interior node (`page_type 3`) | `N+1` × `u32` children | `N+1` × bound pointer (20 bytes each) |
| GiST interior (`page_type 6`) | entry `… ‖ child_page u32` | entry `… ‖ child_page u32 ‖ tag` |
| leaf external value (`0x02`) | `0x02 ‖ first u32 ‖ len u32` (9 bytes) | `0x02 ‖ first u32 ‖ len u32 ‖ tag` (25) |
| leaf external value (`0x04`) | `0x04 ‖ first u32 ‖ stored_len u32 ‖ raw_len u32` (13) | `… ‖ tag` (29) |
| chain pages: catalog `1`, overflow `4`, free-list `7` | `next_page` in the header | `next_page` in the header, its tag the first 16 payload bytes |

Consequences, each a pure function of the variant:

- **Interior payload** is `20·(N+1) + 4·N + Σ sep_len`; the split rules of format.md *Fan-out* use this
  formula unchanged otherwise (`leftpayload(m) = 24·m + 20 + Σ_{i<m} sep_len(i)`). The
  minimum-fanout invariant `44 + RECORD_MAX(0) ≤ C` holds for `C ≥ 76`.
- **Record disposition** (format.md *Large values*) uses the external-pointer sizes 25 and 29: a value
  is an externalize candidate when its current encoded size exceeds 25 (plain) or 29 (compressed),
  and an externalized value weighs 25 or 29 in `record_size`. An encrypted database can therefore
  store a different set of rows near the record cap than a plaintext one — the same class of
  difference as a different page size.
- **Chain pages** hold `C − 16` bytes of content after the successor tag: an overflow page carries a
  slab of up to `C − 16` bytes (`item_count` = its slab length), a free-list page `(C − 16) / 4`
  entries, a catalog page `C − 16` bytes of entries. An over-long catalog entry spans
  `ceil(len / (C − 16))` pages. The last page of a chain carries a zero tag.
- **GiST** nodes fit `C`; an interior entry is 16 bytes longer.

## 3. Cryptographic constructions

All are standardized; every core uses a vetted library for AES, AES-GCM, HKDF and Argon2id
(CLAUDE.md §14). Only the XAES key derivation is written per core, from its specification.

**XAES-256-GCM** is [C2SP XAES-256-GCM](https://c2sp.org/XAES-256-GCM) exactly: for a 256-bit key
`K` and a 192-bit nonce `N`, `L = AES-256_K(0^128)`; `K1` is `L` doubled in GF(2^128) (shift left one
bit, XOR `0x87` into the last byte when the shifted-out bit was 1); `M1 = 00 01 58 00 ‖ N[0:12]`,
`M2 = 00 02 58 00 ‖ N[0:12]`; `Kx = AES-256_K(M1 ⊕ K1) ‖ AES-256_K(M2 ⊕ K1)`; then AES-256-GCM
under `Kx` with nonce `N[12:24]` and a 16-byte tag. `L` and `K1` depend only on `K` and are computed
once at unlock. The two C2SP vectors are in [encryption_vectors.toml](encryption_vectors.toml).

**Subkeys.** At unlock, from the 32-byte data key `DEK`:

```
page_key = HKDF-SHA-256(ikm = DEK, salt = "" (32 zero bytes, RFC 5869), info = "jed/v1/page", L = 32)
meta_key = HKDF-SHA-256(ikm = DEK, salt = "",                           info = "jed/v1/meta", L = 32)
```

**Key-encryption key.** The KEK is derived per key-block copy (§5) from the credential and the copy's
`kdf_salt`:

- `kdf = 1`, a passphrase: `Argon2id(password = the passphrase's UTF-8 bytes, salt = kdf_salt,
  t = kdf_t, m = kdf_m KiB, p = kdf_p, version 0x13, no secret, no associated data, tag length 32)`
  (RFC 9106).
- `kdf = 2`, a host-supplied 32-byte key: `HKDF-SHA-256(ikm = key, salt = kdf_salt, info =
  "jed/v1/kek", L = 32)`.

**Wrap.** `wrapped_dek = AES-256-GCM(key = KEK, nonce = wrap_nonce, plaintext = DEK, aad = the copy's
bytes [0, 76))` — 32 bytes of ciphertext then the 16-byte tag. A failed unwrap is `28P01`.

**Associated data.** Every page seal binds its role and position:

```
body AD = "JEDB" ‖ u8 suite ‖ u8 0x01 ‖ u32 page_index
meta AD = "JEDB" ‖ u8 suite ‖ u8 0x02 ‖ u32 slot ‖ meta' ‖ for each manifest page in chain order: u32 id ‖ its stored bytes
```

where `meta'` is the meta page with its CRC field (32–36) and its reserve (`[page_size − 40,
page_size)`) zeroed.

## 4. Body pages

Every page but the meta slots, the key block and manifest-overflow pages — catalog, leaf, interior,
overflow, GiST leaf and interior, free-list — is sealed whole. Its **plaintext** is the first
`page_size − R` bytes of the page as format.md lays it out with payload capacity `C`: the 16-byte
header with its `crc32` field **zero** (the tag supersedes it), the payload, zero fill. The stored page
is:

| offset | size | field |
|---|---|---|
| 0 | `page_size − 40` | XAES-256-GCM ciphertext of the plaintext |
| `page_size − 40` | 24 | `nonce` — fresh random bytes (§8) |
| `page_size − 16` | 16 | `tag` — the AEAD tag, the value every bound pointer to this page carries |

under `page_key` with the body AD. A reader that follows a bound pointer compares the stored tag with
the pointer's (a mismatch is `XX001`), then decrypts; an authentication failure is `XX001`, and so is
a nonzero `crc32` field or any reserved header field the plaintext rules forbid. The plaintext then
parses exactly as format.md's page of payload `C`.

## 5. Key block (pages 2 and 3)

Two **byte-identical** copies, plaintext, each:

| offset | size | field |
|---|---|---|
| 0 | 4 | `magic` = `4A 45 44 4B` (`JEDK`) |
| 4 | 2 | `key_block_version` (u16) = `1` |
| 6 | 1 | `suite` = `1` |
| 7 | 1 | `kdf` — `1` Argon2id passphrase, `2` HKDF host key |
| 8 | 8 | `generation` (u64), `≥ 1`; `1` at `create` |
| 16 | 4 | `kdf_m` (u32) — Argon2id memory in KiB; `0` for `kdf = 2` |
| 20 | 4 | `kdf_t` (u32) — Argon2id passes; `0` for `kdf = 2` |
| 24 | 4 | `kdf_p` (u32) — Argon2id lanes; `0` for `kdf = 2` |
| 28 | 4 | reserved (0) |
| 32 | 32 | `kdf_salt` |
| 64 | 12 | `wrap_nonce` |
| 76 | 48 | `wrapped_dek` (32-byte ciphertext ‖ 16-byte tag) |
| 124 | 4 | `crc32` — CRC-32/IEEE of `[0, 124)` |
| 128 | rest | zero |

**Bounds, checked before any KDF runs** (so a tampered block cannot make `open` exhaust memory or CPU):
for `kdf = 1`, `1 ≤ kdf_p ≤ 16`, `1 ≤ kdf_t ≤ 16`, `8·kdf_p ≤ kdf_m ≤ 1048576` (1 GiB); for `kdf = 2`
the three fields are zero. Any other `kdf`, `suite`, version, magic, nonzero reserved or tail byte is an
invalid copy. The default parameters at `create` are RFC 9106's second recommendation, `m = 65536`,
`t = 3`, `p = 4`; a host may override them within the bounds.

**Selecting a copy** (encryption.md §5): a copy is valid when its CRC and every field rule above hold.
The reader takes the valid copy with the greater `generation`; two valid copies of equal generation
must be byte-identical (else `XX001`); no valid copy is `XX001`. It unwraps with that copy alone and
never falls back to the other.

A key block is written only by `create`, by compaction (which copies the selected copy to both pages
of the new file), and by a credential change.

**Changing the credential** rewraps the same DEK; no other page is written. The new copy has
`generation` one greater than the selected copy, the new credential's `kdf` and parameters, and a fresh
`kdf_salt` and `wrap_nonce`. The writer, holding the writer gate and the file:

1. derives the new KEK before taking the gate;
2. issues a **stabilization barrier**: flushes any pending `synchronous = off` batch, syncs the file, and
   syncs its directory once per handle (encryption.md §5);
3. writes the new copy over the copy that is **not selected** — the invalid or older copy, or page 3
   when both are valid and equal — then a barrier;
4. writes it over the other page, then a barrier, and only then acknowledges.

A crash before step 3's barrier leaves the old credential working; after it, the new one; never neither.
A failure poisons the handle, which must be reopened. Under the gate the writer first checks that its new
copy's `generation` is still one past the selected copy's; a change that lost a race with another is
refused (`55006`) before it writes, so two different copies of one generation are never written.

**Repair at open.** When the two pages differ (one invalid, or an older generation), a writable open that
unwrapped the selected copy writes it over the other page between two barriers. A read-only open leaves
the pair alone; selection already prefers the newer copy.

## 6. Meta slots

The plaintext meta layout of format.md, with:

| offset | size | field |
|---|---|---|
| 6 | 1 | `suite` (u8) — `0` plaintext, `1` this variant; any other value `XX001` |
| 7 | 1 | `reserved_bytes` (u8) — `0` when `suite = 0`, `40` when `suite = 1`; anything else `XX001` |
| 64 | 16 | `root_tag` — the tag of the catalog chain's head page |
| 80 | 16 | `free_list_tag` — the tag of the free-list head page, zero when `free_list_head = 0` |
| 96 | variable | the inline manifest, `I = floor((page_size − 96 − 40) / 12)` entries, then zero |
| `page_size − 40` | 24 | `nonce` |
| `page_size − 16` | 16 | `tag` |

`root_page ≥ 4`; a nonzero `free_list_head ≥ 4`; manifest entries and chain pages are in `[4, page_count)`.

**Sealing.** With every other field written and the inline manifest in place: seal an **empty
plaintext** under `meta_key` with a fresh nonce and the meta AD (§3) — which covers the slot index, the
whole meta page but its CRC and reserve, and every manifest-overflow page in chain order — and store
the nonce and tag in the reserve. Then compute the meta CRC-32 of format.md over the stored bytes.

**Reading.** Slot selection is keyless and exactly format.md's: magic, version, CRC, bounds, and the
complete manifest. In this variant a manifest entry is checked by its CRC-64 alone — the body page is
ciphertext, so the plaintext rule that a listed page has a valid type and CRC-32 does not apply — and
manifest identities are over **stored** bytes. After selection, the reader that holds the keys checks
the selected meta's tag; a failure is `XX001`, never a fallback to the other slot (a torn write fails
the CRC, and a wrong key is refused at the key block, so a CRC-valid meta that fails its tag was
tampered with). Manifest-overflow pages keep format.md's plaintext layout and CRC-32 unchanged; the
meta tag authenticates them.

## 7. Seal order

A page is sealed once its contents are final, so every child is sealed before the page that points at
it. Both the from-scratch image and an incremental commit seal in this order, which is also the order
they draw nonces (§8):

1. For each table in name order, its data tree and then each index tree in catalog order (a GiST tree
   in its own post-order), each in **post-order**. At a leaf, first every **freshly written** overflow
   chain of its records, in (record, column) order, each chain **last page first**; then the leaf.
   An incremental commit seals only dirty nodes; a kept chain is not resealed.
2. The catalog chain, **last page first**.
3. The free-list chain, **last page first** (incremental commits only).
4. The meta slot(s): an incremental commit's one slot; a from-scratch image's slot 0, then slot 1.

Page **allocation** (which index each page gets) is format.md's, unchanged: sealing order and index
order differ, and a from-scratch writer may emit pages in any index order.

## 8. Nonces and entropy

Every seal draws a fresh 24-byte nonce from the database's key-entropy stream; nothing about nonces
is allocated, counted or persisted. `create` draws, in order, the 32-byte `DEK`, the 32-byte
`kdf_salt`, and the 12-byte `wrap_nonce`, then the from-scratch image's nonces in seal order.
Compaction draws only the image's nonces (it keeps the DEK and key block). A credential change draws
the new copy's 32-byte `kdf_salt`, then its 12-byte `wrap_nonce`, when it is called, before taking the
gate; `encryption_vectors.toml`'s `[rewrap]` pins one.

The key-entropy stream is the environment's key-entropy source (environment.md), separate from the
SQL random source: the OS CSPRNG in production. Tests and the goldens install an explicitly insecure
**seeded** source, under which each database opened through the environment draws its own splitmix64
stream from the seed (entropy.md §2: big-endian 8-byte chunks, a final partial chunk the high bytes of
one more draw — so each draw of `n` bytes consumes `ceil(n / 8)` words, and the 12-byte wrap nonce
two). Under the seeded source every core writes byte-identical encrypted files for the same
history. A commit that fails after it began sealing may have drawn an unspecified number of nonces.

## 9. Credentials and errors

The credential is a `create` / `open` / `attach` argument — a passphrase or a 32-byte host key — never a
setting, so it reaches no SQL surface or error message.

| Case | Result |
|---|---|
| Unwrap fails (wrong credential, wrong kind, tampered copy that kept its CRC) | `28P01 invalid_password` |
| Encrypted file, no credential | `28P02 credential_required` |
| Plaintext file, credential offered | `28P03 credential_not_applicable` |
| In-memory database with a credential | `55000 object_not_in_prerequisite_state` |
| `create` with a credential and `page_size < 1024` | `0A000` |

(`28P04 stale_key` is registered for the page-service follow-on and not yet raised.)

## 10. Not yet built (fail closed)

- **Replication** (not built for any file): a credential change's key-block item in the stream.
- **A credential carried in `hello`** for a `jed pages` server that must own an encrypted database;
  today such a server is given a key or passphrase file, or fails `28P02` (a relay needs none).

## 11. Scratch files

Scratch encryption (encryption.md §6) is a **handle** setting fixed at `open`/`create`,
`scratch_encryption = auto | on`; `auto` is on when the main database is encrypted. With it on, every
scratch file the handle's spill host makes — sort runs, row spools, hash partitions, a spilled temp
domain's page store — is sealed; with it off, attaching an encrypted database is `55000` on a host that
has a scratch target. Hosts with none (in-memory, OPFS, WASI) have nothing to seal. Scratch bytes are
never durable, never reopened and never read by another process or core, so this framing is not a
cross-core byte contract and has no goldens; it is stated so the cores agree on what they protect.

- Each file draws a random 32-byte key and a random 16-byte file id from the handle's **scratch
  stream** — a key-entropy stream of its own, so it never perturbs a database's stream.
- The file's logical bytes are cut into 4096-byte blocks. Block `i` is stored at `i·4136` as its
  XAES-256-GCM ciphertext (always the whole block, zero-padded), its 24-byte nonce and its 16-byte tag.
- The associated data is `"JEDB" ‖ suite ‖ 0x03 ‖ file_id ‖ u64 i ‖ u64 counter`, where `counter` is
  the block's write count, kept in memory (one `u64` per block, outside the logical memory accounts)
  and incremented at every seal. A counter is **never reset**, truncation included, so no version
  sealed before a newer one can authenticate again; a block never written, or cut by a truncation and
  not rewritten since, reads as zeros. A replayed older version of a
  rewritten block, a block moved to another ordinal or file, and any altered byte fail to open, an I/O
  error (`58030`) to the operator reading it.
- The logical size is kept in memory; truncation zeroes the cut part of a kept block.
- A core may buffer the block most recently written in memory and seal it when another block is
  written, so a run of small writes into one block seals it once.

Results, cost and every limit's trip point are unchanged: scratch encryption is unmetered physical work.

## 12. Rekey: DEK rotation and conversion

A **rekey** rewrites a file database under a new data key: encrypted for a new credential — rotating an
encrypted file's DEK, or encrypting a plaintext file — or plaintext, decrypting an encrypted one. It is
compaction (api.md §2.6) with three differences:

1. **Every tree is rebuilt** under the target layout, since node-preserving compaction cannot carry a
   packed page across a capacity change. Each table's data tree and each B-tree or GIN index tree of
   the result is the tree that **inserting its records in ascending key order into an empty tree**
   produces at the target's `C` and pointer width, every value fully materialized (so its storage —
   inline, compressed or in an overflow chain — is re-planned at the target capacity); a GiST tree is
   rebuilt from its leaf keys as always. Catalog entries, row counts, rowid counters, statistics and
   every other catalog object are kept. The image is then the ordinary from-scratch image of that
   snapshot at `txid + 1`, sealed as §7 says.
2. **Draws.** An encrypted target draws, from the database's key-entropy stream (a new stream for a
   plaintext source, the open database's own for an encrypted one), the new 32-byte DEK, `kdf_salt`
   and `wrap_nonce` (a new key block at `generation = 1`), then the image's nonces in seal order. The
   golden `encrypted_rekey_index_table.jed` pins it: a plaintext file holding `encrypted_index_table`'s
   rows (built in any order), rekeyed at `txid 155`.
3. **Failure before the rename.** A record or index key the target cannot store (an encrypted page holds
   40 bytes less and its pointers are wider, so `RECORD_MAX` and the key cap fall), or a result whose
   live pages exceed `max_storage_bytes` (`54P06`), fails before the file is replaced.

The rebuild runs in a **scratch domain** in the target layout — a temp domain sealed under a throwaway
random key when the target is encrypted — whose pages move to a scratch file of the handle's spill host
once they pass the handle's page-cache budget, so memory stays bounded. Encrypting is refused (`55000`)
through a handle whose scratch files are plaintext (§11), and an encrypted target needs a page size of
at least 1024 (`0A000`). A rekey keeps compaction's rules: it never waits (`55006` with a reader, a
write transaction or another process), and it starts a new key lineage, so earlier copies of the file
still open with their old credential.

## 13. Backups

Both backup forms of page-service.md §9 work on an encrypted database, from its owner or a page client
(§14):

- **Page-identical**: the walk follows every chain through its bound pointers, so chain pages and
  interior nodes are authenticated as they are read; every copied page keeps its stored (ciphertext)
  bytes at its index. Pages 2 and 3 both receive the database's **selected key-block copy** — the one a
  credential change replaces only after writing both pages — so the copy holds a complete pair at one
  generation even while a change runs. Both meta slots carry the bootstrap meta of the pinned version
  (its root and free-list tags at 64 and 80), each sealed for its own slot index.
- **Compacted**: the from-scratch image at `txid + 1` under the same DEK and the database's selected
  key-block copy (read from pages 2 and 3, as for the page-identical form, so a page client that
  unlocked before a credential change copies the current pair), every page resealed with fresh nonces
  from the database's key-entropy stream, as compaction does.

Either copy opens with the database's credential at the time of the backup.

## 14. The page service

An encrypted database is served like a plaintext one (page-service.md); every process that reads or
writes its pages holds the DEK. No message is added to the wire protocol (spec/pageservice/):

- **Unlocking a client.** A page client learns the variant from the meta in `read_begun` (offset 6) and
  unlocks with its own credential, reading pages 2 and 3 through `read_pages` like any page and
  selecting a copy by §5. A credential change in progress never leaves both copies invalid, so this
  needs no lock; no credential is `28P02`, a wrong one `28P01`, a credential for a plaintext database
  `28P03`. Pages cross the connection as stored bytes and the client opens them with its own codec.
- **The served meta has no slot.** `read_begun` carries the meta's bytes, not the slot that holds them,
  so a client authenticates a served meta under either slot index. It seals its commit's meta for
  either index; the owner checks the tag (under either index — a meta that authenticates under neither
  is `08P01`, a client bug, before any page is written), then **reseals** the meta for the slot it
  writes, with a nonce of its own, and publishes those bytes. A commit's `commit_status` digest is
  therefore taken with the meta's CRC field and reserve zeroed (spec/pageservice/README.md §3.5), so
  the resealed meta and the one the client sent resolve alike. A delta's inline manifest starts at 96,
  it may not write pages below 4, and its body pages are checked by their manifest identity alone.
- **Credential change** is the owner's: a page client's request is `55000`.
- **Promotion.** A client that promotes itself opens the file with the **data key it holds**, not its
  credential, so a credential changed since it unlocked does not stop it. It adopts the file's selected
  key-block copy and leaves a differing pair unrepaired (§5 makes that safe). The meta is
  authenticated under the held key; a file whose meta slots the held key authenticates in neither
  (the owner rekeyed it before it left) is `28P04`, as for a client that stayed connected, and a reopen
  with the credential then surfaces any tampering as `28P01` or `XX001` (encryption.md §8).
- **Reconnecting.** A client that connects again — to the same owner after a lost connection, or to a
  new one after a handoff — likewise keeps the data key it holds: it reads pages 0–3 through the
  service, adopts the selected key-block copy, and authenticates the served meta under the held key. A
  file decrypted since is `28P04`, checked before the file's size, and so is a meta the held key does
  not authenticate (a rekey since), as at the next transaction of a client that stayed connected.
- **Rekey under the service** is the owner's, under compaction's rules (no client may pin a snapshot).
  A client that stays connected finds, at its next transaction, a meta its held key no longer
  authenticates (or a plaintext one): `28P04 stale_key`, and it reopens with the new credential. A
  plaintext client whose database was encrypted meanwhile gets `28P02`.
- **Backups** through a client read pages 2 and 3 through the service and write the selected copy to
  both (§13).
- **`jed pages`** relays ciphertext without a key. A `jed pages` that must own an encrypted database
  needs one (`--key-file` / `--passphrase-file`), else `28P02`.

