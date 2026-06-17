# On-disk file format — byte spec

> The single-file on-disk format (CLAUDE.md §9), specified to the byte. The storage
> *model* (block seam, page model, root-swap commit) is in
> [../design/storage.md](../design/storage.md); **this** doc fixes the concrete bytes that
> realize it, with byte-exact golden fixtures in [fixtures/](fixtures/). When a decision
> here changes, update [../design/storage.md](../design/storage.md) and
> [CLAUDE.md](../../CLAUDE.md) §9 in the same change.

The load-bearing conformance test (CLAUDE.md §8): a file written by one core must be
byte-readable by another. Because this format is **fully deterministic**, that is realized
as golden-file tests — each core must (a) read a checked-in golden into the expected state,
and (b) write the same logical database to bytes that equal the golden *exactly*. Then
`rust-bytes == golden == go-bytes == ts-bytes` by construction, so each core reads the
other's output. A fourth independent encoder/decoder (the Ruby reference in
[verify.rb](verify.rb)) pins the goldens so they are not merely self-certified.

## Version scope (`format_version` 10)

The current on-disk version is **`format_version` 10** — **array (`T[]`) columns**
([../design/array.md](../design/array.md)). An array is a **structural** type (no catalog object —
the element type is carried inline), so the only on-disk change is in the **column entry**: a new
**`type_code = 15`** (the *Stable type codes* table) followed by the **element-type descriptor**
(the element's own type code, then — for a composite element — its name; *Array column* below) in
place of a scalar's typmod slot. A value is the compact **array body** (`ndim ‖ flags ‖ per-dim
(len, lb) ‖ optional null bitmap ‖ element bodies` — *Value codec* below); a fixed-width element
carries **no per-element length prefix**. The per-table data B-tree and the meta page are
untouched. A file with no array columns still moves to v10 (only the version byte + meta CRC
change). Each version is a **clean break** — older versions are **not read** (we are pre-1.0 and
owe no on-disk compatibility; CLAUDE.md §1, "we own our surface"), so a reader accepts **only**
version 10.

`format_version` 9 was **composite (row) types**
([../design/composite.md](../design/composite.md)). A user-defined composite type
(`CREATE TYPE addr AS (street text, zip int32)`) is a database-level object, so the catalog —
through v8 a chain of **table** entries — became a chain of **kind-tagged** entries. Three
coupled changes, all in the catalog (the per-table data B-tree and the meta page untouched):

1. **Every catalog entry gains a leading `entry_kind` u8**: `0` = a table entry (the v8 layout,
   unchanged after this byte), `1` = a composite-type entry. Composite-type entries are emitted
   **first** (ascending lowercased-name order), then table entries (ascending lowercased-name
   order); `item_count` counts all entries, packed greedily exactly as before. The catalog stays
   a uniform "sequence of entries" — no special head page, no separate page chain.
2. **A composite-type entry** carries the type's name and ordered field list (the *Composite-type
   entry* table below). A composite type used as a column type is referenced **by name**, with a
   new **`type_code = 14`** (the *Stable type codes* table) followed by the type name in the
   column entry's typmod slot.
3. **Load is two-pass**: collect every composite-type entry into a name→definition map, validate
   that every referenced composite name exists and the reference graph is **acyclic** (a dangling
   or cyclic reference is `XX001`), then build the tables (resolving each composite column's name).

A file with no composite types still moved to v9 (the version byte changed, and every table entry
gained the leading `entry_kind = 0`).

`format_version` 8 was an **expression column default**.
A column's `DEFAULT` may be a non-constant expression (a function call like `uuidv7()`,
arithmetic like `1 + 1`) rather than only a constant literal
([../design/constraints.md §2](../design/constraints.md)). The per-column flags byte gained
**bit3 `default_is_expr`**: when set, the default's **expression text** — a length-prefixed
UTF-8 string, the parsed token sequence re-rendered by the same closed token table a `CHECK`
uses (*Check-expression text* below) — is written after the typmod **in place of** the
value-codec default that `bit2 has_default` writes. The two bits are **mutually exclusive**.
On load the text re-parses with the ordinary expression parser (`XX001` if it fails, like a
stored check); the write paths evaluate it per row. A constant-literal default still takes the
`bit2` value-codec path unchanged.

`format_version` 7 was the **per-page checksum on every body page** (catalog / B-tree node /
overflow), the on-disk-integrity layer that lets the loader **detect silent corruption** of a
live page rather than returning wrong results or panicking
([../design/storage.md §6](../design/storage.md)). Two coupled changes:

1. The **page header grows from 12 to 16 bytes** — a `crc32` (u32) is appended after
   `next_page` (*Page header* below). Through v6 only the meta slots were checksummed; a
   bit flip in any other page went undetected. Now **every** body page carries a
   CRC-32/IEEE over its own bytes, verified the instant the page is parsed (`XX001` on
   mismatch) — including the open-time reachability walk, so corruption of a catalog page,
   an interior node, or an overflow chain is caught at **open**, and a leaf the moment it
   faults in.
2. Because the header is 4 bytes wider, the page payload `C = page_size − 16` shrinks by 4
   and the byte layout of every multi-record page shifts (`RECORD_MAX` falls from 116 to
   114 at the 256-byte fixture size). The **`− 12` inside `RECORD_MAX`** is *unchanged* — it
   reserves three interior child pointers (`4·3`), which is independent of the header width
   (it merely coincided with the old 12-byte header). The meta page (its own 36-byte layout
   and CRC over `[0, 32)`) is **untouched** except for the `format_version` value.

`format_version` 6 was the per-index **flags byte** carrying the `unique` bit
([../design/indexes.md §8](../design/indexes.md),
[../design/constraints.md §5](../design/constraints.md)): each catalog index entry gained
an `index_flags` u8 between its key ordinals and its root page — bit0 `unique`, the rest
reserved (written 0, read-validated).

`format_version` 5 was the **secondary-index catalog reshape**
([../design/indexes.md](../design/indexes.md)). Three changes:

1. The catalog table entry records the **primary key as an explicit ordinal list in key
   order** (`pk_count` + ordinals — *Catalog* below). Column-flag **bit0 is retired**
   (reserved, written 0): the list is the single authority, and an order independent of
   declaration order is now expressible — which lifted the composite-PK order narrowing
   ([../design/constraints.md](../design/constraints.md) §3).
2. The catalog table entry gains the table's **index list** (name + key-column ordinals +
   root page, in ascending lowercased-name order).
3. Each index is an on-disk **B-tree of empty-payload records** — the same node pages,
   split/merge rules, and commit model as a table tree; only the record's value-column
   count (zero) differs (*The per-table data B-tree* below).

`format_version` 4 added **`CHECK` constraints to the catalog table entry** (a per-table
list of `(name, expression-text)` pairs after the column entries — *Catalog* below;
[../design/constraints.md](../design/constraints.md) §4); a catalog-only change.

`format_version` 3 was the **page-backed copy-on-write B-tree** format (Phase 6) plus
**out-of-line overflow pages** and **transparent LZ4 compression** for large values (the
*Large values* section below; [../design/large-values.md](../design/large-values.md)).
Compression (large-values.md Slice B) landed **additively within v3**: the `0x03`/`0x04`
forms were reserved by the overflow slice, so no second version bump.

The P6.1 page-backed B-tree (`format_version` 2) changed exactly two things from the step-5b
whole-image format (`format_version` 1):

1. **Each table's rows live in a per-table on-disk B-tree** (interior + leaf node pages),
   not a flat record chain. The B-tree's node layout and its **size-driven split/merge
   rules** are now a §8 **byte contract** (they were a private in-RAM detail through Phase 5
   — transactions.md §3). Fan-out is governed by **page fit**, not a key count, so a node
   fills its page (the SSD / TB-scale goal — storage.md §1).
2. **Commit is incremental copy-on-write.** A commit writes only the **dirty** pages a
   mutation introduced (the path the copy-on-write B-tree copied, plus the rewritten catalog)
   to fresh appended slots, then publishes the new root by writing the **alternate meta slot**
   — not a whole-image rewrite. The meta page gains a **relocatable** catalog-root pointer and
   real **slot alternation** (storage.md §4).

Through v2 the stable type codes, the catalog table-entry encoding, the CRC, and the
order-preserving keys stayed byte-identical to v1, and the **value codec** did too. **v3 extends the
value codec** — and only it — with the external and compressed value states (three new presence-tag
values + a per-row overflow chain; *Value codec* / *Large values* below); every **inline-plain** and
**NULL** value is still byte-unchanged, and the type codes / catalog / CRC / keys are untouched.

**Reclamation (P6.2)** — the allocator reuses dead pages from a free-list **reconstructed on open**
(see *Reclamation* below), so a file no longer grows without bound across its lifetime. The
reachability walk also collects each live record's **overflow chain** (v3), so spilled-value pages
are never handed out as free; a dead chain (from an updated/deleted row) leaks until the next open,
matching the B-tree-orphan model. **Still deferred, not foreclosed**: continuous *within-session*
reclamation + on-disk free-list persistence (the P6.2 follow-ons).

## Conventions

- **All multi-byte integers are big-endian, unsigned** unless stated, consistent with the
  key encoding's MSB-first rule ([encoding.md](../design/encoding.md)).
- **Reserved fields are written as zero and required to be zero on read.** A nonzero
  reserved field, bad magic, unsupported version, or bad checksum is a structured
  `data_corrupted` (SQLSTATE `XX001`) error.
- A page index of **0** means "none"/absent (an absent child pointer, an empty table's root,
  an end-of-chain `next_page`). Pages 0 and 1 are the meta slots, so 0 is an unambiguous
  sentinel — no real body page ever lives at index 0 or 1.
- **Page roles are not positional** (unlike v1). Page 0/1 are the meta slots; every other
  page is a body page (a catalog-chain page or a B-tree node) whose role is discovered only
  by following pointers from the meta — never by its index. On-disk, body pages appear in
  **allocation order** (see *Allocation & incremental commit* below).

## Page model

The file is a flat array of fixed-size **pages**; the page size is a format parameter
recorded in the meta page (**default 8192**; the golden fixtures use **256** so the hex stays
reviewable). It must be a **power of two** in **`[256, 65536]`** — i.e. one of the nine values
`{256, 512, 1024, 2048, 4096, 8192, 16384, 32768, 65536}`. `MIN_PAGE_SIZE = 256` is the floor
(comfortably above the structural minimum `PAGE_HEADER + 36 = 52`, below which the 36-byte meta
header would not fit) and `MAX_PAGE_SIZE = 65536` (64 KiB) the ceiling. A core **rejects** any
other page size — `0A000` when serializing (`create`), `XX001` when reading a file's meta
(`open`). The **power-of-two** requirement keeps every page boundary aligned to the device's
logical/physical sector (the SSD target, CLAUDE.md §9) — a non-power-of-two page straddles sector
boundaries and forces read-modify-write amplification — and collapses the legal set to nine
values, shrinking the cross-core test matrix; it also matches SQLite (the identical rule) and
PostgreSQL (`BLCKSZ` is a compile-time power of two). The **maximum** bounds the largest single
page allocation: without it a corrupt or hostile file could record a multi-gigabyte `page_size`
and force that allocation before any content is validated (the untrusted-input concern, CLAUDE.md
§13). `page_count = file_size / page_size`. Every page is zero-filled to exactly `page_size`. Two
page-payload capacities derive from the page size and recur throughout:

```
C          = page_size - 16       # PAGE_HEADER (v7); the bytes a page body may hold
RECORD_MAX = (C - 12) / 2 (floor) # the largest a single B-tree record may serialize to
```

The `- 16` in `C` is the **v7 page header** (12-byte v6 header + a 4-byte per-page `crc32`).
The `- 12` inside `RECORD_MAX` is a **separate** quantity — room for a **two-key interior**
node's three child pointers (`4·3`) — and is unchanged across versions (through v6 it
coincided with the header width; v7 widened the header but not this reserve). It makes
`2·RECORD_MAX + 12 ≤ C`, so a two-record node — leaf *or* interior — **never** overflows.
Overflow therefore happens only at `N ≥ 3` keys, which is what lets every split be a clean
2-way with two non-empty halves (see *Why the record cap* below).

| page index | role |
|---|---|
| 0 | meta slot 0 |
| 1 | meta slot 1 |
| ≥ 2 | body pages: catalog-chain pages and per-table B-tree nodes, allocated dynamically |

## Meta page (pages 0 and 1)

Two slots for torn-write-safe atomic publish (the bbolt model — storage.md §4). Fields
(layout unchanged from v1 except `format_version` and the now-active meaning of `root_page`
and slot selection):

| offset | size | field |
|---|---|---|
| 0  | 4 | `magic` = `4A 45 44 42` (ASCII `JEDB`, for the engine `jed`) |
| 4  | 2 | `format_version` (u16) — current = **`10`** |
| 6  | 2 | reserved (0) |
| 8  | 4 | `page_size` (u32) |
| 12 | 8 | `txid` (u64) — commit counter; the highest valid slot wins on open |
| 20 | 4 | `root_page` (u32) — the **catalog chain head** (relocatable; ≥ 2) |
| 24 | 4 | `page_count` (u32) — total pages in the file |
| 28 | 4 | reserved (0) — an on-disk free-list head may claim this later; **still written `0`** (P6.2 reconstructs the free-list on open rather than persisting it — see *Allocation & incremental commit*) |
| 32 | 4 | `crc32` (u32) — CRC-32/IEEE over meta bytes `[0, 32)` (excludes this field and the zero-fill tail) |

`page_size` lives at a fixed offset so a reader can learn it before it knows where page 1
begins (page 1 starts at byte `page_size`).

**Checksum.** CRC-32/IEEE (reflected, polynomial `0xEDB88320`, init `0xFFFFFFFF`, final XOR
`0xFFFFFFFF`) — the standard zlib CRC32, hand-rolled identically in every core (no runtime
dependency). Pinned by the vector `crc32("123456789") == 0xCBF43926`.

**`root_page` is relocatable.** In v1 the catalog root was fixed at page 2; in v2 the catalog
chain is rewritten to fresh pages on every commit (it carries each table's B-tree root, which
moves under copy-on-write — see below), so `root_page` is wherever the latest catalog head
landed. A reader **must** follow `root_page`; it may not assume `2`.

**Slot alternation (writing).** A commit writes its meta to slot **`txid & 1`** (even `txid`
→ slot 0, odd → slot 1). Because consecutive `txid`s alternate slots, a commit overwrites
only the **older** slot, leaving the previously-published meta intact throughout the write —
so a torn meta write always falls back to a complete prior snapshot whose body pages are still
present (copy-on-write never overwrote them). `create` seeds **both** slots with the initial
`txid = 1` meta, so two valid slots exist from the first moment (the first even-`txid` commit
then overwrites slot 0).

**Opening (slot selection).** Validate each slot independently (magic, `format_version == 10`,
reserved == 0, `crc32`). Choose the **valid** slot with the **highest `txid`**; on a tie,
slot 0. Exactly one valid → use it (torn-write fallback). Neither valid → `data_corrupted`.

## Page header (catalog and B-tree pages, 16 bytes — v7)

| offset | size | field |
|---|---|---|
| 0 | 1 | `page_type` (u8) — `1` = catalog, `2` = B-tree **leaf**, `3` = B-tree **interior**, `4` = overflow |
| 1 | 1 | reserved (0) |
| 2 | 2 | reserved (0) |
| 4 | 4 | `item_count` (u32) — entries (catalog) / keys `N` (B-tree node) on this page |
| 8 | 4 | `next_page` (u32) — **catalog / overflow only**: next page of the chain, or 0. B-tree nodes write `0` here (a node is reached by a child pointer, not a chain). |
| 12 | 4 | `crc32` (u32) — **new in v7**: CRC-32/IEEE over the page bytes *excluding this field* — i.e. `[0, 12)` then `[16, page_size)`, covering the header, the payload, and the zero-fill tail |

The payload follows at offset **16** and is zero-filled to `page_size`.

**Per-page checksum (v7).** Every body page (catalog `1`, leaf `2`, interior `3`, overflow
`4`) carries a `crc32` over all its own bytes except the 4-byte field itself. It uses the
**same CRC-32/IEEE** routine and polynomial as the meta slot (below). A reader computes the
checksum the instant it parses a page and rejects a mismatch as `data_corrupted` (`XX001`).
Because *every* page read funnels through one parse — including the demand-paged leaf fault
and the open-time free-list reachability walk (which follows catalog and overflow chains by
header) — a single-bit flip in any live page is **detected**, not silently served. The
checksum is part of physical page I/O and is **not** a metered cost unit (it is invisible to
the deterministic `page_read` cost, like the buffer pool — [../design/cost.md](../design/cost.md),
CLAUDE.md §13). The zero-fill tail is covered too: a committed page's tail is always zero, so
the CRC is a deterministic function of the page's logical content (a §8 byte contract).

## Catalog (relocatable page chain rooted at `root_page`)

The catalog is a chain of `page_type = 1` pages, **rewritten to fresh pages on every commit**
(transactions.md §4.5 requires the catalog be copied-on-write too, because each table's
B-tree root moves). Its **encoding is byte-identical to v1**; only its location is dynamic
(`root_page`) and `root_data_page` now points at a **B-tree root node** instead of a record
chain head.

**Each catalog entry is kind-tagged (v9):** a leading `entry_kind` u8 — `0` = a table entry, `1`
= a composite-type entry ([../design/composite.md §3](../design/composite.md)). **Composite-type
entries are emitted first** (ascending lowercased-name order), then table entries (ascending
lowercased-name order). Each page's `item_count` is the number of entries (of either kind) it
holds; entries are packed greedily into the chain, kind-tagged in stream order, exactly as table
entries were through v8 (a single entry must fit one page, i.e. ≤ `C`, else `0A000`; the
`RECORD_MAX = C/2` cap is a B-tree-record rule and does **not** apply to catalog entries, which
never split). Tables are emitted in **ascending order of the lowercased table name** (the engine
stores tables in a hash map keyed by lowercased name; sorting by that key removes any
iteration-order leak — CLAUDE.md §8; names are unique after lowercasing, so there are no ties);
composite types likewise.

**Load is two-pass (v9):** the reader walks the whole chain collecting every composite-type entry
into a name→definition map, validates that every composite **referenced** by a column or a field
exists and that the reference graph is **acyclic** (a dangling or cyclic reference is `XX001`),
then builds the tables — resolving each composite column's type name against the map. Because of
nested composites a single pass cannot guarantee a referenced type is already read (name order
does not imply dependency order), hence the two passes.

Each **table entry** (after its `entry_kind = 0`; v5 adds the primary-key ordinal list after the
columns and the index list after the checks, and retires column-flag bit0):

| field | encoding |
|---|---|
| `name_len` | u16 |
| `name` | `name_len` bytes UTF-8 (original case — round-trips what the user typed) |
| `col_count` | u16 |
| per column (×`col_count`): | |
| &nbsp;&nbsp;`col_name_len` | u16 |
| &nbsp;&nbsp;`col_name` | UTF-8 (original case) |
| &nbsp;&nbsp;`type_code` | u8 (stable, see below) |
| &nbsp;&nbsp;`flags` | u8 — bit0 reserved 0 (**was** `primary_key` through v4 — the `pk` list below is the authority), bit1 `not_null`, bit2 `has_default` (constant default), bit3 `default_is_expr` (**new in v8** — expression default; mutually exclusive with bit2, both set is `XX001`) (reader trusts the bits) |
| &nbsp;&nbsp;`precision` | u16 — **only present when `type_code == 6` (decimal)**; `0` = unconstrained |
| &nbsp;&nbsp;`scale` | u16 — **only present when `type_code == 6` (decimal)** |
| &nbsp;&nbsp;`default` | value-codec bytes — **only present when `flags` bit2 (`has_default`)**; written *after* the typmod |
| &nbsp;&nbsp;`default_expr_len` | u16 — **only present when `flags` bit3 (`default_is_expr`)**; written *after* the typmod (in place of `default`) |
| &nbsp;&nbsp;`default_expr` | UTF-8 — the default's expression text (*Check-expression text* below), `default_expr_len` bytes |
| `pk_count` | u16 — primary-key member count (**new in v5**; `0` = no PK, synthetic rowid keys) |
| `pk_ordinal` ×`pk_count` | u16 each — column ordinals (0-based declaration position) in **key order**; each must be `< col_count` and distinct (else `XX001`) |
| `check_count` | u16 — the table's `CHECK` constraints (v4; `0` for an unchecked table) |
| per check (×`check_count`): | |
| &nbsp;&nbsp;`check_name_len` | u16 |
| &nbsp;&nbsp;`check_name` | UTF-8 (original case) |
| &nbsp;&nbsp;`check_expr_len` | u16 |
| &nbsp;&nbsp;`check_expr` | UTF-8 — the expression text (*Check-expression text* below) |
| `index_count` | u16 — the table's secondary indexes (**new in v5**; `0` for an unindexed table) |
| per index (×`index_count`): | |
| &nbsp;&nbsp;`index_name_len` | u16 |
| &nbsp;&nbsp;`index_name` | UTF-8 (original case) |
| &nbsp;&nbsp;`key_col_count` | u16 — ≥ 1; per index key column: |
| &nbsp;&nbsp;`key_ordinal` ×`key_col_count` | u16 each — column ordinals in **index-key order**; each must be `< col_count` (duplicates allowed — indexes.md §1; else `XX001`) |
| &nbsp;&nbsp;`index_flags` | u8 — bit0 `unique` (**new in v6** — indexes.md §8); bits 1–7 reserved, written 0 (a set reserved bit is `XX001`) |
| &nbsp;&nbsp;`index_root_page` | u32 — the root B-tree node of this index, or 0 if the table has no rows |
| `root_data_page` | u32 — the **root B-tree node** of this table, or 0 if it has no rows |

Columns are emitted in declaration order. Checks are emitted in their **evaluation order** —
ascending byte order of the lowercased `check_name` ([../design/constraints.md
§4.4](../design/constraints.md)); a reader trusts that order (it never re-sorts). Indexes
are emitted in **ascending byte order of the lowercased `index_name`** (the catalog's
in-memory order and the planner's tie-break order — [../design/indexes.md
§5/§6](../design/indexes.md)); a reader trusts that order too.

A **composite column** (a column whose type is a user-defined composite — `type_code == 14`)
appends, in the column entry's typmod slot (where a decimal appends precision/scale), a
`u16 type_name_len` then that many UTF-8 bytes naming the composite type. The named type must
appear in this catalog's composite-type entries (else `XX001`). A composite column is **not** a
key this slice — a composite `PRIMARY KEY` / index / `UNIQUE` column is rejected `0A000` at DDL
([../design/composite.md §6](../design/composite.md)), so no composite key bytes ever reach a
data record.

An **array column** (a structural `T[]` type — `type_code == 15`, **v10**;
[../design/array.md §3](../design/array.md)) appends, in the typmod slot, an **element type
descriptor**: a `u8 element_type_code` (the *Stable type codes* table — a scalar `1`–`13`, or `14`
+ a `u16 name_len` + name for a composite element; a nested-array element `15` is not a jed type).
The element type is carried **inline** (no array-type catalog object — arrays are structural, not
nominal), so an array column is self-describing. An array column is **not** a key this slice — an
array `PRIMARY KEY` / index / `UNIQUE` is rejected `0A000` at DDL ([../design/array.md §8](../design/array.md)).

### Composite-type entry (`entry_kind = 1`, v9)

A composite-type entry records a `CREATE TYPE name AS (field type, …)` definition
([../design/composite.md](../design/composite.md)):

| field | encoding |
|---|---|
| `entry_kind` | u8 = `1` |
| `name_len` | u16 |
| `name` | `name_len` bytes UTF-8 (original case) |
| `field_count` | u16 — ≥ 1 |
| per field (×`field_count`): | |
| &nbsp;&nbsp;`field_name_len` | u16 |
| &nbsp;&nbsp;`field_name` | UTF-8 (original case) |
| &nbsp;&nbsp;`field_type_code` | u8 (the *Stable type codes* table; `14` = a nested composite, `15` = an array-typed field) |
| &nbsp;&nbsp;`field_type_name_len` | u16 — **only when `field_type_code == 14`** |
| &nbsp;&nbsp;`field_type_name` | UTF-8 — **only when `field_type_code == 14`**: the referenced composite type's name |
| &nbsp;&nbsp;`field_element_descriptor` | **only when `field_type_code == 15`**: the array element-type descriptor (a `u8 element_type_code`, then `14` + `u16 name_len` + name for a composite element) — the same descriptor an array *column* uses (the *Each catalog entry* table), one level down |
| &nbsp;&nbsp;`field_flags` | u8 — bit0 `not_null` (declared `NOT NULL`); bits 1–7 reserved, written 0 (a set reserved bit is `XX001`) |
| &nbsp;&nbsp;`precision` | u16 — **only when `field_type_code == 6` (decimal)**; `0` = unconstrained |
| &nbsp;&nbsp;`scale` | u16 — **only when `field_type_code == 6` (decimal)** |

Fields are emitted in **declaration order** (the order they appear in `CREATE TYPE`). A field
type code of `14` references another composite **by name** (nested composites); a field type code
of `15` is an **array-typed field** (`xs int32[]`, [../design/array.md §12](../design/array.md) —
the mirror of an array-of-composite element), carrying the inline element descriptor **before** the
flags byte (where a nested-composite name sits) so the element type is self-describing. The
loader's two-pass validation rejects a dangling reference or a definition cycle — including a
composite reached **through an array field** — as `XX001` (a v10 additive extension; no
`format_version` bump, since an array element descriptor is already a v10 shape).

### Check-expression text

The persisted `check_expr` is the constraint's parsed **token sequence re-rendered** — the
tokens between the `CHECK` parentheses, each rendered by the closed table below, joined with
single spaces (`0x20`). It is a recursion-free byte contract: every core renders the same
token stream to the same bytes, and a loader re-parses the text with its ordinary expression
parser (re-lexing yields a value-identical token sequence by construction). A commit writes
the retained text back **verbatim**, so the bytes are stable across create → commit → load →
commit. Text that fails to lex/parse in an otherwise-valid file is `XX001` (`data_corrupted`)
at open.

| token | rendering |
|---|---|
| word (keyword / identifier) | as written (original case; comparisons are case-insensitive at parse) |
| integer literal | the unsigned decimal digits of its magnitude, no sign, no leading zeros |
| decimal literal `(coeff, scale)` | the digit string `coeff` with `.` inserted `scale` digits from the right — `("150", 2)` → `1.50`, `("5", 1)` → `.5`, `("1", 0)` → `1.` (always contains the `.`, so it re-lexes as a decimal) |
| string literal | `'` + content with each `'` doubled + `'` |
| bind parameter | `$N` (unreachable in a *stored* check — rejected at CREATE TABLE, 42P02) |
| punctuation / operators | their fixed spelling: `,` `.` `(` `)` `*` `+` `-` `/` `%` `=` `<` `>` `<=` `>=` |

Example: `CHECK (a>0 AND b IS NOT NULL)` persists as `a > 0 AND b IS NOT NULL`; `CHECK
(price * qty <= 10000.00)` persists as `price * qty <= 10000.00`.

**Composite primary key.** A composite `PRIMARY KEY` ([../design/constraints.md
§3](../design/constraints.md)) is persisted as the `pk_count`/`pk_ordinal` list in **key
order** (v5 — through v4 it was `bit0` on each member column, which encoded the member *set*
but no independent order; the list is what lifted the `0A000` list-order narrowing). A stored
record's `key` is the concatenation of the members' encodings in that order
([../design/encoding.md §2.3](../design/encoding.md)).

### Stable type codes

Independent of any in-memory enum discriminant (which may be reordered):

| `type_code` | type |
|---|---|
| 0 | reserved |
| 1 | `int16` |
| 2 | `int32` |
| 3 | `int64` |
| 4 | `text` |
| 5 | `boolean` |
| 6 | `decimal` |
| 7 | `bytea` |
| 8 | `uuid` |
| 9 | `timestamp` |
| 10 | `timestamptz` |
| 11 | `interval` |
| 12 | `float64` |
| 13 | `float32` |
| 14 | composite (a user-defined row type — followed by the type name, **not** a fixed body; v9) |
| 15 | array (a structural `T[]` — followed by the element type descriptor, **not** a fixed body; v10) |

A **`float64`** value (`type_code == 12`) is the **8 IEEE 754 bytes, big-endian**, and a
**`float32`** value (`type_code == 13`) is the **4 IEEE 754 bytes, big-endian** — both behind the
presence tag, fixed-width, no length prefix, the `int64`/`uuid`/`timestamp` shape. The stored bits
are preserved verbatim for every value **except `NaN`**: a stored `-0.0` keeps its sign bit and
`±Infinity`/finite values keep theirs, but a `NaN` is **canonicalized to the single quiet pattern**
`0x7FF8000000000000` (`float64`) / `0x7FC00000` (`float32`) on write. A NaN's payload bits are
core-specific (Go's `math.NaN()` is `0x7FF8…001`, hardware `Inf − Inf` is the negative `0xFFF8…`),
so this NaN-only step is what keeps a stored NaN byte-identical across cores; everything else is
verbatim (the `-0 = +0` collapse is a comparison/key concern only — [../design/float.md](../design/float.md)
§3/§10). The on-disk bytes are byte-identical across cores (the float types are exempt from
cross-core identity only for *computed/rendered values*, not for *storage* — determinism.md §6).

A column's collation is **not** stored: there is one collation (`C`) for all text this slice
([../design/types.md](../design/types.md) §11). A per-column collation field is a forward
extension that will claim a spare `flags` bit or a new field under a `format_version` bump when
multi-collation lands. `bytea` and `uuid` have no collation. A non-integer PRIMARY KEY needs no
extra catalog field — the key bytes live in the data-page record (below), not the catalog.

A **decimal** column carries a **typmod** (the `numeric(p,s)` precision/scale) appended to the
column entry **only when `type_code == 6`** — two big-endian `u16`s, `precision` then `scale`.
`precision == 0` means **unconstrained** `numeric` (`scale` then `0` and ignored); a
constrained `numeric(p,s)` stores `precision = p` (`1 … 1000`) and `scale = s`.

A column with a **`DEFAULT`** ([../design/constraints.md](../design/constraints.md) §2)
persists in one of two presence-gated forms, after the typmod (so a column without a default is
byte-unchanged). A **constant** default persists its pre-evaluated value when `flags` **bit2**
(`has_default`) is set, via the **same value codec rows use** (presence tag + body): a present
default is `0x00` + the type body, a `DEFAULT NULL` is the lone `0x01`. An **expression** default
(a non-constant `DEFAULT`, e.g. `uuidv7()` or `1 + 1`) persists its **expression text** when
`flags` **bit3** (`default_is_expr`, **new in v8**) is set: a `u16` length then that many UTF-8
bytes, the parsed token sequence re-rendered by the closed token table in *Check-expression text*
below — identical to how a `CHECK` persists, and re-parsed on load (`XX001` if it fails). bit2 and
bit3 are **mutually exclusive** (both set is `XX001`).

## The per-table data B-tree

Each non-empty table's rows are an **ordered B-tree** keyed by the row's encoded storage key
(memcmp order — [encoding.md](../design/encoding.md)), rooted at the table's `root_data_page`.
It is the on-disk image of the in-memory copy-on-write B-tree (transactions.md §3, pmap):
node ↔ page, one-to-one. A node holds keys **and** their row values at *every* level (a
CLRS-style B-tree, not a B+-tree — values are not pushed to the leaves), plus child pointers
when interior. The same record encoding serves leaves and interior separators.

### Record (a key and its row), unchanged from v1

| field | encoding |
|---|---|
| `key_len` | u16 |
| `key` | `key_len` bytes — the row's storage key, exactly as the engine encoded it |
| `payload` | each column's value, in declaration order, via the value codec |

The key is **stored, not derived** (a no-PK synthetic rowid is not reconstructable from row
data). There is no per-record payload length — the reader walks the columns in declaration
order, taking each value's width from its type (fixed for integers / 8-byte timestamps /
1-byte boolean / 16-byte uuid; self-describing for text/bytea via their `u16` length and for
decimal via `ndigits`). The **on-disk size of a record** — `2 + key_len + Σ value_size` — is
the quantity the split/merge rules below measure.

**Index trees (v5).** A secondary index ([../design/indexes.md](../design/indexes.md)) is a
B-tree of exactly this shape, rooted at the catalog's `index_root_page`, whose records have
**zero value columns**: a record is `key_len ‖ key` (the entry key — indexes.md §3) with an
empty payload, `record size = 2 + key_len`. Every node, split/merge, allocation, commit, and
reclamation rule below applies to an index tree unchanged.

**`RECORD_MAX`.** A single record's **stored** on-disk size must be ≤ `RECORD_MAX = (C-12)/2`
(floor); this is **tighter than v1's ≤ `C`** rule and is what makes every node split clean (see
*Why the record cap* below). Since **v3**, a record over the cap is **not** rejected — its large
values are **compressed** and/or **spill out-of-line** (the *Large values* section), so the
*stored* record (with pointers) falls back under `RECORD_MAX`. Only a record that can't be reduced
below the cap even after compressing and externalizing every spillable value remains a write-side
`feature_not_supported` (**`0A000`**). At the 8192 default, `RECORD_MAX = 4082` (v7); the 256-byte
fixtures cap a stored record at 114 bytes, which is what makes `overflow_table.jed`'s ~600/300-byte
values spill.

### Leaf node (`page_type = 2`)

Header (`item_count = N`, `next_page = 0`) followed by the payload: **`N` records** in
**ascending key order**, packed contiguously. A leaf's payload size is `Σ record_size`. This
payload is byte-identical to a v1 data page's payload — only the page's *role* (a tree node
vs. a chain link) differs.

### Interior node (`page_type = 3`)

Header (`item_count = N`, `next_page = 0`) followed by the payload, in order:

1. **`N + 1` child pointers** — each a big-endian `u32` page index, the roots of the `N + 1`
   subtrees (`child[0] < key[0] < child[1] < key[1] < … < key[N-1] < child[N]`).
2. **`N` records** — the separators, in ascending key order, each carrying its own row value
   (this B-tree stores a value with every key, separators included).

An interior node's payload size is `4·(N + 1) + Σ record_size`. To parse: read `N` from the
header, read `N + 1` child pointers, then read `N` records.

An empty table has `root_data_page = 0` and no node pages. A one-row table is a single leaf
with `N = 1`. The root may be a leaf (small table) or an interior node (taller tree); it is
distinguished by its `page_type`, never by its index.

### Fan-out: the size-driven split/merge byte contract

Fan-out is governed by **page fit**: a node may hold any number of keys whose serialized form
fits the page payload `C`, and it splits when it would overflow. This makes the node
boundaries — and therefore the on-disk bytes — a deterministic function of the **key set and
the order of mutations** (not of any in-RAM tuning constant). Every core and the Ruby
reference run the identical rules, so the trees are byte-identical. The rules:

**A node "fits"** iff its payload size ≤ `C`. The invariant the writer maintains: every
committed node fits, every committed node is **non-empty** (`N ≥ 1`), and every non-root node
is **at least half full** where it can be (`payload ≥ C/2`) — "where it can be" because a row
near `RECORD_MAX` may force an underfull node, which is correct, just not compact.

**Insert.** Descend to the target leaf, insert the record in key order, then walk back up:
whenever a node overflows (`payload > C`), **split it 2-way** and **promote one separator**
to the parent (which may then overflow and split, etc.; a root split grows the height by one).

**Split point.** For an overflowing node with records `r[0 … N)` (and, if interior, children
`c[0 … N]`), define the left-payload after taking the first `m` records:

```
leftpayload(m) = (interior ? 4·(m + 1) : 0) + Σ_{i < m} record_size(i)
m_append       = the largest  m in [1, N-1] with leftpayload(m) ≤ C
m_balanced     = the smallest m in [1, N-1] with 2·leftpayload(m) ≥ payload
```

The split point depends on **where the just-edited record sits** — the record whose
insert/replace triggered this rebuild (for an interior node: the separator the child split
just promoted into it):

- **Right-edge (append) split** — the edited record is the node's **last** (`index N-1`):
  `m = min(m_append, N-2)`. Sequential ascending loads land here every time and keep
  packing left nodes ~full.
- **Balanced split** — anywhere else: `m = min(m_balanced, m_append, N-2)` (the `m_append`
  term keeps the left half fitting `C` even with near-`RECORD_MAX` records).
- The delete path's **merge-overflow** split (below) always uses the **balanced** rule —
  no edited position exists there.

In both cases `m` is clamped to at least `1`. Promote `r[m]`; the **left** node gets
`r[0 … m)` (and `c[0 … m]`), the **right** node gets `r[m+1 … N)` (and `c[m+1 … N]`). The
`RECORD_MAX = C/2` cap guarantees either `m` yields two **non-empty** nodes that each
**fit** (proof under *Why C/2*; for the balanced case additionally:
`leftpayload(m) ≤ leftpayload(m_append) ≤ C` by the `min`, and the right half is
`≤ payload − leftpayload(m_balanced) ≤ payload/2 ≤ C` under the same `payload ≤ 2C` bound).

> Why two rules: largest-left-fit alone is optimal for ascending appends but degenerates to
> a `[N-2 | 1]` splinter for **any other** insert position — under random-order inserts
> (secondary-index maintenance, a future random pk source) leaves converge on a few-percent
> fill (the 2026-06 benchmark finding, [../design/benchmarks.md](../design/benchmarks.md)).
> The position hint keeps the ascending fast path byte-for-byte unchanged while random
> inserts settle at the classic ~66-70% B-tree fill.

**Delete.** Descend to the key (replacing an interior separator by its **in-order
predecessor** — the rightmost record of the left subtree — and deleting that from the left
child), remove the record, then walk back up rebalancing by **merge-then-maybe-split** (no
borrow rotation — merge subsumes it):

- A non-root child is **underfull** when its `payload < C/2`.
- When a child is underfull, **merge** it with an adjacent sibling — prefer the **right**
  sibling (`child[i+1]`) if it exists, else the **left** (`child[i-1]`) — by concatenating
  `left records‖separator‖right records` (and the children, if interior) into one node `M`,
  removing that separator and the absorbed child from the parent.
  - If `M` fits (`payload(M) ≤ C`): `M` replaces the pair; the **parent loses one key**, so
    the parent may itself become underfull — handled when it returns to *its* parent.
  - If `M` overflows: **split `M` 2-way** by the rule above; the two halves and the new
    separator replace the pair, so the **parent's key count is unchanged**.
- **Root collapse:** if the root drains to `N = 0`, it is replaced by its single child
  (interior root → height − 1) or becomes empty (leaf root → `root_data_page = 0`).

Because a merged node is at most `(< C/2) + RECORD_MAX + (≤ C) < 2·C` (the underfull child is
`< C/2`, the separator ≤ `RECORD_MAX`, the sibling ≤ `C`), a single 2-way split always restores
fit — the same `≤ 2C ⇒ one split suffices` bound the insert path relies on.

**Why the record cap.** Capping a record at `RECORD_MAX = (C-12)/2` makes a two-record node —
leaf or interior — never exceed `C` (a leaf's two records are `≤ C-12 ≤ C`; an interior's two
records plus three child pointers are `≤ (C-12) + 12 = C`), so a node overflows only at
`N ≥ 3`, and the split point (either rule above — both are bounded by `min(m_append, N-2)`)
always lands in `[1, N-2]` with both halves non-empty and ≤ `C`. Without the cap, a node could overflow on
its **last**, oversized record, forcing an empty sibling (a degenerate node) or a multi-way
spill — both of which complicate the byte contract across four implementations. The cap buys an
all-2-way, no-empty-node scheme at the cost of a tighter (and later-liftable) oversized-row
limit.

### Value codec

A row value is encoded behind a named `encode_value`/`decode_value` seam, by column type. All
forms begin with a 1-byte **presence tag**: `0x00` **present-inline-plain**, `0x01` **NULL** (the
tag alone), `0x02` **present-external-plain** (the body is an overflow pointer), `0x03`
**present-inline-compressed**, `0x04` **present-external-compressed** (the `0x02`–`0x04` bodies
are in *Large values* below). Any other tag is `data_corrupted`. `0x00` and `0x01` are **unchanged
from v1**. The present-**inline-plain** body depends on the type:

- **Integers** (`int16`/`int32`/`int64`) — the **same order-preserving bytes as keys**
  ([encoding.md §2.1](../design/encoding.md)): fixed-width big-endian, sign-bit flipped.

- **`text`** — a **`u16` byte-length** (big-endian) then exactly that many **UTF-8 bytes** (the
  `C` collation's bytes, verbatim — no escaping, no terminator). The empty string is
  `00`(tag)`00 00`(len). A value whose UTF-8 length exceeds `0xFFFF` is a write-side `0A000`.

- **`boolean`** — a single **`bool-byte`** body: `00` false, `01` true (any other byte is
  `data_corrupted`).

- **`decimal`** — a compact self-describing codec: a **`u8` `flags`** (bit0 = sign, `1` =
  negative; bits 1–7 reserved `0`); a **`u16` `scale`** (the value's display scale `s`); a
  **`u16` `ndigits`** (number of base-10⁴ groups); then `ndigits` × **`u16`** (big-endian)
  groups, **most-significant first**, each `0 … 9999`. **Canonical zero** is
  `flags=0, scale=s, ndigits=0`.

- **`bytea`** — a **`u16` byte-length** then that many **raw bytes** (no UTF-8 validation; any
  byte allowed). The empty value is `00`(tag)`00 00`(len).

- **`uuid`** — a **fixed 16-byte** body (the raw `uuid-raw16` bytes — encoding.md §2.7), with
  **no length prefix**.

- **`timestamp` / `timestamptz`** — both store an **`int64` microsecond instant** via the
  **same 8-byte order-preserving integer body as `int64`** (the two type codes 9/10 differ in
  semantics, not bytes); the `±infinity` sentinels are the extreme `int64` values.

- **`interval`** — a **fixed 16-byte** body: **`i32` months**, **`i32` days**, **`i64` micros**,
  each **big-endian two's-complement** (plain — **no** sign-flip; this is a value codec, not an
  order-preserving key, and interval is not a key this slice — [../design/interval.md](../design/interval.md)).
  No length prefix; the three fields are independent (PG's representation), and comparison goes
  through the canonical 128-bit span at runtime, never these bytes.

- **composite** (`type_code 14`, v9 — [../design/composite.md §4](../design/composite.md)) — a
  **null bitmap** of `ceil(field_count / 8)` bytes (**MSB-first**: field *i*'s NULL bit is
  `0x80 >> (i mod 8)` of byte `i / 8`; a set bit = that field is NULL and contributes **zero** body
  bytes), then each **present** field's value-codec body **in declaration order**, written
  **without its own presence tag** (the bitmap carries presence). A field that is itself a
  composite **recurses** (its body is another bitmap + field bodies). A **whole-value-NULL**
  composite is the lone `0x01` tag (no bitmap). The field types come from the column's composite
  type in the catalog, so the body is self-delimiting. Worked example,
  `addr AS (street text, zip int32)`: `('Main', 90210)` → `00`(tag) `00`(bitmap) `00 04 4D 61 69 6E`
  (text body) `80 01 60 62` (int32 body) — an 11-byte body; `('Main', NULL)` → `00`(tag) `40`
  (bitmap: field 1 NULL) `00 04 4D 61 69 6E` (the int field omitted) — a 7-byte body.

- **array** (`type_code 15`, **v10** — [../design/array.md §4](../design/array.md)) — `ndim u8`,
  `flags u8` (bit 0 = `HAS_NULLS`; other bits reserved, 0), then per dimension `len u32 BE` +
  `lb i32 BE`, then (only when `HAS_NULLS`) a **null bitmap** of `ceil(N / 8)` bytes (MSB-first, like
  composite; `N` = product of the dim lengths), then each **present** element's value-codec body
  **without its own presence tag** (row-major). `ndim` ranges 0–6 (`MAXDIM`): an **empty array** is
  `ndim = 0` (the two bytes `00 00`, no dims/bitmap/elements); a 1-D value is `ndim = 1`; a multidim
  value records each dimension's `len`/`lb` (the `lb` field carries a value's custom lower bound —
  [../design/array.md §12](../design/array.md)). A **whole-value-NULL** array is the lone `0x01` tag. The element
  type comes from the column's array type in the catalog, so the body is self-delimiting; fixed-width
  elements pay **no** per-element prefix. Worked example, `int32[]`: `{1,2,3}` → `00`(tag) `01`(ndim)
  `00`(flags) `00 00 00 03`(len) `00 00 00 01`(lb) `80 00 00 01 80 00 00 02 80 00 00 03`(three int32
  bodies); `{1,NULL,3}` → `00 01`(HAS_NULLS) `00 00 00 03 00 00 00 01` `40`(bitmap: elem 1 NULL) +
  the bodies for elements 0 and 2.

**Rowid reconstruction (no-PK tables).** The synthetic rowid is allocated from a **monotonic
counter** that is never reused. It is **not stored** — on load it is set to `max(rowid) + 1`
over the table's persisted keys (0 for an empty table), exact because a no-PK key is a bare
`int64` rowid and the rowids issued are `0, 1, 2, …`. Walking the B-tree in key order yields
the rowids in ascending order; the largest is the rightmost leaf's last key.

### Large values (overflow pages + compression, v3)

When a record would exceed `RECORD_MAX`, the engine **compresses** its largest variable-length
values and, where that is not enough, stores them **out-of-line**, so the record falls back under
the cap (the design rationale and decisions are in
[../design/large-values.md](../design/large-values.md) §12/§13). The mechanism:

- **Disposition decision (deterministic, a §8 contract).** Compute the all-inline-plain record
  size `R = 2 + key_len + Σ value_size`; if `R ≤ RECORD_MAX`, every value stays inline-plain —
  a record that fits is **never** compressed or spilled. Otherwise run two passes over the
  spillable values (`text`/`bytea`/`decimal`; fixed-width types never compress or spill), each
  pass visiting **largest encoded size first, ties broken by ascending column index**:

  1. **Compress pass.** Candidates: spillable values whose content payload is
     ≥ **`S_COMPRESS = 32`** bytes, ordered by their **inline-plain** encoded size. For each, in
     order, while `R > RECORD_MAX`: run the pinned LZ4 encoder ([lz4.md](lz4.md)) over the
     payload; adopt the compressed form **iff its encoded inline size (`7 + comp_len`) is
     strictly smaller than the inline-plain encoded size** (the *store-smaller* rule —
     a non-shrinking value stays plain, so a reader never pays for a useless decompression).
  2. **Externalize pass.** Candidates: spillable values whose **current** encoded size exceeds
     their external-pointer size (9 bytes plain / 13 bytes compressed), ordered by current
     encoded size. For each, in order, while `R > RECORD_MAX`: move the value's stored bytes
     (compressed if pass 1 adopted compression, else plain) into an overflow chain, leaving the
     fixed pointer in the record.

  The same rule computes the B-tree split weight (`record_size`) and drives the serializer, so
  in-memory split points match on-disk pages, and the per-value compression **attempts** of pass 1
  are what the `value_compress` cost unit meters ([../design/cost.md](../design/cost.md) §3).

- **External-plain pointer (`0x02`).** An externalized plain value's body is the presence tag
  `0x02` then **`u32 first_page`** + **`u32 payload_len`** — a fixed **9-byte** in-record footprint
  regardless of the value's size. `payload_len` is the length of the value's **content payload**:
  the raw UTF-8 bytes (`text`), the raw bytes (`bytea`), or the decimal body
  (`flags|scale|ndigits|groups`, `decimal`). The `u32` length supersedes the inline `u16` cap.

- **Inline-compressed (`0x03`).** The tag, then **`u32 raw_len`** (the content payload's
  decompressed length) + **`u16 comp_len`** + that many bytes of the [lz4.md](lz4.md) block —
  `7 + comp_len` bytes in the record. `comp_len` fits `u16` because an inline form only survives
  the disposition decision inside a record ≤ `RECORD_MAX ≤ 32762`. The reader decompresses to
  `raw_len` bytes and reconstructs the value by column type (exactly the external content payload).

- **External-compressed (`0x04`).** The tag, then **`u32 first_page`** + **`u32 stored_len`** +
  **`u32 raw_len`** — a fixed **13-byte** footprint. The overflow chain carries `stored_len` bytes
  of the **compressed** block (the chain page count follows the compressed size); the reader
  gathers them, decompresses to `raw_len`, and reconstructs by type.

- **Overflow page (`page_type = 4`).** The chain's stored bytes — the content payload (`0x02`) or
  the compressed block (`0x04`) — are split into **`C`-byte slabs** (`C = page_size − 16`), one per
  page, written in order. Each overflow page's header carries `item_count` = the bytes on **this**
  page and `next_page` = the continuation (`0` terminates). The reader follows `next_page` from
  `first_page`, gathering `payload_len`/`stored_len` bytes, then reconstructs the value by column
  type (decompressing first for `0x04`). Overflow pages are ordinary pages for allocation,
  copy-on-write commit, and reclamation (the free-list); the reachability walk collects a live
  record's chain so its pages are never reused while referenced.

- **Allocation order (golden-pinned).** In a from-scratch image a node's own page is allocated
  first, then — while encoding its records in key order — each external value's chain is allocated
  in **column order**, contiguously. This fixes the byte layout the goldens pin (`overflow_table.jed`).

A record that still exceeds `RECORD_MAX` after compressing and externalizing **every** spillable
value (pathological: a huge key, or very many columns at a tiny page) remains a write-side
`feature_not_supported` (`0A000`).

## Allocation & incremental commit

A commit materializes the writer's new committed `Snapshot` (transactions.md §2) by writing
only its **dirty** pages, then publishing the new root. The §2 atomicity rests on a fixed
**write ordering** (storage.md §4):

1. **Allocate** a page index to each dirty page. A page is **dirty** iff it was newly built by
   this transaction's copy-on-write — i.e. it has no on-disk page id yet. Clean nodes (shared
   with the prior committed tree via structural sharing) **keep their existing page** and are
   **not rewritten** — that is the incremental win. Allocation draws from the **free-list**
   first — the **lowest** free page index, deterministically, so the bytes stay cross-core
   identical — and **extends the file** (a fresh index at `page_count`, bumping it) only when
   the free-list is exhausted. A page leaves the free-list **only** by being allocated here, so
   it is immediately part of the new committed version and never of any older one (see
   *Reclamation* below).
2. **Write** the dirty body pages in this **deterministic order**, so the bytes are
   cross-core identical:
   - For each table in **lowercased-name order**, the dirty nodes of its B-tree in
     **post-order** (a node's children before the node — so a parent's child pointers
     reference already-allocated pages), left to right; **then each of its index trees**
     (v5), in the catalog's index order (lowercased-name ascending), post-order each.
   - Then the **catalog chain** (always rewritten fresh: it carries the possibly-moved
     `root_data_page` of every table), as consecutive pages.
3. **`sync()`** — every body page is durable.
4. **Write the meta** to slot `txid & 1` (new `txid`, new `root_page` = the new catalog head,
   new `page_count`).
5. **`sync()`** — the meta is durable; the commit is **published**.

A crash between steps 3 and 5 leaves the prior meta valid (its body pages are intact — copy-on-
write never overwrote them), so the database opens at the prior snapshot; the freshly written
body pages are simply unreferenced. A torn meta write at step 4 is caught by the meta checksum and
falls back to the other slot. Either way the file is never corrupt — it is always a valid
snapshot, the new one or the immediately prior one (storage.md §4, transactions.md §9). **Bit-rot
of an at-rest body page** — distinct from a crash — is caught separately by the **per-page
checksum** (v7, *Page header* above): the page's CRC fails to verify the instant it is parsed, so
a damaged catalog/node/overflow page surfaces as `XX001` rather than wrong rows. This is
**verified at each of steps 1–5** by the fault-injection seam (storage.md §7): a test-only one-shot
crash/tear armed on the pager, exercising mid-body, between-syncs, and torn-meta-write points with a
cross-core recovery matrix.

### Reclamation (the free-list, P6.2)

P6.1 **leaked** every page an old root stopped referencing — `page_count` only grew. **P6.2
reconstructs a free-list of those dead pages and the allocator (step 1) reuses them**, so a
file's size is bounded by its live data plus a session's churn instead of growing on every
commit. The free-list is **reconstructed on open, not persisted** (the TODO's
*reconstruct-on-open first*; the meta's reserved offset-28 field stays `0` — an on-disk
free-list head that lets open skip the walk is a later *open-speed* optimization):

- **On open**, the free-list is `[2, page_count)` **minus the pages reachable from the
  committed root** (the catalog chain plus every table **and index** B-tree node — all
  already walked while loading). Those reachable pages are the only live ones; everything else in the file is dead
  space left by earlier commits and is free.
- **During a session**, the allocator (step 1) draws dirty/catalog pages from the free-list
  (lowest index first) before extending the file. A page leaves the list **only** by being
  allocated, which makes it live in the new committed version — so **a free-list page is never
  reachable from the committed snapshot nor from the immediately-prior (fallback) snapshot**,
  and overwriting it is torn-write-safe (a crash mid-commit falls back to a snapshot that does
  not reference it — *Allocation & incremental commit* above).
- Pages an old root orphans **during** the session are **not** returned to the free-list this
  slice; they are reclaimed at the **next open** (when the free-list is reconstructed). A
  long-lived writer therefore still grows within one session, then compacts on reopen.

**The watermark (transactions.md §8).** A page freed at `txid T` is reusable only once
`oldest_live_txid > T`. Every reconstructed free-list page was already dead at the committed
version when the file was opened (`last-ref < committed.txid`), and on a single file-backed
handle `oldest_live_txid == committed.txid`, so the gate holds trivially. It becomes
load-bearing when **continuous (within-session) reclamation** and **file-backed reader
sharing** land together: returning a just-orphaned page to the free-list must then wait until
`oldest_live_txid` passes the version that last referenced it, lest a still-open reader on an
older snapshot observe a recycled page. Continuous reclamation (return orphans immediately —
needs O(dirty) orphan tracking so a commit stays incremental, or an O(live) reachable-set
recompute) and on-disk free-list persistence are the documented follow-ons.

**From-scratch image (`to_image`).** A clean, garbage-free image of a snapshot — used by
`create`'s initial write and by the **golden tests / Ruby reference** — is the special case
where *every* node is dirty: allocate and write the whole tree (post-order per table, in
name order; each table's tree then its index trees in catalog order) starting at page 2,
then the catalog chain, then both meta slots at `txid = 1`.
This is what the fixtures pin. (An incrementally-committed file additionally contains leaked
pages from intermediate commits; its round-trip correctness is verified by per-core tests, not
by a static golden, because it depends on the commit history.)

## Edge cases

- **Empty database** (no tables): one catalog page with `item_count = 0`; `root_page = 2`,
  `page_count = 3` (two meta slots + the catalog page).
- **Empty table** (no rows): `root_data_page = 0`; no node pages.
- **Single-row table**: one leaf node, `N = 1`.

## Fixtures

[fixtures/](fixtures/) holds byte-exact goldens at `page_size = 256`, generated and checked by
the independent Ruby reference in [verify.rb](verify.rb) (run via `rake verify`). Each fixture
is the **from-scratch image** of its logical content (built by inserting the rows in the
listed order, then serialized clean). Fixtures sized to force a **multi-level tree** exercise
the interior-node format and the split contract.

| fixture | exercises |
|---|---|
| `empty_db.jed` | zero tables; catalog `item_count = 0`; `root_page = 2` |
| `overflow_table.jed` | large **incompressible** `text` + `bytea` values that **spill out-of-line plain** (v3) — `page_type 4` overflow chains (3-page + 2-page), the `0x02` external pointer (compression attempted, rejected by *store-smaller*), and an inline+external+NULL mix in one leaf ([../design/large-values.md](../design/large-values.md) §12) |
| `compressed_table.jed` | large **compressible** values (v3, Slice B) — a `0x03` inline-compressed text (a long run that compresses back under `RECORD_MAX`), a `0x04` external-compressed text (compressed block still over the cap → a chain holding **compressed** bytes), an inline-compressed bytea, and an inline-plain + NULL mix ([../design/large-values.md](../design/large-values.md) §13, [lz4.md](lz4.md)) |
| `one_table_empty.jed` | one table, zero rows (`root_data_page = 0`) |
| `pk_table.jed` | an int PK table whose rows force a **3-node tree** (interior root + two leaves) at page 256 — the load-bearing interior-node + split proof; includes a NULL value in a row |
| `text_table.jed` | a text column — the value codec's text branch; empty string, embedded quote, multi-byte + astral chars, a NULL (single leaf) |
| `bool_table.jed` | a boolean column — the `bool-byte` value branch (single leaf) |
| `bool_pk_table.jed` | a **boolean PRIMARY KEY** (the second non-integer stored key — the `bool-byte` key, `00`/`01`, no presence tag) + a nullable boolean column; rows in key order (false then true) |
| `decimal_table.jed` | a `decimal` column — the decimal branch + per-column `numeric(p,s)` typmod; positive/negative/zero/multi-group/NULL |
| `bytea_table.jed` | a bytea column — the bytea branch; empty value, embedded `0x00`, a high byte, a NULL |
| `uuid_table.jed` | a **uuid PRIMARY KEY** (the first non-integer stored key — the §8 cross-core key-path proof) + a nullable uuid column |
| `default_table.jed` | columns with a **constant** `DEFAULT` — the `has_default` flag (bit2) + default value codec written after the typmod |
| `default_expr_table.jed` | columns with an **expression** `DEFAULT` (v8) — the `default_is_expr` flag (bit3) + the expr-text after the typmod: a `uuid DEFAULT uuidv7()`, an `int32 DEFAULT 1 + 1`, beside a plain no-default column and a constant-default column (bit2) in the same catalog (empty table — the row eval is covered by the conformance corpus) |
| `timestamp_table.jed` | a timestamp column — the 8-byte int64 branch; epoch, pre-1970, BC-era, `±infinity`, NULL |
| `timestamptz_table.jed` | a timestamptz column — the same 8-byte branch under type code 10 |
| `interval_table.jed` | an interval column — the fixed 16-byte branch (i32 months ‖ i32 days ‖ i64 micros, big-endian); a positive multi-field value, a negative value, the zero interval, a months-only/`'1 mon'` value (vs a `'30 days'` value that is span-equal but byte-distinct), and a NULL (single leaf) |
| `nopk_table.jed` | a no-PK table — the stored synthetic `int64` rowid key |
| `composite_pk_table.jed` | a **composite PRIMARY KEY** (`int32` ‖ `int16`) — the concatenated key encoding (encoding.md §2.3) + the v5 `pk_ordinal` list; negative first component and tie-breaking second |
| `index_table.jed` | **secondary indexes** (v5) — a table whose PK list order differs from declaration order (`PRIMARY KEY (b, a)` — the lifted narrowing), one single-column index over a **nullable** column holding a NULL (the encoding.md §2.2 presence tag in stored index order, NULL last) and one auto-named two-column index; empty-payload index records |
| `unique_table.jed` | **unique indexes** (v6) — the per-index `index_flags` byte: a `UNIQUE` constraint's auto-named `t_v_key` (over a nullable column holding two NULLs — *NULLS DISTINCT* stored side by side), a named two-column constraint, a `CREATE UNIQUE INDEX`, and one plain index (`index_flags` 0) in the same catalog |
| `check_table.jed` | **`CHECK` constraints** (v4) — the catalog check list: an auto-named single-column check, an explicitly-named multi-column check, and a check whose text exercises the token rendering (string + decimal literals, `<=`); stored in name order |
| `tall_tree.jed` | enough small int rows to force a **two-level interior** (height-2 tree) — exercises interior-of-interior child pointers and post-order page allocation |
| `torn_meta_slot0.jed` | slot 0 checksum corrupted → loader falls back to slot 1 |
| `torn_meta_slot1.jed` | slot 1 checksum corrupted → loader falls back to slot 0 |

**Incompressible filler (`filler64`).** Fixtures (and mirrored core tests) that need a value the
LZ4 encoder cannot shrink generate it with a pinned PRNG, identical in all four implementations:
**xorshift32 with seed `0x4A454442`** (`"JEDB"`), each step `x ^= x << 13; x ^= x >> 17;
x ^= x << 5` (all modulo 2³²), emitting per step the character `ALPHA64[x mod 64]` where
`ALPHA64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"` (for `text`) or the
byte `x mod 256` (for `bytea`). High-entropy output has no 4-byte repeats for the encoder to match,
so compression never wins *store-smaller* and the value stays plain — deterministically.

The "highest `txid` wins" selection (vs. the torn-write fallback), the **slot alternation**
across consecutive commits, and the **incremental dirty-page-only write** are covered by
per-core unit tests that perform real multi-commit sequences against a file (a fresh
whole-image write gives both slots the same `txid`, so these are not expressible as a static
golden).
