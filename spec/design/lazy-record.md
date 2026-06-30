# Lazy record decode — compact resident leaves + on-demand column decode

> The reasoning behind the **last** lazy-decode level (CLAUDE.md §9, [streaming.md §1](streaming.md)
> "small inline columns"): stop materializing every column of every record into a decoded `Row` at
> leaf-fault time, and instead keep a faulted leaf as its **compact on-disk bytes**, decoding each
> column **on demand** — the moment the query's touched set actually reads it. This is jed's
> equivalent of PostgreSQL `slot_getsomeattrs` / SQLite `OP_Column`, and it **subsumes** the
> deferred [streaming.md §8](streaming.md) "S5" item. This is a *design* doc; the touched-set cost
> contract is [cost.md](cost.md) §3, the large-value lazy path it generalizes is
> [large-values.md §14](large-values.md), the residency model it extends is [pager.md](pager.md),
> the byte format it does **not** change is [../fileformat/format.md](../fileformat/format.md), and
> the snapshot lifetime it composes with is [transactions.md §5/§8](transactions.md). When a
> decision here changes, update [CLAUDE.md](../../CLAUDE.md) §9, [pager.md](pager.md) §3, and
> [streaming.md §8](streaming.md) in the same edit.

**Status: SPEC (no code).** This doc fixes the model and the slice sequence; nothing is built yet.
It is the successor to streaming.md §8 "S5 — lazy small-inline-column decode," **promoted from a
localized codec tweak to a storage-core reshape** after the finding in §2 — that the narrow S5
fights jed's architecture for a fraction of the win, while the reshape attacks the root cause and
pays back in three independent currencies (decode skip, clone elimination, resident-memory
reduction). Built Rust-first, then Go/TS in lockstep (§11). The slices land **cost- and
byte-identical** (§8), so each core lands green independently — the pager (P6.4) / streaming
(S3/S4) precedent.

---

## 1. The gap this closes (the last of four)

[streaming.md §1](streaming.md) tabulates four lazy-decode levels. jed closed three; this doc
closes the fourth, and reframes it:

| Level | jed status before this doc | Where |
|---|---|---|
| **Pages** | ✅ lazy (demand-paged) | bounded CLOCK buffer pool ([pager.md](pager.md), P6.4). |
| **Large / spilled values** | ✅ lazy (per touched column) | `Unfetched` references resolved only for touched columns ([large-values.md §14](large-values.md)). |
| **The result rows** | ✅ lazy (pull cursor) | the streaming `Rows` cursor ([streaming.md](streaming.md), S3/S4). |
| **Small inline columns** | ❌ eager — **this doc** | `decode_leaf_node` → `decode_record_lazy` materializes **every** inline value of a record into a decoded `Row`; only *large* values stay `Unfetched`. |

The framing the rest of this doc rests on: **this is not a new mechanism — it is the
[large-values.md §14](large-values.md) lazy-value path generalized from "large values only" to
"every value."** Today a faulted leaf's `read_value_lazy` produces an `Unfetched` reference for an
external/compressed value but eagerly constructs every inline value. Make the inline values lazy
too and the §14 machinery — the `Unfetched` variant, the `needs_resolution` gate, the
`resolve_columns(row, mask)` resolve at the four read sites, the static touched-set cost contract,
the "escaped reference is poisoned" guard — **carries the whole feature unchanged**. The new
surface is small and local: a way to find a value's byte span without constructing it (§6), and a
choice of where the deferred bytes live (§5).

---

## 2. Why the narrow S5 doesn't pay in jed — and the reshape does

streaming.md §8 specced S5 as "skip *decoding* untouched small inline values … a localized codec
change behind the existing mask," modeled on PG/SQLite. Implementing it surfaced an architectural
mismatch worth recording, because it is *why* this became a reshape:

- **PG and SQLite decode lazily *in the page buffer*.** The tuple stays in the pinned shared page;
  a column is deformed (`slot_getsomeattrs`) / extracted (`OP_Column`) in place, on demand, and
  copied out only when materialized. Deferring a column there costs **nothing** — the bytes are
  already resident and shared.
- **jed's decoded `Row` is detached and owned.** A leaf faults once into a resident `Node`
  (`vals: Vec<Row>` — Rust; `[]storedRow` — Go; `Row[]` — TS), and **every scan deep-clones the
  row out of that shared, immutable node** (`pmap` `Step::Emit(… vals[p].clone())`). So a narrow,
  decode-in-place S5 cannot land: there is no shared page tuple to decode in place, only a
  per-scan deep clone of an already-decoded tree.

Run the narrow S5 (defer a decoded column to an owned-bytes reference, decode on touch) through
that clone and it is a wash-to-loss except in one corner:

| column, per scan | eager today | narrow S5 (defer-in-place) | verdict |
|---|---|---|---|
| untouched **jsonb / array / composite** | deep-clone the whole `Value` tree (many allocs) | clone one compact byte span | **win** |
| untouched **text / bytea / decimal** | clone one `String`/`Vec`/digit-vec | clone one byte span | wash |
| **touched** anything | clone the constructed `Value` | clone the span **then** construct | slight **loss** (extra clone) |
| fixed-width (`int`/`bool`/`uuid`/`ts`/`float`) | trivial copy | — (leave eager) | n/a |

So the narrow S5's clear win is only *untouched deep-tree columns*, and it slightly **regresses**
the common touched path — for a larger, more drift-prone change (a per-type skip-walker mirrored
across three cores). The root cause is the **eager decode + per-scan deep clone**, not the
decode-skip. This reshape removes the root cause: the resident leaf becomes **compact bytes**, the
per-scan clone becomes an `Arc` bump or a single flat copy (§5), and lazy column decode falls out
**uniformly** (every type, no per-type rule) and **for free** (no touched-path regression — a
touched column is decoded once from the span it would have cloned anyway). It also pays back in two
currencies the narrow S5 never touched: **clone cost** (§5) and **resident memory** (§9).

---

## 3. What PG/SQLite do, and the one enabler that makes it cheap in jed

- **PostgreSQL** — `heap_deform_tuple` / `slot_getsomeattrs(n)` deform a tuple only up to the
  **highest referenced attribute**, in the shared buffer page; trailing attrs are never touched.
- **SQLite** — `OP_Column` parses the record header and extracts **one column on demand**, caching
  offsets, stopping at the max referenced column, reading straight out of the page-cache page.

Both decode out of the **resident page**, with no detached copy. The reshape brings jed to the same
shape, and there is **one structural fact that makes it cheap**: jed's B-tree navigation **never
decodes values.** Keys are stored as **raw order-preserving encoded bytes** (`Node.keys:
Vec<Vec<u8>>` / `[][]byte` / `Uint8Array[]`) and every descent / split / merge / range bound
compares them as bytes ([encoding.md](encoding.md)). Only the **value region** (`vals`) is decoded
eagerly today, and **nothing structural depends on it being decoded.** So the value region can go
lazy **without touching the navigation / split / merge code** — exactly the load-bearing storage
core one would least want to disturb across three hand-written implementations.

---

## 4. The model: universal lazy value deferral

The change, stated against the existing code:

- **At fault** (`decode_leaf_node` → `decode_record_lazy` → `read_value_lazy`): parse each record's
  **structure** — key, then per-column presence tag + body **extent** — **without constructing** the
  value body. Each present inline value becomes a deferred reference carrying its type-relevant
  bytes (§5/§6); NULL stays `Value::Null`; a large value stays the §14 `Unfetched::External /
  InlineComp / ExternalComp` it already is. Keys decode exactly as today (navigation needs them).
  The record **weight** is the bytes it occupies on the page — read off the parse cursor, equal to
  the writer's `record_size`, exactly as `decode_record_lazy` already does.
- **At scan emit**: the row of deferred references is cloned out of the resident node as today, but
  the clone is now an `Arc` bump (§5a) or a single flat byte copy (§5b) per value — **never a deep
  `Value`-tree clone**.
- **At a read site**: `resolve_columns(row, mask)` decodes exactly the columns the query's
  **static touched set** selects ([cost.md §3](cost.md)) — the *same call, the same mask, the same
  four sites* the §14 large-value path already uses (materialize, streaming-LIMIT, `DELETE`,
  `UPDATE`). An untouched column is dropped **still deferred**, never decoded.
- **At a write site** (`UPDATE` rewrite): `resolve_all` materializes the rewritten row fully
  resident before re-encoding, so weight/disposition re-plan exactly as an eager writer's — §14's
  rule verbatim.

The **`Value::Unfetched`** variant widens from "a large-value reference" to "any not-yet-decoded
value." The `Row` / `Node` / `Vec<Value>` types are **unchanged in shape**; `needs_resolution`, the
poison guard ("an escaped `Unfetched` panics, never reads as NULL"), and the resolve plumbing all
**generalize without restructuring**. This is the whole reason to route through §14 rather than
inventing a parallel mechanism.

**Scope mirrors the residency model exactly.** Deferral applies to the **demand-paged, file-backed
leaf path** — the same path §14 and the buffer pool ([pager.md §1](pager.md)) apply to. The
**whole-image `from_image` load stays eager** (it has no pager to resolve through later — §14's
carve-out), so a **pure in-memory database stays fully decoded** (it is RAM-resident by definition;
there is nothing to page from and no resident-memory pressure to relieve). A `Row` therefore freely
mixes decoded and deferred values across the two paths, exactly as it already does today (a faulted
leaf holds decoded-small + `Unfetched`-large; `from_image` holds all-decoded).

---

## 5. The resident representation (not a byte contract — per-core idiomatic)

Where do a deferred value's bytes live? Two forms, **(a)** the target and **(b)** an acceptable
lower-risk stepping stone. Crucially, **this choice is invisible** — results and cost are identical
either way (§8) — so, exactly like the buffer pool's CLOCK ([pager.md §3](pager.md)), it is **not a
§8 byte contract** and each core may choose idiomatically and even upgrade later without
cross-core coordination.

- **(a) Zero-copy, block-shared (the target).** The faulted leaf **retains its page block**, and a
  deferred value is `(shared block, offset, len, type)`. The fault does **no** per-value copy
  (just a structure parse to find spans), and the scan-emit clone is a **refcount bump** of the
  shared block. Resident leaf memory is then **≈ the page bytes themselves** (one `page_size` block
  + a thin per-record index), shared across every reader of that leaf — the literal PG/SQLite
  model and the honest buffer-pool bound (§9). Per-core: **Go/TS get it almost for free** — a
  `[]byte` slice / `Uint8Array` subarray **is** a view that keeps the backing block alive under GC;
  **Rust** uses an `Arc<[u8]>` (or the block `Arc<Node>` already in the pool) plus an offset. The
  deferred value holds the shared block, so it stays `'static` and composes with the streaming
  cursor's existing snapshot pin (§7).
- **(b) Owned span (lower-risk first functional slice).** A deferred value owns a `Vec<u8>` /
  `[]byte` / `Uint8Array` copy of its body bytes. The fault copies once (cheap — a flat memcpy, not
  a tree build), the scan-emit clone copies the flat span. This already **kills the deep-tree clone
  and defers decode** (the two wins of §2), at the cost of a per-record copy (a) avoids. Simpler
  lifetimes (`'static` trivially, no block retention), so it is the natural first landing; a core
  can upgrade (b)→(a) for the memory/zero-copy win whenever it chooses.

**Recommendation:** target **(a)**; land **(b)** first where the lifetime work in a core is
non-trivial (chiefly Rust), then upgrade. The keys may likewise become block slices under (a)
(zero-copy keys) — a further follow-on; keep them owned initially (small, and navigation reads them
constantly).

---

## 6. The lazy-decode seam: structure-parse without construction (drift-free)

To defer a value, the fault must find its body's byte **extent** without **constructing** the
value — otherwise it has done the very work it means to skip. The body extents are not
self-delimiting at the record level (values are concatenated; [format.md](../fileformat/format.md)
*Record*), so the structure must be walked: a `text`/`bytea`/`json` body is `u16 len + bytes`, a
`decimal` is a small header + groups, a `jsonb` is a self-delimiting tagged tree, a
composite/array/range recurses over its element bodies.

**Do not hand-write a second, parallel "skip" walker** — that is a per-type, three-core surface
that must track the decoder forever (the §5 drift trap, in codec form). Instead, **thread the
existing decoder in a no-construct mode**: `read_inline_body` / `read_inline_scalar` /
`decode_decimal_body` / `decode_jsonb_body` / `read_composite_body` / `read_array_body` /
`read_range_body` gain a mode in which they advance the cursor **identically** (read the same
lengths, take the same bytes, recurse the same way) but **skip the leaf value construction**
(no `String::from_utf8`, no `Decimal::from_codec`, no `JsonNode`/`Vec<Value>` tree). Because the
cursor advance is the **same code**, column boundaries are found **identically to the eager decode
by construction** — zero drift. The deferred value records `span = bytes[start..cursor]`; resolution
later runs the **same** decoder in normal (construct) mode over `span` and, as a cheap invariant,
**must consume exactly `span.len()`** (a debug assert that catches any future divergence at test
time).

Fixed-width scalars (`int`/`bool`/`uuid`/`timestamp*`/`date`/`interval`/`float`) construct in
O(1) with no allocation — deferring them buys nothing and would only add an `Unfetched` wrapper, so
they are **decoded eagerly even on the lazy path** (the §2 table's last row). Deferral targets the
variable-length / structured bodies, where the on-disk form is far cheaper to keep (and clone) than
the expanded `Value`. This keeps the seam small while capturing essentially all of the win.

---

## 7. Snapshot lifetime, copy-on-write, and the watermark (composition only)

The reshape introduces **no new lifetime model** — it composes with the three already in place:

- **Copy-on-write immutability ([transactions.md §2](transactions.md)).** A clean leaf is immutable
  on disk; a deferred value referencing its block (§5a) or copied from it (§5b) reads bytes that
  never change under it. Resolution works on the **scan's own cloned row**, never the shared tree —
  §14's rule, unchanged — so repeated scans re-resolve (and re-charge) consistently and snapshots
  stay immutable.
- **The buffer-pool pin ([pager.md §3/§4](pager.md)).** Under (a) a deferred value holds the leaf
  block alive (the `Arc` / GC view **is** the pin), so a value outliving its pool entry keeps its
  bytes — identical to how an in-flight node reference already survives eviction. Under (b) the
  value owns its bytes and needs no pin.
- **The streaming cursor's snapshot ([streaming.md §5](streaming.md)).** A streaming `Rows` cursor
  already pins its root snapshot and registers in the reader-liveness watermark; a row of deferred
  values it yields is `'static` for the same reason its `Unfetched` large values already are. No
  new watermark interaction: resolution reads only the pinned snapshot's pages.

Mutation is unchanged in contract: `DELETE` resolves only its filter columns (dropping a row reads
no bodies); `UPDATE` resolves filter + assignment-source columns and `resolve_all`s the rewritten
row before re-encode; the write side stays metered by `value_compress` per stored row version
([cost.md §3](cost.md)).

---

## 8. Determinism & cost — invariant (the contract that lets each core land green)

This is the load-bearing simplification, identical in spirit to the buffer pool
([pager.md §5](pager.md)) and the spill sorter: **the reshape changes *when* a value is decoded and
*where* its bytes live, never *what* a query observes or *what* it costs.**

- **No format change.** On-disk bytes, key encoding, and the goldens are untouched — this is a
  resident-representation/decode-timing change above the block seam. **No `format_version` bump.**
- **Cost is invariant.** Cost is the **static touched set** — `page_read` per node + chain page,
  `value_decompress` per compressed slab, computed at plan time from the columns the query
  references ([cost.md §3](cost.md)). It depends on neither decode timing nor resident
  representation. jed meters **no per-column-decode unit**, so deferring a decode moves no charge.
  Every `# cost:` corpus value holds; the per-core cost suites are unchanged.
- **Results are invariant.** A resolved deferred value equals the eagerly-decoded value
  byte-for-byte (same decoder, §6). Row order is still defined only by `ORDER BY` (CLAUDE.md §8);
  the deferral keys on nothing nondeterministic. The corpus drives `execute()` and is green by
  construction.
- **Errors move only in timing, never in identity.** A `data_corrupted` (`XX001`) from a malformed
  body now surfaces **when the column is touched** rather than at fault — exactly as §14 already
  moved a corrupt overflow chain's error to touch-time (its tests pin "open and untouching queries
  succeed; touching the column is the moment `XX001` surfaces"). An **untouched** corrupt inline
  value is, like an untouched corrupt chain, not read — a deliberate, already-established
  consequence of lazy decode.
- **The poison guard holds universally.** An `Unfetched` that escapes resolution (an engine bug)
  panics/throws on render/compare/encode — never read as NULL (§14). Widening deferral to all values
  widens what this guard protects; it does not weaken it.

Because cost, bytes, and results are all invariant, the slices are **corpus-transparent** and each
core lands independently — no new capability flag, the P6.4 / S3 / S4 precedent.

---

## 9. Memory — the honest buffer-pool bound (the reshape's biggest dividend)

Beyond the decode skip and the clone elimination, the reshape **shrinks resident memory**, and this
is the argument that most directly serves CLAUDE.md §9 ("the in-memory representation is a
first-class concern" + "must not foreclose larger-than-RAM"):

- A decoded `Row` is **much larger than its compact on-disk form**: each `Value` is a 24–32-byte
  tagged enum plus any heap payload (a `String`, a `Vec`, a `Decimal` digit-vec, a whole
  `JsonNode` / array / composite tree). A narrow row of small integers is several × its on-disk
  bytes; a document/array value is many ×.
- The buffer pool ([pager.md §3](pager.md)) bounds **page/leaf count**, but each resident leaf
  currently holds the **inflated decoded form**, so true resident bytes run well above
  `resident_leaves × page_size`. Under (a) a resident leaf is **≈ its page block** — so resident
  memory becomes `≈ pinned_pages × page_size`, the clean, predictable bound the `cache_bytes`
  budget already promises. This makes the budget *mean what it says* and is a real step toward the
  larger-than-RAM end state (a faulted leaf holds compact bytes, not expanded trees).

So the reshape is not only "decode less, clone less" but "**hold less**" — the dividend the narrow
S5 could never reach.

---

## 10. VDBE groundwork (alignment, not a goal)

[streaming.md §3](streaming.md) names a future bytecode VM and identifies its prerequisites. This
reshape supplies one more: **`OP_Column`-over-a-raw-record** — decode column *n* of a record on
demand from its bytes — is exactly what a VDBE needs and exactly what §6 builds. As with the pull
scan cursor (S2), this is *alignment*: the reshape stands on its own (decode skip + clone
elimination + memory), and a VDBE, if it ever lands, builds on it rather than against it. No scope
choice here forecloses or commits to one.

---

## 11. What does NOT change

- **The §8 byte contract** — on-disk format, key encoding, goldens, the cross-core round-trip.
  No `format_version` bump.
- **The cost contract** — the static touched set and every `# cost:` value ([cost.md §3](cost.md)).
- **B-tree navigation / split / merge** — keys are raw bytes; values going lazy does not touch them
  (§3).
- **Pure in-memory databases** — stay fully decoded via `from_image` (§4); the reshape is the
  file-backed demand-paged path, like the buffer pool and §14.
- **The large-value path** — §14 is the foundation this generalizes, not something it replaces; an
  external/compressed value is still resolved through the pager on touch.
- **Snapshot / watermark / mutation contracts** — composition only (§7).

---

## 12. Slicing (the mergeable steps)

Sequenced **seam-first** so the risky control-flow change lands alone on a frozen seam, each step
independently testable and cost/byte-neutral (so each core lands green independently — the P6.4 /
S3 / S4 precedent):

- **L0 — spec (this doc).** + the streaming.md §8 / pager.md §1 / CLAUDE.md §9 / TODO.md updates.
  *No code.*
- **L1 — the no-construct decode mode (no observable change).** Thread the construct/skip mode
  through `read_inline_body` and its callees (§6) and add `inline_body_span` (the start→cursor
  capture). The eager path stays **byte-identical** (the mode defaults to construct); the only new
  thing is the ability to advance past a body without building it, exercised by a unit test that
  asserts span-advance equals decode-advance over a rich row (every type incl.
  jsonb/composite/array/range/decimal). *Mergeable, no behavior change — the P6.4a "seam first"
  move.*
- **L2 — defer inline values at fault (form (b), the heart).** `read_value_lazy` produces a deferred
  `Unfetched::Inline` (owning its span, §5b) for variable-length / structured present values;
  fixed-width stays eager (§6). `resolve_columns` resolves them at the four touched-set read sites
  (reusing the §14 plumbing). The deep-tree clone and the eager decode of untouched columns are
  gone. Cost/results/goldens unchanged (§8); per-core unit tests: a faulted-leaf row resolves to the
  same values an eager decode produces; an untouched corrupt inline body does not surface its error,
  a touched one surfaces `XX001`; resident memory of a wide-untouched-column scan drops. Built
  Rust-first, then Go/TS.
- **L3 — zero-copy block-shared (form (a), the memory win).** Retain the leaf block and make a
  deferred value reference it (Arc slice / GC view, §5a); the fault stops copying and the scan-emit
  clone becomes a refcount bump. Per-core idiomatic and independently landable (Go/TS first, where
  it is nearly free; Rust upgrades (b)→(a)). Add a per-core test that resident leaf bytes track
  `≈ resident_leaves × page_size` (§9). *Optional but recommended — the dividend §9 exists for.*

Deferred follow-ons (none foreclosed): **keys as block slices** (zero-copy keys under (a));
**in-memory databases adopting deferral** (only if a Memory pager backing lands — pager.md §6, so
they page through the identical path); a **per-column offset cache** on a resident leaf (SQLite's
`OP_Column` offset memoization — skip re-walking earlier columns on repeated access of the same
faulted leaf).

---

## 13. Determinism & cross-core notes (summary)

- **Results + cost are the only contract**, and both are invariant (§8); the resident representation
  ((a) vs (b)), the no-construct decode mode, and the deferral are internal machinery, **not** a
  byte contract — each core implements them idiomatically (the pager / spill / concurrency
  precedent), and a core may pick (a) or (b) independently.
- **No format change, no new cost unit** — decode timing and byte location are invisible to the
  on-disk bytes and the static touched-set cost.
- **No nondeterminism leaks** — deferral keys on column position + the static mask (both
  deterministic), never on iteration order or timing; a touched value decodes to the byte-identical
  result the eager path produced.
- **Memory safety holds** — the no-construct walk is owned-cursor / sliced-buffer traversal in every
  core (no `unsafe`, no cgo; CLAUDE.md §2/§13); under (a) the shared block is an `Arc`/GC view, not a
  raw pointer.
