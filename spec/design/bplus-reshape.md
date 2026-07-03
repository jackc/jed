# B+tree reshape — one packed representation, one storage path

> The decision to switch jed's on-disk/in-memory tree from a **B-tree** (records in every
> node) to a **B+tree** (records in leaves only), and to fold into the same reshape one further
> byte change and two representation changes it unblocks — the **per-column null bitmap** in
> the PAX leaf (the queued Stage-4 leaf change, absorbed here so the format breaks once),
> **retiring the `Decoded` node as a residency form** (one packed representation everywhere),
> and **backing in-memory databases and temp-table stores with a `MemoryBlockStore` through
> the pager** — all under a **single `format_version` bump (23 → 24)**.
> This is a *design/decision* doc: it records the call, the rationale, the new byte contract at
> a plan level, and the cross-doc deltas. The byte-exact layout + fixtures land in
> [../fileformat/format.md](../fileformat/format.md) when the slice is built. When a decision
> here changes, update [CLAUDE.md](../../CLAUDE.md) §9, [transactions.md](transactions.md) §3,
> [storage.md](storage.md) §6, and [../fileformat/format.md](../fileformat/format.md) in the
> same edit.
>
> **Status: B1 LANDED** (the format bump — all four implementations byte-identical at
> `format_version` 24, goldens regenerated incl. the `max_sep_table.jed` degenerate-fan-out
> fixture, corpus green in both storage modes with zero corpus cost drift; the exact byte
> contract is [../fileformat/format.md](../fileformat/format.md)). **B2–B4 pending.**
> Supersedes the "in-memory path deliberately
> left separate" carve-outs in [hosts.md §7](hosts.md) and [lazy-record.md §4/§11/§12](lazy-record.md),
> and the B-tree shape decided in [transactions.md §3](transactions.md). Absorbs the PAX
> Stage-4 **per-column null bitmap**, previously earmarked as its own v24 bump (the Stage-4
> text dictionary is *not* absorbed — it stays a deferred door behind the region flags byte).

---

## 1. What this decides

Four changes, deliberately folded into **one** format-breaking reshape because they touch the
same load-bearing code (the persistent tree, the page layout, the loader/serializer, the
split/merge byte contract, the goldens, and the cost baseline) and separating them would pay
that disruption two, three, or four times over:

1. **B-tree → B+tree.** Interior nodes stop carrying records; they become a pure
   `separator-keys + child-pointers` routing skeleton. **All records live in leaves.**
2. **Per-column null bitmap in the leaf (the absorbed PAX Stage-4 item).** Each leaf column
   region replaces the per-value `0x01` NULL presence tag with a **per-region null bitmap**,
   moving toward dense fixed-width value strides for vectorizable columns — the ideal
   gather/SIMD layout, and it *shrinks* the format. Folded in by this doc's own logic: it
   touches the same leaf codec, the same goldens, and the same cost baseline the reshape is
   already regenerating, and two queued leaf-touching format bumps would pay the full regen
   twice. (The Stage-4 **text dictionary** is *not* folded in — it stays a deferred door
   behind the per-region flags byte, §4.1.)
3. **Retire `Decoded` as a residency form (Packed-only).** With interior nodes record-free, the
   only record-bearing node is a Packed leaf. `Decoded` survives **only** as the writer's
   transient materialize-mutate-repack buffer, never a resident read representation. The
   two-form `rowAtMaybeMasked`/`rowAtMasked` read seam collapses.
4. **`MemoryBlockStore` + pinned pool.** An in-memory database — and every **temp-table store**
   (session-local and shared, [temp-tables.md §6](temp-tables.md), which today reuse the same
   fully-resident decoded-tree mode) — stores its data as page-format byte blocks in RAM, read
   through the *same* pager and Packed path as a file. This deletes the `persist` no-op /
   `resident_leaves == 0` / whole-image-`from_image` special cases and makes the in-memory
   footprint compact and honest.

**One `format_version` bump: 23 → 24.** Changes (1) and (2) break the on-disk bytes (the
interior page layout + record redistribution + split/merge rules, and the leaf column-region
encoding). Changes (3) and (4) are representation/residency changes that ride the same bump
without adding format cost (they are byte-invisible above the seam, the
[lazy-record.md §8](lazy-record.md) precedent). The result is the mature embedded-DB
architecture — the one PostgreSQL, SQLite, InnoDB, and **bbolt** (jed's own commit-model
reference, [CLAUDE.md §12](../../CLAUDE.md)) already use.

**Scope — which trees reshape.** The reshape covers every instance of the ordered B-tree:
**table stores**, **secondary btree indexes**, and **GIN entry trees** (a GIN index is the same
empty-payload B-tree, [gin.md §4](gin.md)) — and, through change (4), the **temp-table stores**
that reuse the machinery verbatim. For index/GIN trees the fan-out argument of §2 is neutral
(an empty-payload record already *is* its key, so a separator is the same bytes) — they reshape
for **uniformity**: one tree contract, not two. **GiST trees are untouched**: `page_type` 5/6
carry their own union-key layout ([gist.md §4.1](gist.md)) and are already leaf/interior-shaped;
their pages regenerate in the goldens only for the version byte + CRC. Change (2) is likewise
leaf-region-only: the tagged single-value codec survives where individual values are stored
(catalog defaults, overflow-chain content).

---

## 2. Why B+tree — and why *for jed in particular*

**jed never weighed B+tree against B-tree.** The only recorded tree deliberation
([transactions.md §3](transactions.md)) is *"Decided: B-tree, not a persistent BST"* — the axis
was binary-node vs. page-mappable node. Once "page-mappable" won, records-in-interior-nodes rode
along as an unexamined consequence. There is **no rationale on record** for choosing a B-tree
over a B+tree; it is an under-considered default. The tell: jed adopted **bbolt's**
single-writer / copy-on-write / meta-root-swap model wholesale, and *bbolt is a B+tree*
(`references/bbolt/README.md`: *"Bolt uses a B+tree internally"*), traversed by a **cursor
stack**, not sibling pointers — so the reference jed leaned on already made the opposite choice.

The case for switching:

- **Fan-out — and jed pays more than average for the current default.** A B+tree interior node
  holds only separator keys (raw encoded bytes) + child pointers, so it packs *far* more
  entries per page than a B-tree interior node carrying full rows. jed stores **multi-column
  rows**, so records-in-interior murders fan-out precisely because jed's records are wide →
  deeper tree → more `page_read`s per point lookup. B+tree makes the tree shallower exactly
  where jed is weakest.
- **It deletes the `Decoded`-interior wart at the root.** In a B+tree an interior node is
  `keys + children` — **no values, nothing to decode.** The "interior nodes are always `Decoded`
  and hold records" case from the read-path analysis does not get *packed*, it *ceases to
  exist*. This is the keystone that makes Packed-only (§1.3) clean rather than half-done.
- **Range scans simplify.** All records in leaves → a scan is a leaf walk (via a cursor stack —
  **no leaf sibling pointers**, which would break copy-on-write: fixing a neighbour's back-link
  forces copying the neighbour on every split; bbolt avoids them for exactly this reason). No
  interleaving of interior-node records into the in-order traversal. jed's scan-heavy surface
  (streaming cursor, bounded range scan, ORDER-BY-from-scan-order, index-nested-loop) all reads
  more cleanly over leaf-only records.
- **The record/value machinery lives in one node type.** PAX packing, the value codec,
  `RECORD_MAX`, spill-to-overflow — all leaf concerns. One place across three cores, not two.

The one B-tree advantage — no separator-key duplication, and a lookup can terminate at an
interior node — is marginal and, for wide-rowed jed, dwarfed by the fan-out loss. Not a reason
to keep it.

---

## 3. Why fold the representation changes in

Once interior nodes are record-free (§2), the residual `Decoded`-residency cases from the
read-path analysis are only **in-memory leaves** and **dirty leaves**. Both are cheap to close,
and closing them *here* — while the format is already breaking and the goldens/cost are already
being regenerated — costs no extra format churn:

- **In-memory leaves → Packed** via a `MemoryBlockStore` (a growable byte buffer implementing
  the five-method seam, [hosts.md §2](hosts.md)) + a **pinned pool** (no eviction; an in-memory
  DB is resident by definition). Commit *packs dirty nodes into memory pages* — structurally the
  file commit minus `fsync` — which **unifies the commit path** and removes the `persist` no-op.
  **Temp-table stores ride the same store** — they are today the same fully-resident
  decoded-tree mode ([temp-tables.md §6](temp-tables.md)), and leaving them behind would keep
  the second read path alive. A bonus: their deferred **spill-to-disk seam gets easier** — a
  store already on a `BlockStore` spills by swapping in a temp-file `BlockStore`, not by
  growing a new mechanism.
- **Dirty leaves → transient write-scratch.** A mutation still materializes a leaf's records to
  edit/split/repack, but that `Decoded` buffer is now ephemeral and write-path-only, not a node
  the read path ever sees.

Net: `Decoded` stops being a residency/read form entirely. The resident tree is **skeletal
interior nodes + Packed leaves**, everywhere, in-memory and on-disk alike. The memory-vs-disk
divergence class (the one that produced the window-operand / touched-set bugs) is gone **by
construction** — there is one read path, so a touched-set/mask defect surfaces identically in
every mode and every test.

---

## 4. The new B+tree byte contract (plan level)

The exact bytes + fixtures are authored in [../fileformat/format.md](../fileformat/format.md)
when B1 lands; this fixes the model the bytes must realize.

### 4.1 Node layouts (`format_version` 24)

- **Leaf (`page_type` 2)** — holds records (key + all column values). The **v23 PAX column-major
  *shape* carries over** (key directory ‖ key blob ‖ column directory ‖ per-column regions):
  leaves are already the good part. Two changes. (a) *Which* records are here — **all of
  them**, including those that used to live in interior nodes. (b) **The column-region
  encoding changes (§1 change 2):** each region gains a small region header (a **flags
  byte** — also the door the deferred text dictionary will later use) and a **per-region null
  bitmap** replacing the per-value `0x01` NULL tag, so NULL cells contribute no per-value tag
  bytes and non-NULL runs move toward a **dense fixed-width stride** the vectorized executor
  can gather without per-cell tag dispatch. The exact byte contract — the flags-byte values,
  the bitmap placement, whether a NULL occupies a body slot or is omitted (fixed stride vs.
  compactness), and the restated per-record `record_size` convention (the split weight and the
  temp-budget basis) — is finalized at B1 in [../fileformat/format.md](../fileformat/format.md)
  against the fixtures. `RECORD_MAX` stays a leaf property (§4.2).
- **Interior (`page_type` 3)** — **new, record-free layout**:
  `N+1 child pointers (big-endian u32) ‖ separator-key directory (N+1 u32 prefix-sum) ‖ key
  blob`. No column directory, no value region. A separator is a **copy of the boundary key**
  (the first key of the right subtree — the standard B+tree copy-up separator). Interior fan-out
  is governed by `(separator-key + child-pointer)` fit — much higher than v23's `record +
  pointer` fit.

Keys everywhere remain **raw order-preserving encoded bytes** ([encoding.md](encoding.md)), so
descent/compare is byte-memcmp with no decode — unchanged from today, and the property that lets
interior nodes stay a pure resident skeleton.

### 4.2 Split / merge (the load-bearing byte contract — regenerated)

The v23 rule — *"payload > C → 2-way median-**record**-promote"* ([transactions.md §3](transactions.md))
— is replaced by the standard B+tree split, which differs by node kind:

- **Leaf split (copy-up).** A leaf whose payload exceeds `C` splits into two leaves; the **first
  key of the new right leaf is *copied* up** into the parent as a separator. The record **stays
  in the leaf** (unlike the old median-record promotion).
- **Interior split (push-up).** An overflowing interior node splits; the **median separator
  *moves* up** into the parent (it is a pure routing key, not copied — nothing owns it below).
- **Merge / rebalance.** An underfull leaf merges with a sibling and its parent separator is
  removed; an underfull interior merges by **pulling the parent separator down**. The
  underfull threshold + the merge-then-maybe-split shape mirror the current rebalance, restated
  for the two node kinds.

The split point, fan-out determination (page-fit, not a tuning constant), and the single-record
cap are re-derived for the two-layout world and **pinned with golden fixtures** — this is a §8
cross-core byte contract, so all four implementations (Rust/Go/TS + the Ruby reference) must
produce byte-identical trees.

**`RECORD_MAX` keeps its value deliberately.** The current cap `(C − max(12, 12 + 16·K)) / 2`
is co-justified today by the *interior* record-pair fit ([../fileformat/format.md](../fileformat/format.md)
*Why the record cap*) — a justification that evaporates when interiors hold separators instead
of records. The value is **kept anyway**, re-derived leaf-only (a two-record leaf must fit
`C`): loosening the cap would move the overflow/spill thresholds of
[large-values.md](large-values.md) and churn every large-value fixture for no real gain.

**Degenerate fan-out is part of the contract.** A separator is a copy of a key, and an
index/GIN record *is* its key, so a separator can be as large as `RECORD_MAX(0) = (C − 12)/2` —
an interior node may then fit only **one** separator (two children). The split/merge rules must
handle `N = 1` interiors, the minimum-fan-out invariant is stated explicitly (an interior
always fits ≥ 1 separator + 2 pointers — guaranteed by the kept cap), and a
**max-size-separator golden fixture** pins the degenerate shape.

### 4.3 Range traversal

A cursor holds a **stack of `(node, index)` frames** from root to the current leaf (bbolt's
model). `next()` advances within the leaf, or pops to the parent and descends the next child
when the leaf is exhausted — **no sibling pointers** (COW-incompatible). This is close to what
jed's in-order B-tree cursor already does; the simplification is that only leaves yield records,
so there is no interior-record interleaving to special-case.

---

## 5. The unified representation

- **`Decoded` retires as a residency form.** Interior nodes are `keys + children` (no `vals`);
  leaves are Packed (`packedLeaf` = page block + PAX directories, values reconstructed on
  demand). The `rowAtMaybeMasked` / `rowAtMasked` two-form seam collapses to the Packed path.
- **`Decoded` survives as a transient write buffer only.** `UPDATE`/`INSERT`/`DELETE`
  materialize the touched leaf's records, mutate, re-split/merge, re-pack to a page, and
  publish. Nothing above the writer sees it.
- **`MemoryBlockStore` + pinned pool** backs in-memory databases: a growable RAM byte buffer,
  `sync()` a no-op, a non-evicting pool (all pages pinned resident). In-memory `commit` packs
  dirty nodes to memory pages (no `fsync`) — the file commit minus durability. **Temp-table
  stores** (session-local + shared) back onto the same store kind; whether each temp store owns
  a `MemoryBlockStore` or a session shares one is a B3 decision. The `temp_buffers` /
  `shared_temp_mem` budget survives **by construction** — it is measured in on-disk
  record-encoding bytes, deliberately representation-independent
  ([temp-tables.md §7](temp-tables.md)) — so its *values* move only with the v24 record
  encoding itself (the change-2 NULL bytes), which is part of the one re-baseline.
- **Demand-fault backstop on the single read path.** With one read path, the correctness fix
  from the touched-set analysis lands once: an `Unfetched` that the static touched set missed is
  **resolved on demand** (inline-deferred from its own block span with no pager; external-chain
  through the threaded resolve-context) rather than poisoning/NULL-folding. The static touched
  set stays the **cost basis + prefetch hint**, not the definition of correctness (see §6).

**`resident_leaves` becomes a real count for in-memory** (no longer defined `0`); `cache_bytes`
either applies to the in-memory pinned pool or is treated as unbounded-pinned — a knob decided
at B3 (default: in-memory pins everything, `cache_bytes` bounds only file-backed eviction, so
the observable default is unchanged).

---

## 6. Cost re-baseline

Cost stays a §8 cross-core contract with the **same accrual shape** ([cost.md §3](cost.md)):
`page_read` per B-tree node touched, one per overflow-chain page of every touched large value,
`value_decompress` per touched compressed slab — all from the **static touched set**, charged
up-front, a logical count. **What changes is the numbers**, because the tree shape changes:

- Higher interior fan-out → **shallower tree** → fewer `page_read`s per point lookup (cost
  generally **drops** for lookups).
- Records redistributed to leaves → a full scan's node count changes; scans touch the interior
  skeleton to navigate + all leaves.
- The null bitmap (§1 change 2) shrinks record encodings wherever NULLs occur → node boundaries
  and split points shift → the **same** single re-baseline absorbs it. One more reason the fold
  pays: the cost values move **once**.
- **Every `# cost:` corpus value and every per-core cost-suite value is regenerated** and
  re-agreed across Rust/Go/TS. This is the largest ripple of the reshape and the reason it is a
  deliberate, format-bumping slice rather than a quiet change.

The touched-set-as-cost basis is unchanged; §5's backstop decouples *correctness* from it, so a
prediction miss becomes a (bounded, deterministic) cost under-estimate + a tiny unpredicted
physical fetch, never wrong rows. The backstop fetch itself stays **unmetered** — deliberately:
metering it would make cost depend on prediction quality (a per-core bug surface) rather than
the spec'd static set. This does not soften the §13 bounded-resources guarantee — a touched-set
miss is an engine defect, not something a query can construct on demand, and the backstop reads
only pages the query's own leaf/overflow set already bounds.

---

## 7. Goldens, fixtures, and the cross-core round-trip

- **Regenerate every `.jed` golden** (version byte 24, new interior pages, new split shapes,
  new CRCs) across Rust/Go/TS + the Ruby reference, in lockstep — the atomic-bump discipline
  every prior `format_version` used ([../fileformat/format.md](../fileformat/format.md)).
- **New split/merge byte fixtures** pinning the copy-up / push-up rules and interior fan-out.
- **The cross-core round-trip test holds** (a file written by any core is byte-readable by any
  other — [CLAUDE.md §8](../../CLAUDE.md)); it is the primary guard that the three hand-written
  B+tree implementations agree.
- **`spec/fileformat/verify.rb` moves with the format.** The image-forging verifier (kept by
  standing decision — it forges un-constructable images the cores can't) learns the v24
  interior layout + the leaf region bitmap in the same slice; a verifier that lags the format
  verifies nothing.
- **The two-storage-mode corpus** ([conformance.md](conformance.md), memory vs. file+reopen)
  becomes largely redundant for divergence-catching (one path now) but is **kept** as the guard
  for the fault/eviction path a reopen exercises. Revisit the 14 `# skip: disk` reopen-fragile
  files — with one read path some skips should become unnecessary, and each removal is a small
  coverage win.

---

## 8. Determinism & memory safety (invariant)

- **No new nondeterminism.** Split/merge is deterministic and byte-identical cross-core; range
  order is ascending encoded-key as today; cost is the static touched set. Iteration order,
  wall-clock, and allocation order stay out of results/cost/bytes ([CLAUDE.md §8](../../CLAUDE.md)).
- **Memory safety holds.** The cursor stack and the no-construct decode walk are owned/sliced
  traversals in every core — no `unsafe`, no cgo ([CLAUDE.md §2/§13](../../CLAUDE.md)); the
  `MemoryBlockStore` is a plain byte slice.
- **Encryption / replication doors unaffected.** They ride the block seam below the pager
  ([hosts.md §6](hosts.md)); a `MemoryBlockStore` is just another base host under the same
  codec/tee layering.

---

## 9. Slicing (one format bump at B1, representation after)

Sequenced so the format-breaking structural change lands and stabilizes first, then the
representation/residency changes ride byte-invisibly on top:

- **B0 — spec (this doc) + the §10 doc deltas + TODO.md.** *No code.*
- **B1 — the B+tree + the leaf null bitmap, format bump 23 → 24 (the big one).** The persistent
  map becomes a COW B+tree (interior = keys+children, records leaf-only, copy-up/push-up split,
  cursor-stack range scan); the new interior page layout + loader/serializer; the new leaf
  column-region encoding (flags byte + null bitmap, §4.1) — the bitmap is **bump-bound**, so it
  cannot be split out of B1 without paying a second bump. Regenerate goldens + split/merge
  fixtures (including the max-size-separator case, §4.2) + `verify.rb`; **re-baseline all cost
  values**. The existing packed-leaf / PAX read machinery is **carried forward inside B1** over
  the new region encoding — the disk-mode corpus stays green throughout; B2 is a simplification,
  not the introduction of packed leaves. Land **Rust-first, then Go/TS, then the Ruby
  reference**, each internally consistent (its in-memory tree and on-disk format move together),
  cross-core pinned by the round-trip goldens. This is the format contract; everything after is
  representation-only.
- **B2 — the Packed simplification.** With the read machinery already live over v24 leaves
  (B1), delete what the reshape obsoleted: interior nodes are never packed (they are the
  keys-only skeleton), so the interior-`Decoded`-with-records case and its share of the read
  seam go away. *No further format bump.*
- **B3 — `MemoryBlockStore` + pinned pool.** In-memory leaves become Packed through the pager;
  move the **temp-table stores** (session-local + shared) onto the same store (per-store vs.
  per-session granularity decided here; the record-byte `temp_buffers` basis carries over
  unchanged — §5); unify `commit`; delete the `persist` no-op / `resident_leaves == 0` /
  whole-image special cases; decide the in-memory `cache_bytes` semantics (§5). *No format
  bump.*
- **B4 — retire `Decoded` + the demand-fault backstop.** Confine `Decoded` to the write-scratch
  buffer; delete the `rowAtMaybeMasked`/`rowAtMasked` two-form seam; add the resolve-on-demand backstop to
  the single Packed read path (correctness no longer rides on the static touched set). *No format
  bump.*

**Benchmarks — once, after B4.** Pin baseline numbers (`rake bench:run`) on the pre-B1 master,
then re-run the storage-touching bench families (point lookup, scans/aggregates, insert/commit,
`in_list_*`, `index_range`, `concurrent_read`, the window benches) after B4 and report both
numbers per [CLAUDE.md §10](../../CLAUDE.md). Deliberately **not per-slice**: B1–B3 are
transitional states whose numbers decide nothing; the before/after that matters is the whole
reshape (shallower lookups vs. in-memory commit now paying packing and reads reconstructing on
demand). The `bench-setup` fingerprint + opens gate regenerates the `.jed` datasets across the
bump automatically ([benchmarks.md](benchmarks.md)).

**Risk valve.** If a single format-breaking reshape across three storage cores + Ruby is too
large to land safely at once, B1 alone is still coherent and shippable (B+tree + null bitmap,
format bump, interior records vanish, cost re-baselines once); B2–B4 then follow as
no-format-bump representation slices. What to **avoid** is the reverse order (Packed-only
first, B+tree later) — it re-baselines cost and regenerates goldens twice and would pack
interior nodes only to delete their records.

---

## 10. Doc deltas (apply when B1 lands, per the [CLAUDE.md §10](../../CLAUDE.md) "same change" rule)

- **[CLAUDE.md](../../CLAUDE.md) §9** — prepend `format_version` 24 to the landed-format list
  (B+tree: record-free interior pages + leaf-only records + new split/merge contract + the
  per-column leaf null bitmap); update the in-memory-host description (now `MemoryBlockStore` +
  pinned pool through the pager, not a separate decoded-tree path); note the in-memory footprint
  is now compact (serves "the in-memory representation is a first-class concern").
- **[transactions.md §3](transactions.md)** — "copy-on-write B-tree" → "copy-on-write **B+tree**";
  replace the median-record-promote split rule with the copy-up (leaf) / push-up (interior)
  rules; state records are leaf-only and interior nodes are a keys+children skeleton; `get`
  descends to a leaf (interior nodes route only); range = cursor stack, no sibling pointers.
- **[storage.md §6](storage.md)** — the "Within-page structure" bullet: interior nodes hold
  `separators + N+1 child pointers` only (no records); leaves hold **all** records; drop
  "an interior node prefixes its records with N+1 child pointers".
- **[../fileformat/format.md](../fileformat/format.md)** — the authoritative bytes: `format_version`
  24, the new interior page layout, record redistribution, the new leaf column-region encoding
  (flags byte + null bitmap) + the restated `record_size`, the split/merge byte contract + new
  fixtures (including max-size-separator), the leaf-only *Why the record cap* re-derivation, the
  version-history line.
- **[lazy-record.md](lazy-record.md) §4/§11/§12** — supersede "pure in-memory stays fully decoded"
  (in-memory is now Packed through the `MemoryBlockStore`); promote the §12 "in-memory databases
  adopting deferral (only if a Memory pager backing lands)" follow-on to **done (B3)**.
- **[hosts.md](hosts.md) §4/§7** — the in-memory catalog row: "separate path (decoded tree)" →
  "`MemoryBlockStore` + pinned pool, through the pager"; supersede the §7 "in-memory path
  deliberately left separate" note (now unified); add the `MemoryBlockStore` host; the §4 row's
  "commit is a no-op success" becomes "commit packs dirty pages to memory pages — no `fsync`,
  same observable success".
- **[pager.md](pager.md)** — the buffer pool is now *actually* universal (in-memory no longer
  bypasses it); document the pinned/no-evict mode for in-memory.
- **[packed-leaf.md](packed-leaf.md)** — interior nodes are no longer a `Decoded`-with-records
  case; the two-form `rowAtMaybeMasked`/`rowAtMasked` seam retires (B4); the region encoding
  gains the flags byte + null bitmap; update accordingly.
- **[temp-tables.md](temp-tables.md) §6/§7** — the storage model moves from the fully-resident
  decoded-tree mode to `MemoryBlockStore` + pinned pool through the pager (B3); the deferred
  spill-to-disk seam becomes a `BlockStore` swap; §7 restates the `record_size` the budget sums
  under the v24 encoding (basis unchanged — record bytes, representation-independent).
- **[indexes.md](indexes.md) / [gin.md](gin.md)** — both trees are instances of the reshaped
  ordered B-tree (empty-payload records now leaf-only); they defer to format.md for the byte
  contract — sweep for stale interior-record wording (e.g. indexes.md's right-edge-append
  split note). **[gist.md](gist.md) needs no delta** (own `page_type` 5/6 layout, untouched — §1).
- **[cost.md §3](cost.md)** — same accrual shape, re-baselined values (new node counts / depth);
  note the static touched set is now cost-basis + prefetch, with correctness via the B4 backstop.
- **[api.md §2.1](api.md)** — `resident_leaves` is a real count for in-memory (no longer `0`);
  restate the in-memory `cache_bytes` / `work_mem` handling per §5; §2.2's in-memory `commit`
  wording (no longer a pure no-op — it packs dirty pages; observable result unchanged).
- **[conformance.md](conformance.md)** — the two-storage-mode corpus is retained as the
  fault/eviction guard (its divergence-catching role is subsumed by one-path-by-construction);
  revisit the `# skip: disk` list (§7).
- **[TODO.md](../../TODO.md)** — under "Storage maturation (§9)", add the reshape as an XL item
  with slices B0–B4, the format bump, the cost re-baseline, and the goldens regen; cross-link the
  now-done "Lazy record decode" and the retired "in-memory left separate" carve-out; absorb the
  PAX Stage-4 per-column null bitmap (previously earmarked as its own v24 bump) into this item —
  the text dictionary stays a deferred door.

---

## 11. Risks & open questions

- **Biggest single ripple: the cost re-baseline.** Every `# cost:` value moves; the whole cost
  suite is regenerated and re-agreed cross-core. Non-negotiable and non-mechanical (the numbers
  come from the new tree shape and the new record encoding) — budget for it explicitly.
- **Most-load-bearing code, three hand-written cores + Ruby.** Split/merge is the code most
  guarded by fixtures; a divergence is a byte-contract break. Land Rust-first behind the goldens,
  then port under the round-trip test.
- **Interior separator convention** — copy-up "first key of the right subtree" vs. alternatives —
  finalize at B1 against the goldens (pick one, pin it).
- **Dense-region encoding details** (§4.1) — NULL body slot vs. omitted body (fixed stride vs.
  compactness), the region flags-byte values, the restated `record_size` — finalize at B1
  against the fixtures *and* the vectorized executor's gather needs. Actually **consuming** the
  dense stride in the vectorized executor is a follow-on above the format (no bump), not part
  of this reshape.
- **In-memory `cache_bytes` semantics** (§5) — pin at B3 to keep the observable default unchanged.
- **Temp-store granularity** — one `MemoryBlockStore` per temp store vs. per session — decide at
  B3 (observable only through internals; the record-byte budget is representation-independent).
- **Persistent-BST fallback is gone.** transactions.md §3 kept a BST as the documented fallback
  "if the B-tree proves too costly to keep in lockstep"; a B+tree is *more* structure to keep in
  lockstep, so this reshape doubles down on the page-mappable-tree bet. Acceptable — the goldens
  + round-trip already make the current B-tree lockstep tractable — but worth naming.
