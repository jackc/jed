# LZ4 block codec — the pinned, hand-rolled compressor (Slice B)

> The byte-exact specification of jed's value compressor. Large variable-length values are
> transparently compressed before (or instead of) spilling out-of-line
> ([../design/large-values.md](../design/large-values.md) §2/§6); the compressed blob is stored in
> the record (`0x03` inline-compressed) or in an overflow chain (`0x04` external-compressed —
> [format.md](format.md) *Large values*). Because the compressed **bytes** are golden-pinned and the
> compressed **size** feeds the deterministic cost (`page_read` chain counts — large-values.md §8.3),
> the encoder must produce **identical output in every core**: Rust, Go, TS, and the Ruby reference
> each hand-roll exactly the algorithm below (no library — the §14 analysis in large-values.md §6.2).
> The vectors in [lz4_vectors.toml](lz4_vectors.toml) pin `input → exact compressed bytes`; every
> implementation must match them byte-for-byte.

## 1. The format: a faithful LZ4 *block*

The output is a standard **LZ4 block** (Yann Collet's openly published block format — no frame
header, no checksum; the surrounding record/chain supplies lengths). A block is a sequence of
**sequences**:

```
sequence = token  [lit-length extension]  literals  offset  [match-length extension]
token    = 1 byte: high nibble = literal count, low nibble = match length − 4
offset   = u16 LITTLE-ENDIAN: distance back into the output (1 … 65535; 0 is invalid)
```

- A nibble value `15` means the length continues in **extension bytes**: each `255` adds 255,
  the first byte `< 255` adds itself and terminates (so `15 + 255 + … + last`).
- The **last sequence is literals only**: the token's match nibble and everything after the
  literals are omitted. Reaching the end of the block after a literal run terminates decoding.
- Match length is stored **minus 4** (`MIN_MATCH`); a match may overlap its own output
  (offset < length), which is how runs compress.

**Little-endian exception.** The 2-byte offset is the one deliberate exception to jed's
big-endian house rule ([../design/encoding.md](../design/encoding.md)), taken so the blob stays
readable by any conformant LZ4 decoder (debuggable with off-the-shelf tools, verifiable against
reference LZ4). The blob is opaque to the rest of the format.

**End-of-block constraints** (the block format's rules, which the §2 encoder obeys):

- The **last 5 bytes** of the input are always literals (`LAST_LITERALS = 5`): a match ends at or
  before `len − 5`.
- A match must **start** at least 12 bytes before the end of the input (`MFLIMIT = 12`).
- Inputs shorter than 13 bytes therefore compress to a single all-literals sequence.
- The empty input compresses to the single byte `0x00` (a token with zero literals, no match).

## 2. The pinned encoder

LZ4 **decompression** is fully specified by the format, but **compression is not** — a conformant
encoder is free in its match search, so different libraries emit different bytes. jed pins **one**
encoder; these parameters are normative:

| constant | value | meaning |
|---|---|---|
| `MIN_MATCH` | 4 | minimum match length |
| `MAX_OFFSET` | 65535 | maximum match distance (`u16`) |
| `MFLIMIT` | 12 | no match starts after `len − 12` |
| `LAST_LITERALS` | 5 | no match extends past `len − 5` |
| `HASH_LOG` | 12 | hash table of `2^12 = 4096` entries |
| `HASH_MUL` | 2654435761 | Knuth multiplicative hash (the LZ4 constant) |

```
hash(src, p) = ((le32(src, p) * HASH_MUL) mod 2^32) >> (32 − HASH_LOG)
```

where `le32(src, p)` reads 4 bytes little-endian at `p`. The algorithm — **greedy, step 1, single
candidate per slot, no backward extension** (every branch below is normative):

```
compress(src):
  out    = []
  table  = array of 4096 entries, all EMPTY        # positions; EMPTY = "no entry yet"
  anchor = 0                                       # start of the pending literal run
  p      = 0
  limit  = len(src) − MFLIMIT                      # last legal match start (may be < 0)
  while p ≤ limit:
      h     = hash(src, p)
      cand  = table[h]
      table[h] = p                                 # store AFTER reading the candidate
      if cand ≠ EMPTY  and  p − cand ≤ MAX_OFFSET  and  le32(src, cand) == le32(src, p):
          mlen = MIN_MATCH                         # extend forward, stopping before the tail
          while p + mlen < len(src) − LAST_LITERALS  and  src[cand + mlen] == src[p + mlen]:
              mlen += 1
          emit_sequence(out, src[anchor … p), p − cand, mlen)
          p      = p + mlen                        # resume after the match
          anchor = p                               #   (positions inside the match are NOT hashed)
      else:
          p += 1                                   # step is always 1 (no acceleration)
  emit_last_literals(out, src[anchor … len))
  return out
```

`emit_sequence(out, literals, offset, mlen)`:

```
push token: min(len(literals), 15) << 4  |  min(mlen − 4, 15)
if len(literals) ≥ 15: push extension bytes for len(literals) − 15   (255* then remainder)
push the literal bytes
push offset as u16 little-endian
if mlen − 4 ≥ 15:      push extension bytes for mlen − 4 − 15        (255* then remainder)
```

`emit_last_literals(out, literals)`: the same token/extension/literal emission with a `0` match
nibble and nothing after the literals. Emitted even when the run is empty (empty input → `0x00`).

Determinism notes (each is a place a careless port diverges):

- The hash multiply is **modulo 2³²** (wrap, not widen): mask to 32 bits in languages without
  native `u32` wrap-around.
- The table stores the **latest** position whose hash mapped to the slot (overwrite), and a
  position is inserted **only where a match search ran** — never for positions covered by an
  emitted match.
- Candidate acceptance re-verifies the full 4 bytes (`le32` equality) — a hash collision is
  rejected, not trusted.
- Match extension is byte-at-a-time and stops **strictly before** the last-5-literals tail.

## 3. The decoder

Decoding is defined entirely by the format. jed's decoder takes the compressed block **and the
expected decompressed length** (`raw_len`, carried by the `0x03`/`0x04` forms — format.md) and is
**total and safe**: every read is bounds-checked, the output never grows past `raw_len`, and every
malformed block yields the structured error `data_corrupted` (never a panic / exception escape —
CLAUDE.md §13). Normative checks:

- A truncated token, extension run, literal run, or offset → `data_corrupted`.
- `offset == 0` or `offset > bytes-decoded-so-far` → `data_corrupted`.
- A literal or match copy that would exceed `raw_len` → `data_corrupted` (checked **before**
  copying, so a hostile block cannot balloon memory).
- After the final (literals-only) sequence the output length must equal `raw_len` exactly, else
  `data_corrupted`.
- Match copies proceed **byte-by-byte ascending** (an overlapping match replicates the run), the
  standard LZ4 semantic.

The decoder accepts any well-formed block — it does not (and must not) reject blocks merely
because an encoder other than §2 produced them; the §1 end-of-block constraints are encoder
obligations, not decode-time checks.

## 4. Conformance

- **[lz4_vectors.toml](lz4_vectors.toml)** pins `input → compressed` byte vectors (hex). Every
  core (and the Ruby reference) must reproduce each `compressed` exactly and decode it back to
  `input`. The vectors cover: empty / 1-byte / sub-13-byte inputs (all-literals), a long
  single-byte run (overlapping match + length extensions), a repeating multi-byte pattern,
  ≥ 15-literal runs (token extensions), an incompressible high-entropy input, and inputs straddling
  the `MFLIMIT`/`LAST_LITERALS` boundaries.
- The golden fixtures (`compressed_table.jed` — format.md) pin the codec **in situ**: the
  compressed bytes a record stores, byte-exact `rust == go == ts == ruby`.
- The compressed **size** also feeds the deterministic cost (chain `page_read` counts and the
  `value_compress`/`value_decompress` units — [../design/cost.md](../design/cost.md) §3), which is
  the second, independent reason the encoder is pinned (large-values.md §8.3).
