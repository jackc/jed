# Commit manifest: record the page's CRC-32 (landed in format v53, 2026-11-13)

**Status: landed as part of `format_version` 53 together with
[deferred-interior.md](deferred-interior.md)**, in all three cores and the Ruby reference,
unconditionally; the byte specification is [format.md](../fileformat/format.md) (*Commit manifest*,
*Page header*). The two were sequenced against each other and the answer is that they are one
change: both rewrite the meta's manifest area, and landing them apart would spend two format bumps
and write that area's capacity arithmetic twice (§3). §4 records the switch-gated work that preceded
the landing (`JED_MANIFEST_CRC32`, `JED_PAGE_TXID_STAMP`, both since removed); the landing itself is
[deferred-interior.md](deferred-interior.md) §9.10.

## 1. The problem

A commit manifest entry is a page id and the CRC-64/ECMA-182 of the page's complete bytes
([../fileformat/format.md](../fileformat/format.md) *Commit manifest*). Every body page already
carries a CRC-32/IEEE in its header, so a commit hashes each page it writes twice, and recovery hashes
each listed page twice. The CRC-64 is a byte-at-a-time loop in all three cores and was the largest
single CPU cost of a small commit in the Go core.

One 8 KiB page, one machine (Core Ultra 9 285K), single runs:

| checksum | Go | Node | safe Rust |
|---|---|---|---|
| CRC-64/ECMA-182, byte at a time (today) | 0.76 GB/s | 0.33 GB/s | 0.74 GB/s |
| CRC-64/ECMA-182, slicing-by-8 | 3.0 GB/s | 0.49 GB/s | not measured |
| CRC-32/IEEE, platform library | 52 GB/s | 7.9 GB/s | none in std |
| CRC-32/IEEE, hand-written slicing-by-8 | not measured | 1.1 GB/s | not measured |
| CRC-32C, platform library | 24 GB/s | none | none in std |
| XXH64, hand-written | 25 GB/s | not measured | 25 GB/s |
| XXH32, hand-written | not measured | 12.5 GB/s | 12.8 GB/s |

CRC-64 is a poor fit for JavaScript, which has no 64-bit integers. CRC-32C has no accelerated path
in Node, the browser, or dependency-free Rust.

## 2. The change

A manifest entry becomes `page_index u32, crc32 u32` (8 bytes instead of 12).

- **Plaintext page:** the entry's `crc32` is the page's own header checksum field. The writer copies
  it; no second hash. Recovery checks the page against its header checksum, as it does today, and
  additionally requires the field to equal the entry.
- **Encrypted page:** the page has no plaintext checksum, so the entry's `crc32` is the CRC-32/IEEE
  of the stored ciphertext. This replaces the CRC-64 pass, one for one, and stays keyless.
- **Manifest-overflow chain:** `manifest_crc64` (meta offset 48) becomes `manifest_crc32`, the
  CRC-32/IEEE of the same `page_id || page` stream, in the low 4 bytes of the same 8-byte field. The
  high 4 bytes are reserved, zero, and validated zero.

The prototype marked a meta with narrow entries by the top bit of its dirty-count field (offset 40),
so both layouts could read without a version bump. **v53 drops that bit**: the version fixes the
layout, and offset 40 is a plain count again.

What the entry binds is unchanged: the content this transaction wrote, so an older but internally
valid page at a reused address is rejected.

### 2.1 The stale-page stamp (new in this spec)

Narrowing the entry alone would drop the strength of that rejection from 64 bits to 32: a lost page
write that leaves an older, internally valid page behind is wrongly accepted with probability 2⁻³²,
per lost page write, per power failure. **The maintainer ratified a second, independent detector
rather than a wider hash** (2026-11-13), because any wider value costs a second pass over the page,
which is the entire CPU win.

**Page header offset 2**, two bytes reserved and zero through v52
([../fileformat/format.md](../fileformat/format.md) *Page header*), becomes `txid16` (u16): the low
16 bits of the txid of the commit that wrote the page. It sits inside the region the page's own
CRC-32 already covers, so it costs no extra bytes anywhere and no extra hashing.

Recovery, validating a manifest entry, now makes **two independent checks**: the page's content
against the entry's `crc32`, and the page's `txid16` against the commit window. A stale page left by
a lost write carries the stamp of the commit that really wrote it, so it fails the second check
outright — **detected with certainty**, not with probability — unless it is from a commit exactly
65,536·k earlier, where the 32-bit CRC is the backstop. The effective strength is ≈2⁻⁴⁸ in general
and exact for the realistic case.

**The window.** Under `synchronous = on` a manifest describes one commit and the check is equality
with that commit's low 16 bits. Under `synchronous = off` a cumulative manifest lists every page
written since the durability floor ([validated-cow.md](validated-cow.md) §4), and each page carries
the stamp of its *own* commit, so the check is membership in `(floor_txid, newest_txid]`:

    (txid16 − floor_txid) mod 2¹⁶  ∈  [1, newest_txid − floor_txid]

A batch is bounded by the inline manifest capacity (§3), at most 1,016 commits, far below 2¹⁶, so
the window's low-16 values are distinct and the check is exact rather than approximate.

**Scope.** Every page carrying a page header is stamped, uniformly, so the rule is total and the
writer needs no per-type case. The stamp is checked **only** during manifest validation — an
ordinary page read is validated by its CRC-32, as today. That scoping is what keeps the stamp
compatible with every rule that lets a page survive a commit unwritten: a retained interior under
[deferred-interior.md](deferred-interior.md) §9.4, a shared overflow chain under
[large-values.md](large-values.md) §15, and a kept catalog chain all keep an older stamp, and none
of them is written or listed, so none is ever checked against a window it would fail.

Neither checksum is authentication. In an encrypted file the page's 128-bit AEAD tag already
provides that, and the stamp rides inside the sealed plaintext.

## 3. Effect

- **Capacity.** A narrower entry buys back almost exactly what the deferred-interior remap region
  spends. The region takes a third of the manifest area ([deferred-interior.md](deferred-interior.md)
  §9.2); the entry shrinks 12 → 8. Landed together, the inline manifest capacity is **unchanged or
  one better than v52 at every page size**:

  | page size | variant | remap `cap` | inline `I` (v53) | v52 | deferred alone |
  |---|---|---|---|---|---|
  | 256 | plaintext | 6 | 16 | 16 | 11 |
  | 4096 | plaintext | 134 | 336 | 336 | 224 |
  | 8192 | plaintext | 270 | 678 | 677 | 452 |
  | 1024 | encrypted | 11 | 75 | 74 | 50 |
  | 8192 | encrypted | 103 | 672 | 671 | 448 |

  The inline capacity is also the `synchronous = off` batch cap, so deferred interiors cost nothing
  there when the two land together, where landing deferral alone would have cut it by a third. This
  is the concrete reason the sequencing answer is "one version, both at once."
- **CPU.** In the Go core's commit profile the CRC-64 fell from 0.61 s to 0.03 s over 6,000
  single-row commits (0.28 s to 0.05 s with deferred interiors). Commit time, `fsync` off, one run
  each, with deferred interiors on: 243 → 228 µs, 328 → 295 µs, 303 → 254 µs. The stamp is a 2-byte
  store inside a buffer the writer already owns and does not appear in the profile.
- **Bytes written** are unchanged.

The CRC-64 that remains is the page service's commit-resolution digest of the meta page, a wire
identity outside the file format.

## 4. What was run

Go core, with the switch on: the unit tests (three tests that hard-code the 12-byte layout were made
layout-aware); the conformance corpus; `mise run sim:sweep 1..100 all go` (2,200 traces); and, together
with deferred interiors, `sim:sweep 1..200 all go` (4,400 traces) and the randomized comparison
test. With the switch off the unit tests and an 880-trace sweep pass.

Not run: the simulator's regression traces (their Ruby checker reads 12-byte entries), anything
cross-core, the page-service process corpus.

**The stamp (§2.1), Go core, behind `JED_PAGE_TXID_STAMP=on`** (`impl/go/page_stamp.go`). Every
sealer is stamped with the txid the commit publishes — `snap.txid`, set before `persist` in every
path, a page client's included, since the write gate fixes its `base + 1` before it builds a page.
Recovery takes the window's floor from the other meta slot when that slot is older; when it is not
(torn, or this candidate is the fallback) the floor is unknown and the CRC-32 is the only check.
Run: the unit suite with the stamp off (unchanged, every v52 golden byte-identical) and on, alone and
with both other switches; `sim:sweep 1..20 all go` with the stamp alone (440 traces) and
`sim:sweep 1..100 all go` with all three switches (2,200 traces), every oracle passing. A stamp-off
rerun of one seed proves the switch reached the simulator: image hashes differ while recovered
generation, logical digest, live and page counts are identical.
`TestStampIsAnIndependentDetector` forges the case a narrow manifest alone cannot catch — an older
stamp on a page whose entry agrees with its CRC-32 — and requires rejection, and acceptance once the
stamp check is disabled. With every switch on, the only failures are byte comparisons against
committed v52 fixtures, all of which regenerate at v53.

**One interaction found, and fixed.** "The page header's reserved bytes are unchecked" is true of
`parsePage` but not of a **manifest-overflow page**, whose validation required header bytes 1–3 to be
zero (recovery, and the page service's delta check). A stamped overflow page therefore read as
corrupt and recovery silently fell back to an older generation — caught by three tests, not by any
stamp-specific one. The rule is now `flags == 0 && txid16 == low16(meta txid)`: a manifest page is
always written by the commit whose meta names it, under `synchronous = off` too, so it gets an exact
equality, stronger than the window. **The Rust, TS and Ruby validators carry the same `[1, 4)` zero
check** and need the same change.

`verify.rb` reads both the narrow manifest and the stamp, with the same window rule
(deferred-interior.md §9.8), so the regression traces and a 550-trace image-checking sweep agree with
it under these switches.

**Not yet covered by the stamp**, each recorded for the landing (deferred-interior.md §9.9):
the integrity check validates a live generation with no window (it holds one meta slot, not the
floor); the page service checks a client delta's page digests but not its pages' stamps; and an
encrypted page's stamp is sealed, so keyless manifest validation cannot read it — there the AEAD tag
a pointer binds is the stronger check already.

## 5. Decided

1. ~~Whether 32 bits is enough.~~ **Answered 2026-11-13:** 32 bits, plus the independent `txid16`
   stamp of §2.1. The alternative that keeps 64 bits in the entry — two seeded XXH32 passes — was
   declined because it needs a new hand-written hash in three cores and the Ruby reference and
   reintroduces the second pass over the page that this change exists to remove.
2. **Open:** whether the page header checksum itself should change. Its slow paths are the
   hand-written slicing-by-8 in Rust and the browser. Out of scope for v53.
3. **Open:** the commit-resolution digest could move to CRC-32 with the wire protocol's next
   revision. Out of scope for v53.

## 6. Work if adopted

Folded into the single v53 landing change ([deferred-interior.md](deferred-interior.md) §9.9), which
owns the shared surface. This change's own share: `format.md` (*Page header* for the stamp,
*Commit manifest*, the meta table at offset 40 and 48, inline capacity), `validated-cow.md`
(including the §4 batch window), `encrypted.md` §6, `page-service.md` and the `spec/pageservice/`
fixtures, the `cow_*` fixtures, the Ruby reference and `verify.rb`, the three cores, the simulator's
checkers. Every golden regenerates regardless, since the stamp changes every page's bytes.
