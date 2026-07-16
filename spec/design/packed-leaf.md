# Packed (block-backed) leaves — decode-in-place resident representation over PAX

> **Status note (v24, [bplus-reshape.md](bplus-reshape.md)):** the B+tree reshape supersedes this
> doc's v23-era framing in three places. (a) **Interior nodes no longer hold records** — the
> "interior nodes are always Decoded (with separator rows)" case below *ceased to exist*: an
> interior node is a record-free `separator keys + children` skeleton, and every record-bearing
> node is a leaf. (b) The **leaf region encoding gained the v24 region header** (a flags byte;
> fixed-width columns a null bitmap + dense untagged slots, variable columns end-offset
> directories with zero-span NULLs — [../fileformat/format.md](../fileformat/format.md) *Leaf
> node*), which is exactly the dense stride §11's vectorized tracks want. (c) The remaining
> Decoded-residency cases and the two-form `rowAtMaybeMasked`/`rowAtMasked` read seam **have
> retired** (reshape B3/B4): in-memory databases page from a `MemoryBlockStore`, a commit demotes
> its clean leaves back to pool-faulted `OnDisk` references, the masked-reconstruction lanes are
> deleted (reconstruction is uniformly lazy; a touched-set miss resolves on demand — the
> demand-fault backstop), and `Decoded` survives only as the writer's transient
> materialize–mutate–repack buffer plus the deliberately-resident temp-table stores
> ([bplus-reshape.md](bplus-reshape.md) status note). (d) Since the 2026-07 point-lookup pass, a
> clean Packed leaf also retains **keys and record weights only as page-backed directory facts**:
> navigation borrows key spans, weights are derived lazily, and owned keys/weights appear only at
> the Packed→Decoded mutation boundary. (e) Since the 2026-07 cold-fault pass, those PAX
> end-offset directories themselves stay as **validated byte ranges in the retained page**: fault
> parsing still scans every entry for ascending/bounds validation, then access reads the needed
> big-endian `u32` directly instead of retaining per-record integer arrays. Body text below is kept as
> the design rationale; where it says "interior nodes are always Decoded" or "B-tree stores
> records in interior nodes too", read it as v23 history.
>
> The reasoning behind giving a **demand-paged, file-backed B+tree leaf** a *packed* resident form:
> stop faulting each clean leaf into a fully-decoded `Vec<Row>` / `[]storedRow` detached from the page,
> and instead keep the leaf as its **raw page block + the PAX directories the fault already parses**,
> reconstructing each row (or, better, each *touched column*) **on demand** at scan/emit time — the
> moment the query pulls it. This is jed's equivalent of PostgreSQL's raw `shared_buffers` page +
> `slot_getsomeattrs` and SQLite's raw page-cache page + `OP_Column`. It is the completion of
> [lazy-record.md](lazy-record.md): lazy-record made *variable-length* values compact block-slices but
> left *fixed-width* values eagerly inflated in the resident node; this doc removes the resident
> `Vec<Row>` entirely, so a resident leaf is `≈ page_size` for **all** data. It is also the missing
> consumer half of **PAX** ([../fileformat/format.md](../fileformat/format.md) *Leaf node*,
> `format_version` 23): PAX made leaf bytes **column-major** and the fault parse them into per-column
> directories — then throws those directories away and materializes full rows. This doc keeps them.
> This is a *design* doc; the touched-set cost contract is [cost.md](cost.md) §3, the lazy-value path it
> builds on is [lazy-record.md](lazy-record.md), the residency model it extends is [pager.md](pager.md),
> the byte format it does **not** change is [../fileformat/format.md](../fileformat/format.md), and the
> snapshot lifetime it composes with is [transactions.md](transactions.md) §5/§8. When a decision here
> changes, update [CLAUDE.md](../../CLAUDE.md) §9, [lazy-record.md](lazy-record.md) §12, and
> [pager.md](pager.md) §3 in the same edit.

**Status: re-designed against PAX (`format_version` 23, column-major leaves); NOT built on master.**
An earlier prototype (`origin/feat/packed-leaf`, Rust S1–S2 + Go/TS ports) was written against the
**row-major** leaf layout that predated PAX; it used a per-*record* offset index (`rec_off`) and a
row-major whole-record walk (`decode_record_lazy`). PAX made leaves **column-major** — records are no
longer contiguous — so the per-record index and the row-major walk are **obsolete** and that code is
**superseded** (§13). The reshape remains **cost-, byte-, and result-neutral** (§8) — a
resident-representation / decode-timing change above the block seam, over the *already-bumped* v23
format — so there is **no `format_version` bump**, the conformance corpus is transparent by
construction, and each core lands green independently (the pager P6.4 / lazy-record L1–L3 precedent).
Built **Rust-first, then Go, then TS** (§11). Resident representation is explicitly **not** a §8 byte
contract (lazy-record §5), so each core implements it idiomatically.

**The one-line change:** the fault (`decode_leaf_node` / `decodeLeafNode`) already calls
`parse_pax_leaf` / `parsePaxLeaf` to get the column directories, then runs a full decode loop and
**discards the directories**. Packed = *keep the directories + the block as the resident form, skip the
decode loop, and reconstruct on demand.*

---

## 1. The gap this closes (the fixed-width hole, retold column-major)

[lazy-record.md §1](lazy-record.md) tabulated four lazy-decode levels and closed all four *for
variable-length values*. But its §6 deliberately left **fixed-width scalars eagerly decoded even on the
lazy path** ("deferring them buys nothing"), and — the finding this doc rests on — a faulted leaf still
stores a fully-decoded row vector ([`Node.vals`](../../impl/rust/src/pmap.rs), `[]storedRow`, `Row[]`).
PAX changed the *bytes* (column-major); it did **not** change the *residency*. On master today
`decodeLeafNode` ([format.go](../../impl/go/format.go)) does exactly this:

```
leaf, _ := parsePaxLeaf(pg.payload, n, K)     // parse key dir + column dir + K value dirs
for i in 0..n:                                 // FULL DECODE — every record …
    for c, ty in colTypes:                     //   … every column
        row[c] = readValueLazy(ty, leaf.value(c, i))   // leaf.value(c,i) = O(1) span via colOff
    vals = append(vals, row)                    // materialize a storedRow, detached from the block
return &pnode{keys, vals, weights}              // leaf (the directories) is DISCARDED
```

So the resident cost of a leaf is the **inflated decoded form**, not the page bytes:

- A decoded `Value` is a **32-byte struct in Go** (post the vectorized Stage-0 shrink, `04080cab`;
  it was 104 bytes when the original packed-leaf prototype measured it) and a 24–32-byte tagged enum in
  Rust — still larger than most values' on-disk bytes, and a `storedRow` adds a 24-byte slice header +
  `N × 32 B` on top.
- A **narrow all-fixed-width** leaf is the worst case: a record that is ~16 B on disk becomes ~90 B+
  resident in Go (`storedRow` header + inline 32-byte `Value`s), so an 8 KB page still balloons to
  several× resident (the Stage-0 shrink narrowed the pre-shrink ~16× to roughly ~6×, but did **not**
  close it). lazy-record's block retention drops the page block entirely when nothing defers (all
  fixed-width), so the honest `≈ page_size` bound is *not* reached.
- The buffer pool ([pager.md §3](pager.md)) bounds **page count**, but resident *bytes* run well above
  `resident_leaves × page_size` — the two diverge hardest exactly for fixed-width leaves.

The framing: **lazy-record generalized the `Unfetched` deferral to variable-length values; this doc
removes the resident row vector itself.** The faulted leaf becomes the page block + the PAX directories,
and a row is reconstructed *on demand at emit* by the **same** `readValueLazy` the fault runs today —
moved from fault-time (once, for every column of every row, stored) to emit-time (per pull, per touched
column, transient). Fixed-width then costs its on-disk bytes resident, not a `storedRow` of 32-byte
`Value`s.

---

## 2. What PostgreSQL and SQLite do (the reference behavior)

Both keep the page cache as **raw page images** and decode **transiently, in place**, never storing a
decoded row in the cache — the shape this doc adopts.

- **PostgreSQL** — `shared_buffers` holds raw 8 KB page images; `slot_getsomeattrs(n)` /
  `heap_deform_tuple` deform a tuple **in place in the buffer** into a transient `TupleTableSlot`
  (`tts_values[]` Datum + `tts_isnull[]`), only up to the highest referenced attribute. A `Datum` is a
  `uintptr_t` — fixed-width by-value packed into the word, by-reference a **pointer into the page**
  (`fetchatt`), never a copy. The slot is overwritten row by row.
- **SQLite** — the pager cache holds raw page images; `OP_Column` extracts **one column on demand** out
  of the resident page into a transient `Mem` register (`zData = pC->aRow + aOffset[p2]`), caching the
  parsed column offsets on the cursor (`pC->aOffset`) and stopping at the max referenced column. A text
  `Mem` is often `MEM_Ephem` — a zero-copy pointer into the page.

**Net:** raw page in the cache; decode transient, in place, per touched column; fixed-width in a machine
word; variable-length a pointer into the page; offsets memoized. jed already matches the *raw page in
the cache* half (the block is read into the pool). This doc matches the *decode-in-place* half — and, on
PAX, the *per-column offset memo* half comes **for free from disk** (§6): PAX's value directories *are*
`aOffset`, materialized by the format rather than derived on the cursor.

---

## 3. The model: a Decoded/Packed leaf duality

A leaf `Node` is in one of two forms; **all interior nodes are always Decoded** (separators are small,
row-major on disk — v23 regroups leaves only — and read constantly by navigation):

- **Decoded** — `vals: Vec<Row>` / `[]storedRow`, as today. The form for **in-memory / `from_image` /
  mutated / dirty** leaves. A pure in-memory database (no pager) stays fully Decoded — it has nothing to
  page from and no resident pressure to relieve (lazy-record §4's carve-out, verbatim).
- **Packed** — the leaf's whole page image + **validated, page-backed PAX directory ranges**
  (`key_end` plus the variable column end-offset directories) and thin per-column region
  descriptors. Holds **no per-record key objects, decoded rows, eager weights, or decoded directory
  arrays**. Produced **only** by `decode_leaf_node` on a demand-paged fault.

Navigation uses a form-neutral `key_at(i)` accessor: Decoded returns its owned key and Packed returns a
borrowed span of the page's key blob. Binary search therefore reads the retained bytes directly and
allocates no key wrapper or copy. Per-record **weights** (split math) are likewise derived only when
mutation/rebalance/serialization needs them, without decoding a value:
`weight(i) = key_len(i) + Σ_c value_region_len(c, i)`. A Packed leaf carries neither a weight vector nor
per-record key objects while clean.

---

## 4. The `row_at` accessor seam

All value reads go through a single accessor that hides the form. Two shapes, the second the PAX
dividend:

- `Node::row_at(i) -> Row` (and a borrow helper that materializes-then-lends `&Row` for the
  streaming-visit callbacks) — reconstructs the **whole** record `i`.
- `Node::col_at(i, c) -> Value` / a touched-mask variant `row_at_masked(i, mask)` — reconstructs **only
  the touched columns**. This is the shape row-major could not offer cheaply and PAX makes O(1).

Behavior by form:

- **Decoded** → `vals[i].clone()` (whole row) or `vals[i][c].clone()` (one column) — exactly today's
  `.vals[i].clone()`.
- **Packed** → reconstruct on demand from the retained directories: `readValueLazy(colTypes[c],
  paxLeaf.value(c, i))` for each touched column `c`, where `paxLeaf.value(c, i)` is the **O(1)** byte
  span `colVals[c][colOff[c][i] : colOff[c][i+1]]`. Fixed-width columns decode into the `Value`;
  variable-length columns become `Unfetched::Inline` **block-slices** — *identical* to what the fault
  builds today, just built now, and only for the columns the query asks for.

Emit then proceeds exactly as before: `resolve_columns(row, mask)` resolves the touched columns
([lazy-record.md §4](lazy-record.md)); the executor consumes the owned `Row`. Landing the accessor
first (S1), while the representation is still all-Decoded, is a **no-behavior-change** seam — the
lazy-record L1 / pager P6.4a "seam first" move. It is also *absent from master* today (value reads still
index `.vals[i]` directly — 6 sites in `format.rs`, 16 in `pmap.rs`, and the Go/TS equivalents), so S1
is still the correct, layout-independent first slice.

---

## 5. The Packed representation (per-core idiomatic, not a byte contract)

Like lazy-record's (a)/(b) choice ([lazy-record.md §5](lazy-record.md)), the Packed form is
**invisible** — results and cost are identical either way (§8) — so it is **not** a §8 byte contract and
each core chooses idiomatically. The representation is **validated views/offsets for the PAX
directories**, retained instead of decoded into owned integer arrays and then discarded:

- **Rust** — `packed: Option<PackedLeaf>` on `Node` (leaves only; `None` for Decoded/interior), where
  `PackedLeaf { block: Arc<Vec<u8>>, dirs: PaxDirs }` — `PaxDirs { key_dir, regions, … }` retains
  payload offsets for the key and variable-value directories plus one descriptor per column
  ([format.rs](../../impl/rust/src/format.rs)). A block-slice is
  `(Arc clone, off, len)`; the `Arc` keeps the page alive past pool eviction (the existing
  `Unfetched::Inline` L3 mechanism, generalized from "held when a value defers" to "the leaf's backing
  store").
- **Go** — the retained `*paxLeaf` (`{keyBlob, keyEnd, regions}`); `keyEnd` and variable `ends` are
  `[]byte` views into the page, while `key(i)` and the column accessors return page subslices. There
  is no resident decoded end-offset array.
- **TS** — numeric payload offsets for the key and variable-value directories over the retained
  `Uint8Array.subarray` block view (single-threaded), plus one descriptor per column.

The decisive difference from the pre-PAX prototype: **no fault-time offset pass.** Row-major needed a
`decode_record_lazy` cursor advance to compute per-record start offsets (`rec_off`) at fault time. PAX
delivers the boundary index **on disk** — `parse_pax_leaf` scans the directories in one pass with **no
value decode at all**, validates every entry's ordering and bounds, and retains only their page ranges
or offsets. The fault therefore does no per-value copy, no per-value decode, no boundary computation,
and no decoded end-offset-array allocation. Keys are block spans located by direct big-endian reads
from the shared `key_end` directory; search compares those spans directly.

---

## 6. Per-column offsets — provided by PAX, not memoized (the prototype's S3, obsoleted)

The pre-PAX prototype carried a deferred **S3**: a write-once, on-leaf per-*column* offset memo (SQLite
`aOffset`) so repeated scans of a cached row-major leaf could skip re-walking each record's columns
left-to-right by their length prefixes. **PAX obsoletes this.** The whole rationale was "avoid
re-deriving column boundaries"; PAX's **value directories (`colOff`) *are* those boundaries**, written
in the page, validated once at fault, and giving `value(c, i)` in O(1) by a direct big-endian directory
read — no left-to-right walk ever, first scan or hundredth. There is nothing left to memoize at the
column-span level.

The one residual the prototype's S3 gestured at — skipping a nested `jsonb` / array / composite
**structural** re-walk *inside* a value on repeated access — is a separate, much narrower concern (it is
about re-parsing a single value's interior, not locating columns), and it stays a §11 follow-on,
addable per-core if a workload ever needs it. The drift risk that made row-major's S3 unattractive (a
second, cache-driven decode path) does not arise for column *location* under PAX, because location is
read straight from the on-disk directory that every core already parses identically.

**The strict upgrade PAX unlocks.** Because `colOff` gives direct per-column offsets, `row_at` can
reconstruct **only the touched columns** (§4's `row_at_masked`) at O(1) per column — the true
`OP_Column` / `slot_getsomeattrs` model. Row-major packed-leaf could not do this cheaply: skipping to
column `c` there required walking columns `0…c−1` to find `c`'s offset. So PAX + packed-leaf together
reach the PG/SQLite decode-in-place ideal that neither reaches alone.

---

## 7. Snapshot lifetime, copy-on-write, mutation (composition only)

No new lifetime model — it composes with the three already in place ([lazy-record.md §7](lazy-record.md)):

- **Copy-on-write immutability.** A clean leaf's page is immutable on disk; a Packed leaf's directories
  and block-slices read bytes that never change under them. Reconstruction works on the scan's own
  cloned row, never the shared tree — so repeated scans re-reconstruct (and re-charge) consistently.
- **The buffer-pool pin.** Under Packed the leaf's `Arc<Vec<u8>>` (Go/TS GC view) **is** the pin — a
  reconstructed row's block-slice values outlive pool eviction, identical to how an in-flight
  `Unfetched::Inline` value already survives it.
- **The streaming cursor's snapshot.** A row of block-slice values a streaming `Rows` yields is
  `'static` for the same reason its `Unfetched::Inline` values already are.

**Mutation.** A copy-on-write insert/delete descends to a leaf. On reaching a **Packed** leaf it first
**materializes keys, rows, and weights together to Decoded** (`decoded_parts()`), then the existing
`build` / `node_insert` / `node_remove` / `merge_rebalance` logic runs with the same split/merge rules.
A mutated leaf is always Decoded (and dirty, page `0`), so serialization (`serialize_dirty`, which
only touches dirty nodes, re-emits PAX column-major from the Decoded rows) stays unchanged. Rust may
subsequently edit that Decoded leaf in place for INSERT only when `Arc::get_mut` proves unique
ownership; Go/TypeScript require the leaf's private mutation-generation stamp to match the map's
still-active token. All three additionally require `page == 0`; a Packed/clean or aliased leaf always
takes the ordinary rebuild path ([transactions.md §3](transactions.md)). Delete/rebalance stays
copy-on-write. The write side stays metered by `value_compress` per stored row version
([cost.md §3](cost.md)).

---

## 8. Determinism & cost — invariant (why each core lands green)

Identical in spirit to the buffer pool and lazy-record: the reshape changes **when** a value is decoded
and **where** the leaf's bytes live, never **what** a query observes or **what** it costs.

- **No format change.** On-disk bytes, key encoding, goldens, the cross-core round-trip — untouched.
  PAX already owns v23; packed-leaf is a residency change *over* v23, so **no `format_version` bump.**
- **Cost is invariant.** Cost is the **static touched set** — `page_read` per node, `value_decompress`
  per compressed slab — computed at plan time ([cost.md §3](cost.md)). jed meters **no per-column-decode
  unit**, so moving a decode from fault-time to emit-time (and touched-column-only) moves no charge.
  Every `# cost:` corpus value holds; the per-core cost suites are unchanged.
- **Results are invariant.** A reconstructed value equals the eagerly-decoded value byte-for-byte (same
  `readValueLazy` over the same `value(c,i)` span, §4). Row order is still defined only by `ORDER BY`
  (CLAUDE.md §8).
- **Errors move only in timing.** A malformed inline body surfaces `XX001` **when touched**, exactly as
  lazy-record already moved it to touch-time; an *untouched* corrupt body is not read (the established
  lazy-decode consequence). A malformed *directory* still surfaces `data_corrupted` at fault, exactly as
  master's `parsePaxLeaf` already does (it is parsed eagerly either way).
- **The poison guard holds.** An `Unfetched` that escapes resolution panics/throws — never read as NULL.

Because cost, bytes, and results are invariant, the slices are **corpus-transparent** and each core
lands independently — no new capability flag.

---

## 9. Memory — the honest buffer-pool bound, now for all data

The dividend lazy-record §9 could not reach for fixed-width. Under Packed a resident leaf is **≈ its
page block** (one `page_size` buffer + thin per-column descriptors; the end-offset directory bytes are
already inside that buffer and are not copied into per-record `u32` arrays), the literal PG/SQLite
model. Resident memory becomes
`≈ pinned_pages × page_size` for **fixed-width and variable-length alike**, so the `cache_bytes` budget
finally *means what it says*, and the narrow-fixed-width blow-up is gone. This is a real step toward the
larger-than-RAM end state (CLAUDE.md §9): a faulted leaf holds compact page bytes + column offsets, not
expanded row vectors.

---

## 10. What does NOT change

- **The §8 byte contract** — on-disk format, key encoding, goldens, the round-trip. No `format_version`
  bump (PAX already owns v23).
- **The cost contract** — the static touched set and every `# cost:` value.
- **B-tree navigation / split / merge** — keys are raw bytes through `key_at(i)`; values going Packed
  does not touch them, and per-record weights are derivable from the directories (§3).
- **Interior nodes** — always Decoded, row-major on disk (small separators, read constantly).
- **Pure in-memory databases** — stay Decoded via `from_image` (§3), like the buffer pool and
  lazy-record.
- **The large-value / lazy-record path** — `Unfetched::Inline` block-slices are exactly what `row_at`
  reconstructs; this generalizes the resident store, it does not replace the value path.
- **PAX validation and corruption timing** — `parse_pax_leaf` / `parsePaxLeaf` still scans and validates
  every directory entry at fault. Only the retained representation changes from decoded end-offset
  arrays to page-backed ranges/offsets.
- **Snapshot / watermark / mutation contracts** — composition only (§7).

---

## 11. Slicing (Rust-first; each mergeable, cost/byte/corpus-neutral)

- **S0 — spec (this doc).** + the lazy-record.md §12 / CLAUDE.md §9 / pager.md §3 / TODO.md updates.
  *No code.*
- **S1 — the `row_at` / `col_at` accessor seam (no observable change).** ✅ **landed (Rust).** Introduce
  `Node::row_at(i)` and the touched-column `col_at(i, c)` / `row_at_masked(i, mask)` (+ the `with_row`
  borrow helper and `decoded_rows` for mutation materialization) and route the `.vals[i]` read sites in
  `pmap.rs` through them (the `format.rs` serialize sites keep direct `vals` reads — `serialize_dirty`
  only touches dirty/Decoded nodes; `serialize_node` materializes a Packed root leaf via the seam).
  Representation stays all-Decoded, so `row_at = vals[i].clone()` — byte-identical. *Mergeable, no
  behavior change.*
- **S2 — Packed leaf (the memory win).** ✅ **landed in all three cores.** `decode_leaf_node` retains
  `(block, PaxDirs, col_types, n)` and stores **no** per-record key, row, or weight vector; `key_at`
  borrows the page key span, weights derive from the directories, and `row_at` / `col_at`
  reconstruct via `read_value_lazy(col_types[c], dirs.value(c, i))`; mutation descent materializes
  Packed→Decoded through `decoded_parts` (§7). The touched-column accessors
  are built and unit-tested here even though the executor does not yet *drive* masked reconstruction —
  that is the deferred S3 below. Unit tests: a faulted-leaf reconstruction shares one page block across
  all its deferred inline values (resident `≈ page_size`, §9), and `col_at`/`row_at_masked` reconstruct
  only the touched columns byte-identically to the whole row. Representation-invariant tests also
  pin that a faulted leaf has empty resident key/weight vectors and that key/weight access matches
  the encoder facts.
- **Cold-fault P1 — zero-copy PAX directories.** ✅ **landed in all three cores 2026-07.** Fault
  parsing still scans every key and variable-value end offset and rejects descending/out-of-bounds
  directories immediately, but retains a byte range (Go) or payload offset (Rust/TypeScript) instead
  of an owned `N`-entry integer array. Direct big-endian reads preserve O(1) key/value location while
  removing `1 + V` per-leaf allocations for `V` variable-width columns. White-box tests pin both the
  page-backed representation and fault-time `XX001` behavior for malformed directories.
- **S3 — touched-column-only reconstruction wired through the executor (the PAX dividend).** *Landed
  in all three cores 2026-07 (Track A1, a per-core internal optimization like the vectorized executor —
  results/cost/byte-neutral, no `format_version` bump). Go first (`materializeRel`/`scanRange`/`storeScan`
  masked feeds), then Rust (`row_at_maybe_masked` + a `recon` seam through `collect_range` /
  `walk_range_visit` / `RangeCursor`; the whole-row scans stay for mutation/FK/index-maintenance) and TS
  (`rowAtMaybeMasked` + a `recon` seam through `rangeEntriesCounted` / `scanRange` / the pull iterators),
  each verified by a per-core paged-vs-resident `masked_scan` battery.* The scan feed threads the
  query's touched-column mask (`relMasks`, a `[]bool` already computed at plan time and used by
  `resolveColumns`) through the pmap traversals (`scanRange`/`scanRangeRev`/`rangeCursor`/
  `rangeEntriesCounted`) and the SELECT eager feed (`materializeRel` → `ScanWithUnitsMasked`), so a Packed
  leaf calls `row_at_masked` and skips decoding untouched columns.
  **The "marginal" assessment was disproven by a wide-table scan bench.** On a file-backed, all-fixed-
  width table, a scan touching one column pays a **width-linear** reconstruction tax (~32 B + a decode per
  *untouched* column); at 64 columns the touched-column path runs **~2.3–3.0× faster** (`count(*)` most,
  since it decodes nothing) with **B/op unchanged** — the decode-CPU dividend is large for wide tables,
  negligible for narrow ones. (The *allocation* dividend — B/op, the still-full-width `storedRow` — is
  captured by the columnar gather of Track A2, landed below.) **The silent-wrong-result risk is contained, not traded
  away:** untouched columns are left `Null` (no poison sentinel), which is safe because the mask is a
  **complete superset** of every column any consumer reads — the same invariant `resolveColumns` already
  relies on for deferred variable-length values, now load-bearing for fixed-width too and guarded by the
  paged-vs-resident battery (`impl/go/masked_scan_test.go`, a wide fixed-width table × a spread of query
  shapes). Mutation / FK / index-maintenance reads keep the **whole-row** `ScanWithUnits`/`GetWithUnits`
  (they recompute keys from the old row), so masking is scoped to read-only SELECT feeds. Cost-neutral (no
  per-column cost unit, §8). *This also subsumes the pre-PAX prototype's deferred S3 offset memo, which PAX
  obsoletes (§6).*
- **S4 — port S1+S2 to Go**, then **S5 — port S1+S2 to TS.** Mirror the Rust reshape idiomatically (Go
  retains `*paxLeaf`; TS retains the parsed directories over a `Uint8Array.subarray`); each lands green
  independently. The `col_at`/`row_at_masked` accessors are ported too (S3-ready), just not driven.
- **Track A2 — columnar gather (the allocation dividend).** *Landed in all three cores 2026-07 (per-core
  internal, like A1 — results/cost/byte-neutral, no `format_version` bump). Go first; then the **vectorized
  aggregate executor** itself was ported to Rust (`exec_vectorized_agg` / `agg_columnar`) and TS
  (`execVectorizedAgg` / `aggColumnar`) so the AGGREGATE gather rides it there too — the executor is a
  single-base-table SUM/COUNT/MIN/MAX/AVG (whole-table or single-integer-key GROUP BY) that folds
  int64-bucketed on the row path (in-memory) or columnar on the file-backed path, reusing each core's
  scalar `Acc`/fold/finalize so the fold is byte-identical (the scalar grouped path already folds through
  the same accumulator). Conformance (in-memory) exercises the row path byte-identically; the file-backed
  columnar path is proven by the paged-vs-resident batteries.* A1 removed the
  untouched-column **decode** but still allocated a **full-width `storedRow`** per record (untouched
  columns left `Null`), so the B/op stayed width-linear — a 64-column `count(*)` allocated ~100 MB of
  all-`Null` rows. A2 gathers **only** the touched columns into dense per-column lanes straight off the
  leaves (the new `pMap.columnarScan` → `colAt` per admitted entry, an O(1) PAX span on a Packed leaf;
  interior-node separator entries gathered alongside the leaves, as a B-tree stores records there too),
  **never** building a full-width row. Wired for the **filter-free vectorized aggregate** path only
  (`batch.go aggColumnar` → `foldAggColumnar` / `groupByIntKeyColumnar`, mirroring the row-fed
  `foldAggBatch` / `groupByIntKey` but reading `lane[i]` instead of `survivors[i][idx]`): a wide-table
  single-column aggregate drops from O(rows × columns) to O(rows) allocation (bench: 64-column `count(*)`
  ~100 MB → a few KB and ~19× faster; `sum(col)` ~12× less allocation, ~5× faster; both now **flat**
  across table width). **Cost-neutral by construction** — `ColumnarScanMasked` charges the identical
  `page_read` (same node visits) / `value_decompress` / `storage_row_read` block as the row feed, and
  the fold charges the identical `aggregate_accumulate` — proven by the `masked_scan_test.go` paged-vs-
  resident battery (single-leaf **and** a multi-level tree, whole-table + grouped kernels, rows AND cost).
  **Gated to file-backed stores** (`store.paging != nil`; an in-memory store's row path already shares
  its rows zero-copy, so a lane gather would only add allocation) and **declines to the row path** on any
  spillable touched column (so no value-resolution step is needed and the lanes carry no unfetched
  values). A `WHERE` filter is handled by **Track A3** below (a selection vector over the lanes), not a
  decline.

- **Track A2 — projection feed (the allocation dividend for bare-column projections).** *Landed in all
  three cores 2026-07 (per-core internal, like A1 — results/cost/byte-neutral, no `format_version` bump).
  Go first (`projectColumnar` → the `emitColumnar` drive), then Rust (`Emitter::Columnar` + `project_columnar`,
  driven eagerly and lazily) and TS (a `"columnar"` `EmitMode` + `projectColumnar`, same two drives). Unlike
  the aggregate gather, this needs no vectorized executor — it is a standalone emit mode.* The sibling of
  the aggregate gather: a **bare-column projection** over a single-table
  full/PK-bounded scan with no ORDER BY / LIMIT / OFFSET / blocking operator (`SELECT c0, c3 FROM
  t [WHERE …]`) previously materialized a **full-width `storedRow`** per record just to project a few
  columns — the same width-linear B/op the aggregate feed removed (`project_c0` bench: 64-column `SELECT
  c0 FROM t` allocated ~136 MB). `batch.go projectColumnar` (gated + shaped exactly like `aggColumnar`)
  gathers **only** the touched columns into dense lanes via the same `ColumnarScanMasked`, then returns a
  new **`emitColumnar`** emitter that builds each output row directly from the lanes on emission — never a
  full-width row. Bench: `project_c0` **~136 MB → ~10 MB (≈13× less) and ~7× faster** at 64 columns, B/op
  now **flat across width** (a bare column ref is a zero-cost slot read, so the lane read is cost-identical
  to the row-fed projection eval). **Cost-neutral by construction** — the same `page_read` / `storage_row_read`
  block, then `row_produced` per emitted row charged by the `emitColumnar` drive exactly like the
  `emitProject` drive over a bare-column projection (lazy: an early exit skips the `row_produced` of rows
  it never pulls). Same **file-backed / non-spillable** gate; **declines** (falls through to the identical-cost
  materialize path) for an in-memory store, a spillable column, or any non-column projection — verified by
  the `masked_scan_test.go` battery (projection cases added to the multi-level tree for the
  interior-separator gather). A `WHERE` filter is handled by **Track A3** below.

- **Track A3 — filter vectorization (a selection vector over the lanes).** *Go core landed 2026-07
  (per-core internal, like A1/A2 — results/cost/byte-neutral, no `format_version` bump). Landed in all three
  cores 2026-07: the **projection** path first (`filterColumnar` + a `sel` selection vector threaded through
  the columnar emit drive in each core), then the **aggregate** path once the vectorized aggregate executor
  was ported to Rust + TS (each core's `filter_columnar`/`filterColumnar` feeds the same selection vector to
  its columnar aggregate fold).*
  A2 gathered only **filter-free** aggregates and projections; a `WHERE` predicate forced the full-width
  row path. A3 lifts that: `batch.go filterColumnar` evaluates `plan.filter` over the gathered lanes into a
  **selection vector** (`[]int32` of survivor indices), and the fold (`foldAggColumnar` /
  `groupByIntKeyColumnar`) / emit (`emitColumnar`, which gains an optional `sel` field) visits **only** the
  selected lane positions — so a **filtered** aggregate or projection also gathers columnar, never a
  full-width row. The crux is cost- and result-identity, and it holds **by construction**: `filterColumnar`
  reuses the scalar `rExpr.eval` **verbatim** over a *single reusable scratch row* (the masked columns
  filled from the lanes at that row index, untouched columns left `Null`), so the predicate's
  `operator_eval` charges, its 3VL survivor test (keep iff `TRUE`), and its **result** are byte-identical to
  the scalar `WHERE` loop — because the row path *also* feeds the filter a **masked** row (untouched columns
  `Null` via `resolveColumns` / `rowAtMasked`) and the filter references only masked columns
  (`collectTouched` includes `plan.filter`), so a scratch row filled from the lanes is the same input. The
  one reusable scratch row is the allocation win: no full-width `storedRow` per scanned row, only the
  `int32` survivor indices. Same **file-backed / non-spillable** gate (a filter over a spillable
  text/decimal column keeps the row path — the lanes carry no unfetched values). Bench (64-column table,
  filter over 1 column, ~50% selectivity): `sum(c0) WHERE c0 > 500` **~128 MB → ~9 MB (≈14× less) and ~5.9×
  faster**; `SELECT c0 WHERE c0 > 500` **~132 MB → ~10 MB (≈13.5× less) and ~6× faster**; B/op **flat across
  width**. Verified by the `masked_scan_test.go` battery — the filtered aggregates + projections (single-leaf
  and multi-level tree, partial/empty/full selectivity, AND/OR predicates, filtered GROUP BY) now take the
  columnar path and must agree with the resident row path on rows **and** cost, so a mis-indexed selection
  vector diverges loudly.

The columnar read-path work is now **complete across all three cores** (2026-07): A1 (touched-column scan
wiring), the A2 projection feed + aggregate gather, and A3 filter vectorization for both projections and
aggregates — including the **vectorized aggregate executor** (single-base-table SUM/COUNT/MIN/MAX/AVG,
whole-table or single-integer-key GROUP BY) that the aggregate gather rides — have all landed in Rust, Go,
and TS. Deferred follow-ons (none foreclosed): **Nested-value structural memo** (skip re-parsing a single `jsonb`/array/composite value's
*interior* on repeated access — the narrow residual of §6, not the column-location memo PAX already
provides); **in-memory databases adopting
deferral** only if a Memory pager backing lands ([pager.md §6](pager.md)).

---

## 12. Determinism & cross-core notes (summary)

- **Results + cost are the only contract**, and both are invariant (§8); the Packed representation, the
  reconstruct-at-emit timing, and the touched-column reconstruction are internal machinery — **not** a
  byte contract — each core implements them idiomatically (the pager / spill / lazy-record precedent).
- **No format change, no new cost unit** — decode timing, byte location, and which columns are
  reconstructed are invisible to the on-disk bytes and the static touched-set cost.
- **No nondeterminism leaks** — reconstruction keys on column position + the static mask (both
  deterministic) and reads the on-disk directory (identical across cores), never on iteration order or
  timing; a touched value decodes to the byte-identical result the eager path produced.
- **Memory safety holds** — block-slice traversal is owned-cursor / sliced-buffer in every core (no
  `unsafe`, no cgo; CLAUDE.md §2/§13); the shared block is an `Arc`/GC view, the directories are parsed
  once and immutable under copy-on-write — so concurrent readers race on nothing.

---

## 13. Relationship to the pre-PAX prototype (`origin/feat/packed-leaf`)

The prototype landed the *thesis* (raw-page-resident faulted leaf, reconstruct at emit, no inflated row
vector) but built its *mechanism* against the **row-major** layout that PAX replaced. What carries over
and what is superseded:

| Prototype element | Under PAX (this doc) |
|---|---|
| Goal: no resident `Vec<Row>`, leaf ≈ page_size, reconstruct at emit | **Kept** — still unmet on master (§1) |
| Decoded/Packed duality; interior always Decoded; in-memory stays Decoded | **Kept** (§3) |
| `row_at` accessor seam (S1) | **Kept + extended** with touched-column `col_at` (§4) |
| Snapshot / COW / mutation-materializes-to-Decoded / `Arc` pin (§7) | **Kept** (§7) |
| Cost/byte/result invariance, no format bump (§8) | **Kept** (§8) |
| `rec_off: Vec<u32>` per-*record* offset index | **Obsolete** — records aren't contiguous under PAX; the index is per-*column* (`colOff`), and it comes from disk |
| Fault-time offset-computation pass (`decode_record_lazy` cursor advance) | **Obsolete** — `parse_pax_leaf` already produces the directories with no value decode (§5) |
| Row-major whole-record walk to reconstruct row `i` | **Obsolete** — replaced by per-column O(1) gather via `dirs.value(c, i)` (§4) |
| Deferred S3 on-leaf column-offset memo (SQLite `aOffset`) | **Obsolete** — PAX's value directories *are* `aOffset`, on disk (§6) |
| §1 row-major worked examples / 104-byte `Value` figures | **Retold** column-major over the 32-byte `Value` (§1) |

Consequently the prototype's per-core code (`format.{rs,go,ts}`, `pmap.{rs,go,ts}`) is **re-derived
against `parse_pax_leaf`, not rebased** — a forward-rebase would conflict heavily and the reconstruction
logic is rewritten, not replayed. A fresh branch off the PAX master (`feat/packed-leaf-pax`) carries
this redesign; `origin/feat/packed-leaf` is retained only for reference.
