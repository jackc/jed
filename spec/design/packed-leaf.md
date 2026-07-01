# Packed (block-backed) leaves — decode-in-place resident representation over PAX

> The reasoning behind giving a **demand-paged, file-backed B-tree leaf** a *packed* resident form:
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
- **Packed** — `block: Arc<[u8]>` (the leaf's whole page image) + the **parsed PAX directories** (the
  `paxLeaf` / `PaxDirs` the fault already builds: `keys` spans, `colVals` spans, `colOff` value
  directories). Holds **no `Vec<Row>`**. Produced **only** by `decode_leaf_node` on a demand-paged
  fault.

Navigation is unaffected: B-tree search / split / merge / cost-count compare **keys** (raw bytes,
directly available as `paxLeaf.keys[i]` in both forms) and **never read `vals`** — confirmed across all
`.vals` sites (the value region is read only at emit and mutation). So the value representation can
change without touching the load-bearing navigation code (the same structural fact lazy-record §3 rests
on). Per-record **weights** (split math) are likewise derivable from the directories without decoding a
single value — `weight(i) = 2 + key_len(i) + Σ_c (colOff[c][i+1] − colOff[c][i])` — so a Packed leaf can
carry weights (or compute them lazily) with no value decode.

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
each core chooses idiomatically. The representation is **the parsed PAX directories**, retained instead
of discarded:

- **Rust** — `packed: Option<PackedLeaf>` on `Node` (leaves only; `None` for Decoded/interior), where
  `PackedLeaf { block: Arc<[u8]>, dirs: PaxDirs }` — `PaxDirs { col_start, col_off, … }` is exactly the
  struct `parse_pax_leaf` already returns ([format.rs](../../impl/rust/src/format.rs)). A block-slice is
  `(Arc clone, off, len)`; the `Arc` keeps the page alive past pool eviction (the existing
  `Unfetched::Inline` L3 mechanism, generalized from "held when a value defers" to "the leaf's backing
  store").
- **Go** — the retained `*paxLeaf` (`{keys, colVals, colOff}`); `colVals[c]`/`keys[i]` are `[]byte`
  subslices of the block and therefore GC-alive views of the page.
- **TS** — the retained parsed directories over a `Uint8Array.subarray` view of the block
  (single-threaded).

The decisive difference from the pre-PAX prototype: **no fault-time offset pass.** Row-major needed a
`decode_record_lazy` cursor advance to compute per-record start offsets (`rec_off`) at fault time. PAX
delivers the boundary index **on disk** — `parse_pax_leaf` reads the directories in one pass with **no
value decode at all** — so the fault does no per-value copy, no per-value decode, and no boundary
computation: it parses the directory `u32`s (already required to validate the page) and retains them
with the block. Keys stay owned/decoded initially (small, read constantly); **keys as block-slices** is
a deferred follow-on (§11).

---

## 6. Per-column offsets — provided by PAX, not memoized (the prototype's S3, obsoleted)

The pre-PAX prototype carried a deferred **S3**: a write-once, on-leaf per-*column* offset memo (SQLite
`aOffset`) so repeated scans of a cached row-major leaf could skip re-walking each record's columns
left-to-right by their length prefixes. **PAX obsoletes this.** The whole rationale was "avoid
re-deriving column boundaries"; PAX's **value directories (`colOff`) *are* those boundaries**, written
in the page, parsed once at fault, and giving `value(c, i)` in O(1) by array index — no left-to-right
walk ever, first scan or hundredth. There is nothing left to memoize at the column-span level.

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
- **The buffer-pool pin.** Under Packed the leaf's `Arc<[u8]>` (Go/TS GC view) **is** the pin — a
  reconstructed row's block-slice values outlive pool eviction, identical to how an in-flight
  `Unfetched::Inline` value already survives it.
- **The streaming cursor's snapshot.** A row of block-slice values a streaming `Rows` yields is
  `'static` for the same reason its `Unfetched::Inline` values already are.

**Mutation.** A copy-on-write insert/delete descends to a leaf and rebuilds it. On reaching a **Packed**
leaf it first **materializes it to Decoded** (`to_decoded()` = `row_at` over all records), then the
existing `build` / `node_insert` / `node_remove` / `merge_rebalance` logic runs **unchanged** — a
mutated leaf is always Decoded (and dirty, page `0`), so serialization (`serialize_dirty`, which only
touches dirty nodes, re-emits PAX column-major from the Decoded rows) also stays unchanged. The write
side stays metered by `value_compress` per stored row version ([cost.md §3](cost.md)).

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
page block** (one `page_size` buffer + the thin directory `u32` arrays — themselves a slice of the
parse — shared across every reader of that leaf), the literal PG/SQLite model. Resident memory becomes
`≈ pinned_pages × page_size` for **fixed-width and variable-length alike**, so the `cache_bytes` budget
finally *means what it says*, and the narrow-fixed-width blow-up is gone. This is a real step toward the
larger-than-RAM end state (CLAUDE.md §9): a faulted leaf holds compact page bytes + column offsets, not
expanded row vectors.

---

## 10. What does NOT change

- **The §8 byte contract** — on-disk format, key encoding, goldens, the round-trip. No `format_version`
  bump (PAX already owns v23).
- **The cost contract** — the static touched set and every `# cost:` value.
- **B-tree navigation / split / merge** — keys are raw bytes (directly `paxLeaf.keys[i]`); values going
  Packed does not touch them, and per-record weights are derivable from the directories (§3).
- **Interior nodes** — always Decoded, row-major on disk (small separators, read constantly).
- **Pure in-memory databases** — stay Decoded via `from_image` (§3), like the buffer pool and
  lazy-record.
- **The large-value / lazy-record path** — `Unfetched::Inline` block-slices are exactly what `row_at`
  reconstructs; this generalizes the resident store, it does not replace the value path.
- **The PAX on-disk parse** — `parse_pax_leaf` / `parsePaxLeaf` is unchanged; packed-leaf **retains** its
  result instead of discarding it.
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
- **S2 — Packed leaf (the memory win).** ✅ **landed (Rust).** `decode_leaf_node` retains `(block,
  PaxDirs, Arc<col_types>, n)` and stores **no** row vector; `row_at` / `col_at` / `row_at_masked`
  reconstruct via `read_value_lazy(col_types[c], dirs.value(c, i))`; mutation descent materializes
  Packed→Decoded through `decoded_rows` (§7). The `col_at` / `row_at_masked` touched-column accessors
  are built and unit-tested here even though the executor does not yet *drive* masked reconstruction —
  that is the deferred S3 below. Unit tests: a faulted-leaf reconstruction shares one page block across
  all its deferred inline values (resident `≈ page_size`, §9), and `col_at`/`row_at_masked` reconstruct
  only the touched columns byte-identically to the whole row. Built Rust-first.
- **S3 — touched-column-only reconstruction wired through the executor (the PAX dividend).** *Deferred
  follow-on (assessed 2026-07). The accessor (`col_at`/`row_at_masked`) is built (S2); what remains is
  threading the query's touched-column mask through the pmap + storage scan API so the scan calls
  `row_at_masked` instead of `row_at`.* **Why deferred:** in jed the expensive-decode dividend the
  `OP_Column`/`slot_getsomeattrs` model promises is **already captured by lazy-record + S2** — every
  spillable/structured value (text/bytea/decimal/json/composite/array/range) is *already* reconstructed
  as a cheap `Unfetched::Inline` block-slice, so S3's remaining compute win is only skipping untouched
  *fixed-width* decodes (a few bytes → an `i64`) plus a few `Arc` clones — marginal. Against that, S3
  threads a `&[bool]` mask through the whole scan API of **all three cores** and, if `row_at_masked`
  left untouched columns `Null`, would trade jed's fail-loud poison guard (§8 — an unresolved
  `Unfetched` *panics* if read) for a **silent wrong result** on an incomplete mask (mitigable only with
  a poison sentinel, widening the surface further). The headline win (resident `≈ page_size` for all
  data) is fully delivered by S1+S2 without it. Revisit if a wide-table scan bench shows the fixed-width
  decode is a real hot-path cost. Cost-neutral when built (no per-column cost unit, §8). *This also
  subsumes the pre-PAX prototype's deferred S3 offset memo, which PAX obsoletes (§6).*
- **S4 — port S1+S2 to Go**, then **S5 — port S1+S2 to TS.** Mirror the Rust reshape idiomatically (Go
  retains `*paxLeaf`; TS retains the parsed directories over a `Uint8Array.subarray`); each lands green
  independently. The `col_at`/`row_at_masked` accessors are ported too (S3-ready), just not driven.

Deferred follow-ons (none foreclosed): **S3 touched-column scan wiring** (above); **nested-value
structural memo** (skip re-parsing a single `jsonb`/array/composite value's *interior* on repeated
access — the narrow residual of §6, not the column-location memo PAX already provides); **keys as
block-slices** (zero-copy keys under Packed); **in-memory databases adopting deferral** only if a Memory
pager backing lands ([pager.md §6](pager.md)).

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
