# Large values — overflow pages + transparent compression (design)

> The reasoning behind how the engine stores a value too large to keep inline in a B-tree
> record: pushing it **out-of-line** onto a chain of overflow pages, and (optionally)
> **compressing** it first. This is jed's equivalent of PostgreSQL **TOAST** — one subsystem
> with two tools for the same job. This is a **design** doc for a **deferred** feature: it
> fixes the *model* and the cross-core contracts so the eventual implementation has a spec to
> conform to; the *bytes* land in [../fileformat/format.md](../fileformat/format.md) and the
> cost units in [../cost/schedule.toml](../cost/schedule.toml) when it is built. When a decision
> here changes, update [CLAUDE.md](../../CLAUDE.md) §9, [storage.md §6](storage.md),
> [../fileformat/format.md](../fileformat/format.md), and [types.md](types.md) in the same edit.

**Status: Slice A (overflow / out-of-line storage) BUILT — Slice B (compression) design-only.**
Slice A landed in all three cores + the Ruby reference as `format_version` 3 (the bytes in
[../fileformat/format.md](../fileformat/format.md) "Large values", goldens incl.
`overflow_table.jed`; the resolved decisions in §12 below; the `page_read` accrual in
[cost.md](cost.md) §3). Compression (§6, forms `0x03`/`0x04` — reserved in v3, additive when
built) remains deferred (TODO.md). Per CLAUDE.md §14 a third-party dependency is **never** added
on an agent's initiative — and as §6 below shows this feature needs **none**, which is part of
the point.

---

## 1. The problem: two size ceilings, one B-tree invariant

Today a value lives **inline** in its B-tree record ([format.md](../fileformat/format.md)
*Record*): the record is `2 + key_len + Σ value_size` bytes, packed into a leaf or interior
node page. Two independent ceilings cap how large a value may be, and **both trip a write-side
`0A000`** (`feature_not_supported`):

1. **`RECORD_MAX = (C − 12) / 2`** — at the 8 KiB default, **~4084 bytes** per *record*
   ([format.md:243](../fileformat/format.md)). This is the binding limit (it bites first) and
   it is **not arbitrary**: the *Why the record cap* proof ([format.md:322](../fileformat/format.md))
   shows capping a record at `C/2` is exactly what guarantees the B-tree's **all-2-way-split,
   no-empty-node, no-multi-way-spill** invariant across four implementations. Lift it naively
   and the split/merge byte contract breaks.
2. **The value codec's `u16` length field** — a `text`/`bytea` body is a `u16` byte-length
   then the bytes ([format.md:341](../fileformat/format.md)), so a single value over **65535
   bytes** is a separate `0A000`, independent of `RECORD_MAX`.

The key observation that shapes this whole subsystem: in jed the *purpose* of moving a value
out-of-line is **not merely "the value won't fit a page."** It is **"shrink the record's inline
footprint back under `RECORD_MAX` so the B-tree invariant holds."** Compression serves the
*same* goal — it shrinks the inline footprint. That is precisely why PostgreSQL unifies
compression and out-of-line storage into one mechanism (TOAST), and why jed should too: they
are two tools for one job, designed together, sharing one per-value disposition flag space.

This subsystem lifts both ceilings and unblocks several deferred items that depend on
over-page values: raising `decimal`'s 1000-digit cap ([types.md §12](types.md), gated
explicitly on "overflow pages / TOAST"), and the headline `json`/`jsonb` and `array` types
(TODO.md Phase 3).

---

## 2. The model: a TOAST-equivalent per-value disposition

Every variable-length value (`text`, `bytea`, large `decimal`, future `json`/`array`) gets one
of **four dispositions**, recorded per value in the record (§5):

| disposition | where the bytes live | record holds |
|---|---|---|
| **inline-plain** | in the record, verbatim | the value body (today's behaviour) |
| **inline-compressed** | in the record, compressed | the compressed blob + original length |
| **external-plain** | in an overflow-page chain | a fixed-size **pointer** (first page, length) |
| **external-compressed** | in an overflow-page chain, compressed | a pointer + compressed length + original length |

Fixed-width values (`int*`, `boolean`, `uuid`, `timestamp*`) are **always inline-plain** — they
are small and never spill. Disposition is a property of a *stored value*, decided at write time
by the rule in §3; on read, the value is materialized transparently (§7) — the SQL layer never
sees the disposition.

This mirrors PostgreSQL's varlena `{plain, compressed, external, external-compressed}` states.
Like PG, jed **compresses before it externalizes**: shrinking a value in place is cheaper than a
chain read, so out-of-line storage is the fallback when compression alone does not get the
record under threshold.

---

## 3. The write-time disposition decision (a §8 cross-core contract)

The disposition decision is **fully deterministic and byte-identical across cores** — it
determines the bytes on disk (the goldens) *and* the overflow-page count (the cost, §8). It is a
classic §8 divergence hotspot; spec it exactly, do not let it be a per-core heuristic.

**Recommended algorithm (TOAST-faithful, row-target driven — matches PG, CLAUDE.md §1):**

1. Encode every value inline-plain; compute the record size `R = 2 + key_len + Σ value_size`.
2. If `R ≤ T_target`, done — store all inline-plain. (`T_target` is a spill *target* strictly
   below `RECORD_MAX`, leaving headroom; PG uses `page/4`. A concrete default and its derivation
   from `C` are fixed in format.md when this lands.)
3. Otherwise, repeatedly take the **largest** still-inline-plain variable-length value (ties
   broken by **column declaration order** — the deterministic tiebreak) and:
   a. if it is **compressible** (§6) and compression gets it smaller, mark it **inline-compressed**;
   b. recompute `R`; if `R ≤ T_target`, stop.
4. If `R` is still over target, repeat over the largest remaining value(s), this time
   **externalizing** them (external-plain, or external-compressed if step 3 already compressed
   it), until `R ≤ T_target`.
5. A single value whose *compressed* form still exceeds the per-page inline budget is
   externalized regardless of target — that is the case the old `RECORD_MAX`/`u16` ceilings
   rejected.

The "largest-first, ties by declaration order" selection order is the load-bearing determinism
rule: every core must externalize/compress the **same** values in the **same** order, or the
files diverge.

> **Considered alternative — per-value thresholds (simpler, deferred):** compress any value over
> `S_compress`, externalize any value still over `S_external`, decided per value without
> reference to the whole-record size. Simpler and just as deterministic, but it can externalize a
> value even when the row would have fit inline (worse space/locality). The row-target model
> above is the PG-tracking default (§1); this knob is the documented fallback if the target model
> proves fiddly in practice. **Decision is open until implementation.**

---

## 4. Overflow pages (the out-of-line chain)

An externalized value's bytes live in a **chain of overflow pages**, a new page kind alongside
the leaf/interior nodes:

- **New `page_type = 4` (overflow).** It reuses the existing 12-byte page header
  ([format.md](../fileformat/format.md) *Page header*): `next_page` chains to the continuation
  page (`0` terminates); a length field records how many payload bytes this page carries (a tail
  page is partially filled). The remaining `C − 12` bytes hold a slab of the value.
- **The chain is filled deterministically:** the value's bytes (compressed or raw) are split
  into `C − 12`-byte slabs in order, one per page, the last partial. No per-core freedom in how
  bytes are partitioned — the layout is a §8 byte contract like everything else on disk.
- **The record holds a fixed-size external pointer** instead of the value body: the first
  overflow page index (`u32`), the stored (on-chain) length, and — when compressed — the original
  (decompressed) length. Because the pointer is small and fixed-width, externalizing a value
  drops the record far under `RECORD_MAX`, restoring the B-tree invariant of §1 **without
  changing the split/merge proof** — the proof only needs records ≤ `RECORD_MAX`, and a record
  full of pointers trivially is.
- **The `u16` ceiling is lifted** by carrying the length in the external pointer as a wider
  field (`u32`/`u64`), and for inline forms by the storage-form prefix of §5 — the bare-`u16`
  text/bytea body is superseded, not patched.

**Commit, reclamation, and the watermark — overflow pages are ordinary pages (§9 / P6.2).**
They are allocated from the **free-list** and written copy-on-write at commit exactly like B-tree
nodes ([storage.md §4](storage.md), format.md *Allocation*). An `UPDATE` or `DELETE` that drops
a value frees its whole chain; the pages return to the free-list under the **oldest-live-txid
watermark** (transactions.md §8) like any freed page — no special path. This composes with P6.2
reclamation for free; the only new bookkeeping is *collecting a record's overflow chain* into the
reachable-set walk on open, so an externalized value's pages are not mistaken for free.

---

## 5. Record / value-codec changes + the `format_version` bump

The value codec ([format.md](../fileformat/format.md) *Value codec*) gains a **storage-form
discriminator** ahead of a variable-length body, superseding the bare-`u16`-length form:

- The present-value tag (`0x00`) is followed by a small **form byte** for variable-length types:
  `0` inline-plain, `1` inline-compressed, `2` external-plain, `3` external-compressed.
  (Fixed-width types keep their current bodies unchanged — no form byte; they never spill.)
- **inline-plain** = the form byte + today's body (`u16` len + bytes), so it is the
  byte-for-byte current encoding behind a one-byte prefix.
- **inline-compressed** = form byte + original length + compressed length + the compressed blob.
- **external-\*** = form byte + the §4 external pointer.

This is a value-codec change + a new `page_type`, so it is a **`format_version` 3** bump (clean
break, like v1→v2; the 15 goldens regenerate byte-exact `rust == go == ts == ruby`). **Reserve
all four form codes in the version-3 bump even though compression lands later** (§9): the
overflow slice writes only forms `0` and `2`, but laying out the full discriminator once means
the compression slice is **additive within v3** (it starts emitting forms `1`/`3`) rather than a
second version bump. That is the concrete payoff of "design both together, implement in two
slices."

---

## 6. The compressor: a hand-rolled, deterministic LZ4-block codec

### 6.1 Intellectual property — clear

- **LZ4** (Yann Collet): reference implementation **BSD 2-Clause**; the block and frame **format
  specifications are openly published and free to reimplement**. We reimplement the block format
  **from scratch** (§6.3), so there is **no license obligation at all** — and the format itself
  carries **no known patent encumbrance** (it ships in the **Linux kernel**, ZFS, and many
  databases — patent-cautious environments). Contrast its sibling **zstd**, which shipped with an
  explicit patent grant precisely because that ecosystem worried; LZ4 has no such overhang.
- This is an engineering read, not legal advice; confirm with counsel if it becomes
  ship-blocking. No problem is expected.

### 6.2 Why hand-rolled, not a library (the §14 analysis)

jed's contract is **byte-exact across cores** (`rust == go == ts == ruby`, pinned by goldens —
CLAUDE.md §8) and **identical deterministic cost** (§13). LZ4 **decompression is fully specified**
(any conformant decoder yields identical output), but **LZ4 compression is not** — the encoder is
free to choose match-finding strategy, hash-table size, acceleration, and tie-breaks, so
lz4_flex (Rust), pierrec/lz4 (Go), and lz4js (TS) emit **different compressed bytes, and often
different sizes,** for the same input. Run that through §14:

- **Clause 1** ("all cores match byte-identically") — **fails** on the encode path; the libraries
  diverge.
- **Clause 2** ("significantly faster *and* identical output") — fails the identical-output rider
  (and different *size* also breaks the cost identity of §8).
- **Clause 3** (crypto) — N/A.

So a compression **library is not even admissible** here. The encoder must be a **single,
spec-pinned algorithm hand-written in each core** — exactly like the key encoder
([encoding.md](encoding.md)) and the decimal arithmetic ([decimal.md](decimal.md)), which is the
project's normal mode for "mechanical, must-be-byte-identical" surfaces (CLAUDE.md §2/§5).

**Net: this feature requires no third-party dependency at all.** A library could, in principle,
serve the *decode* path (decompression is standardized, so clause 1 holds), but the decoder is
~50 lines and we need a coherent codec, so hand-roll both. If anyone later proposes a library
regardless, §14 requires explicit human sign-off — record that we considered and declined one.

### 6.3 What the spec pins

LZ4's **block format** is the right choice for a deterministic hand-roll *specifically because it
has no entropy-coding stage* (no Huffman/FSE to reproduce bit-exactly — the thing that makes zstd
painful to pin). A block is a sequence of `(literal-run, match)` tokens; the **decompressor is
defined entirely by the format**. What we pin is the **encoder**, so that one input maps to one
output in every core:

- **Stay byte-faithful to the real LZ4 block format**, including its **little-endian** 2-byte
  match offsets. This is the *one* deliberate exception to jed's big-endian house rule
  ([encoding.md](encoding.md)), taken on purpose: a faithful blob can be read by **any conformant
  LZ4 decoder**, which makes our hand-rolled decoder verifiable against reference LZ4 and the
  on-disk blobs debuggable with off-the-shelf tools. The compressed blob is otherwise an opaque
  byte string to the rest of the format.
- **Fix the encoder's free parameters** as named spec constants: the hash function, the
  hash-table size, a single acceleration/step, the minimum match length (4, per LZ4), greedy
  match selection with a defined tie-break, and the block-format end constraints (the trailing-
  literals rule and the "last match must be ≥ 12 bytes from the end" rule the decoder relies on).
- **"Store the smaller form" rule:** after compressing, if the compressed body is not smaller than
  the raw body by at least the form's overhead, store the value **uncompressed** (inline-plain or
  external-plain). Deterministic given a deterministic compressor; the per-value form byte (§5)
  records the outcome, so a reader never guesses.
- **Minimum-compression-size threshold `S_compress`:** values below it are never compressed
  (header overhead dominates). Pinned as a spec constant.
- **Ship `(input → exact compressed bytes)` byte-fixture vectors** as shared fixtures (the §8
  model, like `encoding/*.toml`), so any core's encoder is verified against the canonical output,
  not just "round-trips."

---

## 7. Read path (transparent + lazy)

The disposition is invisible above the storage seam: when the executor **materializes** a value
(projection, comparison, function argument), the codec resolves it — read the chain for an
external value, decompress for a compressed one — and hands the SQL layer the plain value. No
planner/evaluator change beyond the codec seam.

**Lazy materialization (recommended, PG-faithful):** an external value's chain is read **only when
the value is actually needed**. A `SELECT other_col WHERE pk = $1` that never touches the large
column reads **no** overflow pages. Because the executor is identical across cores, *when* a value
is materialized is itself deterministic — so this optimization does not threaten cross-core cost
identity (§8). The alternative (eagerly materialize every value on record read) is simpler but
pays the chain read even when unused; lazy is the recommended default and what the cost rules of
§8 assume.

---

## 8. Cost model (§13 — bound untrusted queries)

Three accrual rules, all **logical** and deterministic (so a future buffer pool stays invisible —
cost.md §3, pager.md §5):

1. **Overflow-chain reads → `page_read`.** Materializing an external value charges one
   `page_read` per overflow page in its chain (the §4 logical count), accrued **when the value is
   materialized** (§7) — so an unread external value costs nothing, deterministically. This slots
   into the existing `page_read` unit (P6.3) with no new unit. (Slice A materializes **eagerly**,
   so the as-built rule folds the chain pages into the scan's up-front `page_read` block — §12,
   cost.md §3; the unread-value-costs-nothing refinement arrives with §7's lazy read.)
2. **Decompression → a new `value_decompress` unit.** Decompressing a value is real CPU work an
   untrusted query can drive (§13); it must be metered or the cost ceiling cannot bound it. Charge
   per unit of work on materialization. **Granularity is open** (per decompressed byte vs. per
   input page) — pick the one that is cheap to compute identically in every core and fix it in
   `schedule.toml`. Write-side **compression** cost is the single writer's own and lower-stakes,
   but is metered the same way for symmetry and determinism.
3. **Compressed size feeds cost.** The number of overflow pages a value occupies depends on its
   **compressed** size, so the compressor's output size flows straight into `page_read`. This is a
   **second, independent reason** (besides the byte-exact goldens of §6) the compressor must be
   deterministic and identical cross-core — a size difference is a cost difference, which is a §8
   divergence.

---

## 9. Build order (overflow first, compression second, format designed once)

The two halves are sequenced, and the sequencing is the substantive recommendation:

1. **Slice A — overflow / out-of-line storage (first).** The structural change: it touches the
   B-tree size invariant (§1), the value codec + `page_type 4` (§4/§5), the free-list/watermark
   integration (§4), the `page_read` accrual (§8.1), and the `format_version` 3 bump (§5) — and it
   reserves *all four* form codes including the compressed ones. Writes only inline-plain +
   external-plain. **No dependency, no compressor.** This alone lifts `RECORD_MAX` and the `u16`
   ceiling and unblocks the downstream items of §1.
2. **Slice B — transparent compression (second).** Layered *above* the spill seam: the
   deterministic LZ4-block codec + its fixtures (§6), the inline-compressed/external-compressed
   forms (already reserved in v3, so **additive — no second version bump**), the compress decision
   + store-smaller rule (§3/§6.3), and the `value_decompress` unit (§8.2).

Implementing A before B matches the dependency (compression has nowhere to put an
over-target value until the out-of-line path exists) and the difficulty (A is the structural,
invariant-touching work; B is a self-contained transform). Designing **both** in this one doc is
what lets the v3 format reserve the compression form codes up front, so B costs no further format
churn.

---

## 10. Determinism & cross-core contract (summary)

Everything an external/compressed value touches is a §8 byte/cost contract:

- **Disposition decision** (§3) — which values inline/compress/externalize, in largest-first /
  declaration-order sequence — byte-identical across cores.
- **Compressor output** (§6) — byte-identical, pinned by input→output fixture vectors.
- **Chain layout** (§4) — value bytes partitioned into `C − 12`-byte slabs in order,
  deterministically.
- **Cost** (§8) — `page_read` per chain page + `value_decompress`, logical and identical across
  cores; the watermark/free-list interaction is the existing P6.2 contract.
- **Goldens** — `format_version` 3 fixtures regenerate byte-exact `rust == go == ts == ruby`,
  plus new fixtures exercising each disposition (an inline-compressed value, an external chain
  spanning multiple pages, an external-compressed value).

---

## 11. Open questions / non-foreclosure

- **Spill target vs. per-value threshold** (§3) — row-target (PG-faithful) is the recommended
  default; the per-value-threshold simplification is the documented fallback. **Open.**
- **`value_decompress` granularity** (§8.2) — per-byte vs. per-page. **Open**; pick for cheap
  cross-core identity.
- **Algorithm beyond LZ4** — LZ4-block is the recommendation (simple, IP-clear, no entropy stage).
  zstd is explicitly **out** for the hand-roll (entropy coding makes byte-exact reproduction
  across four cores impractical). Not foreclosed if a future need justifies the cost, under §14.
- **Encryption-at-rest interaction** (storage.md §6, CLAUDE.md §9) — both are page-body
  transforms at/above the block seam. If encryption lands, **compress-then-encrypt** is the
  correct order (encrypted bytes do not compress). Designs must not foreclose each other; neither
  is built now.
- **Slotted pages / in-place value updates** (format.md *Within-page structure*) — orthogonal;
  this subsystem rewrites whole node pages like the rest of P6.1.

---

## 12. Slice A — resolved implementation decisions (overflow, no compression)

These pin the open questions of §3/§7/§11 for the **first** slice (out-of-line storage only;
compression is Slice B). They were chosen with the maintainer; the byte details land in
[../fileformat/format.md](../fileformat/format.md) when goldens regenerate.

- **Spill trigger = `RECORD_MAX`** (§3, "only when forced"). The target `T_target` is exactly
  `RECORD_MAX = (C−12)/2` — the existing B-tree record cap. A record that fits inline stays fully
  inline; a value spills **only** when the record would otherwise trip the `0A000` oversized-item
  narrowing. This preserves the B-tree split/merge proof unchanged (every stored record is still
  ≤ `RECORD_MAX`) and minimizes externalization for the common case. The PG-style lower target is
  a later tunable. A record whose key + fixed-width values + one external pointer per spillable
  value *still* exceeds `RECORD_MAX` (pathological: huge key / very many columns at a tiny page)
  remains `0A000` — externalization cannot reduce it further.

- **Disposition encoding = extend the presence tag** (refines §5; no separate form byte). The
  value codec's present/NULL tag gains an external state: **`0x00`** present-inline-plain (today's
  body, byte-unchanged), **`0x01`** NULL, **`0x02`** present-external-plain. **`0x03`** (inline-
  compressed) and **`0x04`** (external-compressed) are **reserved** for Slice B — a `0x03`/`0x04`
  (or any tag ≥ `0x05`) is `data_corrupted` until then. Because inline and NULL are unchanged,
  **every existing value is byte-identical**; only spilled values use the new tag.

- **External pointer = `tag(0x02) ++ u32 first_page ++ u32 payload_len`** (9 bytes). `payload_len`
  is the length of the value's **content payload `P(v)`** held in the chain: the raw UTF-8 bytes
  for `text`, the raw bytes for `bytea`, the decimal body (`flags ++ scale ++ ndigits ++ groups`)
  for `decimal`. The `u32` length supersedes the inline `u16` (lifting the 64 KiB ceiling); the
  inline `u16` is never the binding limit because `RECORD_MAX ≤ 32762 < 65535`, so a value spills
  long before it would overflow `u16`. Only variable-length types (`text`/`bytea`/`decimal`) ever
  spill; fixed-width types are always inline.

- **Overflow page = `page_type 4`.** `P(v)` is split into `C`-byte slabs (`C = page_size − 12`),
  one per page; each page's header carries `item_count` = bytes in this page and `next_page` =
  the continuation (`0` terminates). The reader follows `next_page` from `first_page`, gathering
  `payload_len` bytes, then reconstructs the value by column type. Allocation order is
  deterministic (post-order tree walk; within a record, column order; the chain's slabs in order),
  so the bytes stay cross-core identical and golden-pinnable. **`to_image` now carries a per-page
  `next_page`** (it previously hard-coded `0`, valid only for leaf/interior nodes).

- **Read = eager materialization** (resolves §7 for Slice A). An external value's chain is read
  when its **leaf is decoded** (whole-image load or buffer-pool fault), reconstructing the full
  in-memory `Value`; the in-memory model never holds a lazy reference, so the planner/executor are
  untouched. Lazy (read-on-touch) materialization is a tracked follow-on.

- **Reclamation = reconstruct-on-open, extended to read spillable leaves.** The **default `open`
  is the lazy demand-paged path**, whose free-list reconstruction does *not* read leaf bodies — so
  it cannot see the overflow pointers buried in records. Slice A extends the reachability walk to
  **read the leaves of tables with spillable columns** and collect their live chains, so overflow
  pages are marked reachable and never handed out as free. Dead chains (from an updated/deleted row
  or a rewritten leaf) **leak until the next open**, exactly matching the P6.2 B-tree-orphan model.
  On-disk free-list persistence (so open needn't read leaves — the larger-than-RAM end-state) and
  continuous within-session reclamation remain the documented P6.2 follow-ons.

- **Cost = `page_read` per chain page, folded into the scan's up-front block** (§8.1, as built).
  A scan's `page_read` block counts the B-tree nodes its bound intersects **plus one per overflow
  chain page of every record the bound admits** — so a full scan pays every chain, a point lookup
  pays only the admitted record's chain, and a miss or empty bound pays none. Like the rest of the
  block it is charged up front and does **not** short-circuit under `LIMIT` (cost.md §3). This is
  the eager-materialization reading of §8.1's "charged when materialized": with §7's lazy read a
  tracked follow-on, the per-*touched-value* refinement is revisited there. Deterministic and
  cross-core identical (the chain page count is `ceil(payload/C)` under the §3 disposition rule).

- **Format version.** Clean break to **`format_version 3`** (v2 not read), regenerating the 15
  goldens (only the version field + CRC change for non-spilling fixtures) plus new external-value
  goldens. The version bump, the `format.md` byte-pinning, the Ruby reference, and all three cores
  move **together** — that lockstep step is what makes `rake verify` green again; during
  development the mechanism is built under the v2 version field, since a core cannot bump the
  version alone without regenerating the shared goldens.
