# Commit manifest: record the page's CRC-32 (proposal)

**Status: proposal, prototyped in the Go core only**, behind `JED_MANIFEST_CRC32=on`. Nothing here is
in `format.md`, the Ruby reference, or the Rust and TS cores, and the prototype does not bump the
format version.

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
- **Manifest-overflow chain:** `manifest_crc64` (meta offset 48) becomes the CRC-32/IEEE of the same
  `page_id || page` stream, in the same 8-byte field.

The prototype marks a meta with narrow entries by the top bit of its dirty-count field (offset 40), so
both layouts read without a version bump. A real format version would drop the bit.

What the entry binds is unchanged: the content this transaction wrote, so an older but internally
valid page at a reused address is rejected. The strength drops from 64 bits to 32: such a page is
wrongly accepted with probability 2⁻³², per lost page write, per power failure. Neither checksum is
authentication.

## 3. Effect

- **Capacity.** The inline manifest of an 8 KiB meta holds 1,016 entries instead of 677 (675 instead
  of 450 beside a remap list, [deferred-interior.md](deferred-interior.md)). The inline capacity is
  also the `synchronous = off` batch cap, so a batch holds half again as many pages.
- **CPU.** In the Go core's commit profile the CRC-64 fell from 0.61 s to 0.03 s over 6,000
  single-row commits (0.28 s to 0.05 s with deferred interiors). Commit time, `fsync` off, one run
  each, with deferred interiors on: 243 → 228 µs, 328 → 295 µs, 303 → 254 µs.
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

## 5. Open questions

1. Whether 32 bits is enough. The alternative that keeps 64 bits and stays fast in every core is two
   XXH32 passes with different seeds (about 1.4 µs per page in Node); it needs a new hand-written
   hash in three cores and the Ruby reference.
2. Whether the page header checksum itself should change. Its slow paths are the hand-written
   slicing-by-8 in Rust and the browser.
3. The commit-resolution digest could move to CRC-32 with the wire protocol's next revision.

## 6. Work if adopted

`format.md` (*Commit manifest*, meta table, inline capacity), `validated-cow.md`, `encrypted.md` §6,
`page-service.md` and the `spec/pageservice/` fixtures, the `cow_*` fixtures, the Ruby reference and
`verify.rb`, the three cores, the simulator's checkers.
