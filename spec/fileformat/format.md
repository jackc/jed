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

## Version scope (`format_version` 24)

`format_version` **24** — the **B+tree reshape** ([../design/bplus-reshape.md](../design/bplus-reshape.md),
slice B1). Two coupled changes, deliberately one bump:

1. **B-tree → B+tree.** Every ordered tree (table stores, secondary btree indexes, GIN entry
   trees) becomes a **B+tree**: records live **only in leaves**; an **interior** page
   (`page_type = 3`) is a record-free routing skeleton — `N+1` child pointers ‖ a **separator
   directory** ‖ the separator **key blob** (*Interior node* below). A separator is a **copy of a
   boundary key** (never a record), so interior fan-out is governed by `(separator + pointer)` fit —
   far higher than v23's `(record + pointer)` fit, so trees are shallower. The **split/merge byte
   contract is regenerated** for the two node kinds: leaf splits **copy up** the right half's first
   key; interior splits **push up** the median separator; leaf merges concatenate and **remove** the
   parent separator; interior merges **pull it down** (*Fan-out* below). GiST pages (`page_type`
   5/6) are untouched (their own layout); overflow and catalog pages are byte-identical to v23.
2. **Leaf column regions gain a region header; the per-value NULL tag is gone.** Each leaf column
   region now leads with a **flags byte** (reserved `0` — the door a later string dictionary will
   use) and stores its values in one of two class-determined shapes (*Leaf node* below): a
   **fixed-width** column region is a **null bitmap** + a **dense `N × width` slot array** (no
   per-value presence tag, no value directory — the gather/SIMD stride); a **variable-width**
   column region is a **value directory** + the v23 per-value codec bytes with **NULL encoded as a
   zero-length span** (no bitmap, no `0x01` tag byte). The presence tag `0x01` never appears inside
   a v24 leaf; the single-value codec — catalog defaults, overflow chain content, composite/array
   element bodies — is **byte-unchanged** (`0x01` lives on there).

Directories throughout (leaf key directory, variable-region value directories, interior separator
directory) are **end-offset directories**: `N` big-endian `u32`s where `off[i]` is the byte offset
one past item `i` within its blob — item `i` spans `[off[i−1], off[i])` with `off[−1] = 0` (the
redundant leading zero of v23's `N+1`-entry prefix sums is dropped; this is part of what keeps the
two-record fit under the unchanged `RECORD_MAX`, *Why the record cap* below). The record's **split
weight** is restated as its actual attributable bytes — `record_size = key_len + Σ value_size`
with a NULL variable-width value contributing **0** and a fixed-width value always contributing its
**width** (*Record* below); the v23 phantom `2 +` is dropped. `RECORD_MAX` **keeps its v23 value**
`(C − max(12, 12+16·K))/2`, re-derived leaf-only (*Why the record cap*).

`format_version` **23** was the **PAX leaf layout** (superseded in arrangement details by v24; the
column-major *shape* carries forward). A B-tree **leaf** page stored its records **column-major**
(a *Partition Attributes Across* layout — each record still lives wholly on one page, but its
columns are grouped): a key directory (`N+1` `u32` prefix-sum) ‖ key blob ‖ a column directory
(`K+1` `u32`) ‖ per column a value directory (`N+1` `u32` prefix-sum) + the column's value bodies
(the unchanged 1-byte-presence-tag codec, NULL = a `0x01` byte). **Interior** pages stayed
row-major and carried full records (a CLRS-style B-tree). The per-record `key_len u16` was dropped
(directories carry lengths); the split weight stayed `2 + key_len + Σ value_size` and `RECORD_MAX`
tightened to `(C − (12+16·K))/2`. The payoff — a **single-column scan reads one contiguous byte
run**, the columnar/vectorized-execution enabler — carries into v24 unchanged.

`format_version` **22** — **`varchar(n)` length limits** ([../design/types.md §15](../design/types.md)).
A `text` column entry now appends, in the **typmod slot** (where a `decimal` appends precision/scale),
a single **`u32 varchar_max_len`** big-endian — `0` = unbounded (a plain `text` / `varchar` / `string`
column), `1 … 10485760` = the `varchar(n)` / `string(n)` length limit. A composite type's `text`
**field** carries the same `u32` in its typmod slot. The stored **value** codec is unchanged (`text`
is still `0x00` present ‖ `u16` byte length ‖ UTF-8 — an over-length value is rejected `22001`, or
truncated, *before* it reaches the codec). A file whose every `text` column is unbounded still moves
to v22 by its version byte + meta CRC and a `varchar_max_len = 0` on each text column/field.

`format_version` **21** — **`EXCLUDE` constraints** ([../design/gist.md](../design/gist.md) §7/§8,
GX3). The table entry gains a per-table **exclusion list** *after* the foreign-key list (before
`root_data_page`): `excl_count u16`, then per exclusion the constraint **name** (`u16` length +
UTF-8), the backing **GiST index name** (`u16` length + UTF-8), and an **element vector**
(`elem_count u16`, then per element a `column_ordinal u16` + an `operator_strategy u8` — `&&` = 0,
`=` = 1) — in ascending lowercased-name order (*Exclusion list* in the *Catalog* section below). The
backing GiST index is stored like any GiST index, but the **index list now admits multi-column GiST
indexes**: a leaf/interior bound is the per-column component bounds **concatenated** (each `range_ops`
range body or scalar `[min,max]` key blob), so a single-column GX1/GX2 index is byte-unchanged. An
exclusion constraint owns no extra B-tree beyond its backing index. A table with no exclusion still
moves to v21 by its version byte + meta CRC (every table entry gains `excl_count = 0`).

`format_version` **20** — **GiST indexes** ([../design/gist.md](../design/gist.md), GX1). A per-index
`index_kind = 2` selects the GiST access method, and the index's on-disk form is a persisted **R-tree**
of bounding-predicate nodes, in two new `page_type`s — `5` (GiST leaf) / `6` (GiST interior) (see
*Page header* + *Catalog* below). A leaf entry is `bound_len u16 ‖ encode_range_body(bound) ‖
skey_len u16 ‖ skey`; an interior entry is `bound_len u16 ‖ encode_range_body(union) ‖ child_page u32`;
nodes are ordered canonically (`range_total_cmp`, ties by storage key / subtree-min key) and allocated
post-order, so the tree is a pure function of the indexed row set — byte-identical cross-core. The
catalog index entry is otherwise unchanged (`index_root_page` points at the R-tree root, `0` for an
empty index). A file with no GiST index moves to v20 only by its version byte + meta CRC.

`format_version` **19** — **storable `json` / `jsonb` columns**
([../design/json.md](../design/json.md), slices J1/J1b). A column type can now be **`json`**
(`type_code` 18) or **`jsonb`** (`type_code` 19) — both **plain scalar** catalog entries with **no
extra descriptor** (like `text`/`uuid`; the reserved `has_jsonb_dict` door — json.md §3.2 — stays
clear, zero bytes), so a table with no json/jsonb column is byte-unchanged but for the version byte +
meta CRC. A **`json` value** stores the input text **VERBATIM**, length-prefixed exactly like `text`
(`0x00` present ‖ `u16` byte length ‖ UTF-8). A **`jsonb` value** stores the canonical decomposed
binary form: the `0x00` present tag, then a **self-delimiting tagged-node tree** serialized
depth-first (no outer length prefix — it walks itself, like array/range). Every node leads with a
**one-byte tag** (low nibble = kind, high nibble = flags, reserved `0`): `0x0` null, `0x1` false,
`0x2` true, `0x3` number (the **decimal value body** — `flags ‖ u16 scale ‖ u16 ndigits ‖ groups`),
`0x4` string (an **unsigned LEB128 varint** byte-length ‖ UTF-8), `0x5` **`STRING_DICT`**
(*reserved* — the dictionary door §3; a reader before the dictionary slice rejects it `XX001`), `0x6`
array (varint count ‖ child bodies), `0x7` object (varint count ‖ members — each a string-node key
‖ value node, in canonical key order: length-then-bytewise). A nonzero flag nibble or `STRING_DICT`
is `XX001` data_corrupted. Numbers are exact `decimal` (never binary float); object keys are deduped
last-wins then sorted at parse time, so the bytes are a pure function of the value. Both bodies are
variable-length and ride the **large-value overflow + LZ4 path** like `text`/`bytea`. The goldens
`json_table.jed` / `jsonb_table.jed` pin the bytes `rust == go == ts == ruby`. A reader accepts
**only** version 19.

`format_version` 18 was **reference-only collations** (the
reference-only pivot, [../design/collation.md §2/§5/§9](../design/collation.md)). A referenced
collation is a **kind-tagged catalog entry** (`entry_kind` 3 — the *Catalog* section below), emitted
**after sequences and before tables** (*composites → sequences → collations → tables*), and it is now
**metadata only**: a **flags byte** (bit0 `is_default`) followed by the **name**, the
**`(unicode_version, cldr_version)` version pin**, and the **description** — each a `u16` length +
UTF-8. The **compiled table is NOT in the file**: it is **vendored into the binary** and resolved by
name on open; the recorded version is the pin a future graded verdict checks. This **supersedes
v17's baked snapshot** (the LZ4-compressed `.coll` artifact is gone). The **per-database default
collation** is the `is_default`-flagged entry (no separate header/meta field — `C` ⇒ none flagged). A
**per-column collation** rides the column-entry **flags byte bit 6 `has_collation`**; when set, a
trailing name (`u16` length + UTF-8) follows the default. A `C` (byte-order) column leaves the bit
clear and writes nothing, so a non-collated table is byte-unchanged but for the version byte + meta
CRC. A collation entry owns no B-tree. Each version is a **clean break** — older versions are **not read**
(we are pre-1.0 and owe no on-disk compatibility; CLAUDE.md §1, "we own our surface").

`format_version` 16 was **range columns**
([../design/ranges.md](../design/ranges.md)). A column type can be a **range** (`type_code` 17 —
the *Catalog* table below — followed by an **inline element-type descriptor**, one scalar code, the
same self-describing shape an array column uses one level down). A **range value** is a compact body:
a **flags byte** (bit0 `EMPTY`, bit1 `LB_INF`, bit2 `UB_INF`, bit3 `LB_INC`, bit4 `UB_INC`) followed
by the **present bound bodies** (each the element's value-codec body, no presence tag). The empty
range is the lone flags byte `0x01`; discrete subtypes (`i32`/`i64`/`date`) are stored in PostgreSQL
canonical `[)` form. A range owns no B-tree and carries no default this slice.

`format_version` 15 was **IDENTITY columns**
([../design/sequences.md §13](../design/sequences.md)). A `GENERATED { ALWAYS | BY DEFAULT } AS
IDENTITY` column desugars exactly like `serial` (an **owned** sequence — v14 — plus a `DEFAULT
nextval(...)` expression default — v8 — and `NOT NULL`), so the sequence entry, the owner link, and
the column's expression default are **unchanged**. The only on-disk change is in the **column entry's
`flags` byte**: it gains **bit 4 `is_identity`** and **bit 5 `identity_always`** (set only when bit 4
is, for `GENERATED ALWAYS`; clear for `GENERATED BY DEFAULT`) — the *Catalog* table below. An identity
column thus also carries bit 1 (`not_null`) and bit 3 (`default_is_expr`, the `nextval` default). A
non-identity column writes both bits 0, so its bytes are the v14 shape.

`format_version` 14 was the **`serial` owned-sequence link**
([../design/sequences.md §12](../design/sequences.md)). A `serial` / `bigserial` / `smallserial`
column creates a sequence that is **owned by** its column; the owner link must persist so `DROP TABLE`
auto-drops the owned sequence after a reopen. The only on-disk change is in the **sequence entry**: the
`flags` byte gains **bit 2 `has_owner`**, and when set, the entry's six i64 fields + flags are followed
by the owner reference — `owner_table_len u16` + owner table name, then `owner_column u16` (the owning
column's 0-based ordinal) — the *Sequence entry* table in the *Catalog* section below. A non-owned
sequence (a plain `CREATE SEQUENCE`) writes nothing after the flags byte, so its bytes are the v13
shape (only the version byte + meta CRC change). No value-codec, key-encoding, or B-tree change.

`format_version` 13 was **GIN inverted indexes**
([../design/gin.md](../design/gin.md)). A GIN index is a second index *kind* beside the ordered
B-tree; it owns an on-disk B-tree exactly as an ordinary index does, so the only catalog change is
in the **index list** of the table entry: each index entry gains a one-byte **`index_kind`**
discriminator (`0` = ordered B-tree, `1` = GIN) between its `index_flags` byte and its
`index_root_page` (*Catalog* below). An ordinary index writes `index_kind = 0`, so every table
entry with indexes grows by one byte per index; a GIN entry's *key bytes* differ (a term ‖
storage-key, [../design/gin.md §4](../design/gin.md)) but the page/record framing does not. The
per-table data B-tree, the value codec, and the meta page are untouched.

`format_version` 12 was **sequences**
([../design/sequences.md](../design/sequences.md)). A sequence (`CREATE SEQUENCE s`) is a
database-level catalog object — a named, persisted, monotonic i64 generator — that owns no B-tree
and adds no value-codec change, so the only on-disk change is a **third kind of catalog entry**:
`entry_kind = 2` (joining `0` = table, `1` = composite-type). A sequence entry carries the name, six
fixed `i64` fields (`increment`, `min_value`, `max_value`, `start`, `cache`, `last_value`), and a
`flags` byte (bit 0 `cycle`, bit 1 `is_called`) — the *Sequence entry* table in the *Catalog* section
below. Emission order across the catalog is **composite-type entries (kind 1), then sequence
entries (kind 2), then table entries (kind 0)**, each group in ascending lowercased-name order; a
sequence is referenced by nothing at load (a `DEFAULT nextval('s')` is stored expr-text, resolved at
evaluation), so it needs no two-pass.

`format_version` 11 was **`FOREIGN KEY` constraints**
([../design/constraints.md §6](../design/constraints.md)). A foreign key is referential metadata
on the *referencing* table — it owns no B-tree and adds no value-codec change — so the only on-disk
change is in the **table catalog entry**: after the index list and before the trailing root-page
pointer, a new **foreign-key list** (`fk_count`, then per FK: the constraint name, the local-column
ordinal list, the referenced table name, the referenced-column ordinal list into the parent, and a
one-byte `on_delete`/`on_update` action field — *Foreign-key list* in the *Catalog* section below).
The per-table data B-tree, the value codec, and the meta page are untouched. A file with no foreign
keys still moves to v11 (the version byte + meta CRC change, and every table entry gains an
`fk_count = 0`).

`format_version` 10 was **array (`T[]`) columns**
([../design/array.md](../design/array.md)). An array is a **structural** type (no catalog object —
the element type is carried inline), so the only on-disk change is in the **column entry**: a new
**`type_code = 15`** (the *Stable type codes* table) followed by the **element-type descriptor**
(the element's own type code, then — for a composite element — its name; *Array column* below) in
place of a scalar's typmod slot. A value is the compact **array body** (`ndim ‖ flags ‖ per-dim
(len, lb) ‖ optional null bitmap ‖ element bodies` — *Value codec* below); a fixed-width element
carries **no per-element length prefix**. The per-table data B-tree and the meta page are
untouched. A file with no array columns still moved to v10 (only the version byte + meta CRC
change).

`format_version` 9 was **composite (row) types**
([../design/composite.md](../design/composite.md)). A user-defined composite type
(`CREATE TYPE addr AS (street text, zip i32)`) is a database-level object, so the catalog —
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
   count (zero) differs (*The per-table data B+tree* below).

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
C             = page_size - 16                    # PAGE_HEADER (v7); the bytes a page body may hold
RECORD_MAX(K) = (C - max(12, 12 + 16·K)) / 2 (floor)  # largest a single B-tree LEAF record may serialize to
```

The `- 16` in `C` is the **v7 page header** (12-byte v6 header + a 4-byte per-page `crc32`).
The reserve inside `RECORD_MAX` is a **separate** quantity that keeps a **two-record leaf** fitting
`C` (since v24 records live only in leaves, so the cap is a **leaf** rule; v23's interior-pair
co-justification evaporated with interior records — the value is deliberately **kept**,
[../design/bplus-reshape.md §4.2](../design/bplus-reshape.md)). The v24 worst-case two-record leaf
overhead is `leafOverhead(2, cols) ≤ 12 + 13·K` (all-variable columns — *Leaf node* below), so
`2·RECORD_MAX(K) + leafOverhead(2, cols) ≤ C` holds for every column mix and a two-record leaf
**never** overflows; leaf overflow therefore happens only at `N ≥ 3` records, which is what lets
every leaf split be a clean 2-way with two non-empty halves (see *Why the record cap* below).
`RECORD_MAX(0) = (C-12)/2` is exact for index trees: a two-record index leaf is
`2·(C−12)/2 + 4·2 + 4 = C`. Interior nodes have **no record cap** — a separator is a copy of a key
(itself ≤ `RECORD_MAX(0)` by the leaf cap), and the interior split rules handle the degenerate
fan-out a near-cap separator forces (*Fan-out* below).

`leafOverhead(N, cols)` is the bytes a v24 leaf's payload carries **beyond** `Σ record_size` — the
key directory (`4·N`), the column directory (`4·(K+1)`), and each column region's header: a
**fixed-width** column contributes its flags byte + null bitmap (`1 + ceil(N/8)`), a
**variable-width** column its flags byte + value directory (`1 + 4·N`) (*Leaf node* below; `F` + `V`
= the table's fixed/variable column counts, `F + V = K`):

```
leafOverhead(N, cols) = 4·N + 4·(K+1) + F·(1 + ceil(N/8)) + V·(1 + 4·N)
```

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
| 4  | 2 | `format_version` (u16) — current = **`24`** |
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

**Opening (slot selection).** Validate each slot independently (magic, `format_version == 24`,
reserved == 0, `crc32`). Choose the **valid** slot with the **highest `txid`**; on a tie,
slot 0. Exactly one valid → use it (torn-write fallback). Neither valid → `data_corrupted`.

## Page header (catalog and B-tree pages, 16 bytes — v7)

| offset | size | field |
|---|---|---|
| 0 | 1 | `page_type` (u8) — `1` = catalog, `2` = B-tree **leaf**, `3` = B-tree **interior**, `4` = overflow; `5` = GiST **leaf**, `6` = GiST **interior** (**new in v20** — a persisted R-tree node, [../design/gist.md §4.1](../design/gist.md): a leaf entry is `bound_len u16 ‖ encode_range_body(bound) ‖ skey_len u16 ‖ skey`, an interior entry `bound_len u16 ‖ encode_range_body(union) ‖ child_page u32`) |
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

**Each catalog entry is kind-tagged (v9, extended v12/v18):** a leading `entry_kind` u8 — `0` = a
table entry, `1` = a composite-type entry ([../design/composite.md §3](../design/composite.md)), `2` =
a sequence entry ([../design/sequences.md §3](../design/sequences.md)), `3` = a collation reference entry
([../design/collation.md §5](../design/collation.md)). Entries are emitted in kind order
**composite-type (1), then sequence (2), then collation (3), then table (0)**, each group in ascending
lowercased-name order (collations sort by their exact, case-sensitive name). Each page's `item_count` is the number of entries (of any kind) it holds;
entries are packed greedily into the chain, kind-tagged in stream order, exactly as table entries
were through v8 (a single entry must fit one page, i.e. ≤ `C`, else `0A000`; the `RECORD_MAX = C/2`
cap is a B-tree-record rule and does **not** apply to catalog entries, which never split). Each group
is emitted in **ascending order of the lowercased name** (the engine stores each object set in a hash
map keyed by lowercased name; sorting by that key removes any iteration-order leak — CLAUDE.md §8;
names are unique after lowercasing, so there are no ties).

**Load is two-pass (v9):** the reader walks the whole chain collecting every composite-type entry
into a name→definition map, validates that every composite **referenced** by a column or a field
exists and that the reference graph is **acyclic** (a dangling or cyclic reference is `XX001`),
then builds the tables — resolving each composite column's type name against the map. Because of
nested composites a single pass cannot guarantee a referenced type is already read (name order
does not imply dependency order), hence the two passes. **Sequence entries (v12)** are
self-contained — referenced by nothing at load — so a sequence entry is registered directly into
the catalog's sequence set as the chain is walked, with no second pass.

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
| &nbsp;&nbsp;`flags` | u8 — bit0 reserved 0 (**was** `primary_key` through v4 — the `pk` list below is the authority), bit1 `not_null`, bit2 `has_default` (constant default), bit3 `default_is_expr` (**new in v8** — expression default; mutually exclusive with bit2, both set is `XX001`), bit4 `is_identity` (**new in v15** — a `GENERATED … AS IDENTITY` column; implies bit1 + bit3 — sequences.md §13), bit5 `identity_always` (**new in v15** — `GENERATED ALWAYS` when set, `GENERATED BY DEFAULT` when clear; meaningful only with bit4, else `XX001`), bit6 `has_collation` (**new in v17** — a text column with a non-`C` effective collation, collation.md §5); bit7 reserved 0 (reader trusts the bits) |
| &nbsp;&nbsp;`precision` | u16 — **only present when `type_code == 6` (decimal)**; `0` = unconstrained |
| &nbsp;&nbsp;`scale` | u16 — **only present when `type_code == 6` (decimal)** |
| &nbsp;&nbsp;`varchar_max_len` | u32 — **only present when `type_code == 4` (text)** (**new in v22**); `0` = unbounded (`text`/`varchar`/`string`), `1 … 10485760` = the `varchar(n)` length limit ([../design/types.md §15](../design/types.md)) |
| &nbsp;&nbsp;`default` | value-codec bytes — **only present when `flags` bit2 (`has_default`)**; written *after* the typmod |
| &nbsp;&nbsp;`default_expr_len` | u16 — **only present when `flags` bit3 (`default_is_expr`)**; written *after* the typmod (in place of `default`) |
| &nbsp;&nbsp;`default_expr` | UTF-8 — the default's expression text (*Check-expression text* below), `default_expr_len` bytes |
| &nbsp;&nbsp;`collation_len` | u16 — **only present when `flags` bit6 (`has_collation`)** (**new in v17**); written *after* the default |
| &nbsp;&nbsp;`collation` | UTF-8 — the effective collation name (`collation.md` §5), `collation_len` bytes |
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
| &nbsp;&nbsp;`index_kind` | u8 — **new in v13**: `0` = ordered B-tree, `1` = GIN ([../design/gin.md](../design/gin.md)); `2` = GiST (**new in v20** — a persisted R-tree, `index_root_page` points at its root, pages 5/6 above, [../design/gist.md](../design/gist.md)); `3…` reserved. At v20 a value `> 2` is `XX001`. A GIN/GiST index always has `index_flags` bit0 (`unique`) clear |
| &nbsp;&nbsp;`index_root_page` | u32 — the root B-tree node of this index, or 0 if the table has no rows |
| `fk_count` | u16 — the table's `FOREIGN KEY` constraints (**new in v11**; `0` for a table with none) |
| per foreign key (×`fk_count`): | |
| &nbsp;&nbsp;`fk_name_len` | u16 |
| &nbsp;&nbsp;`fk_name` | UTF-8 (original case) — the constraint name |
| &nbsp;&nbsp;`fk_local_count` | u16 — ≥ 1, the referencing column count |
| &nbsp;&nbsp;`fk_local_ordinal` ×`fk_local_count` | u16 each — referencing-column ordinals into **this** table, in declaration/list order; each `< col_count` (else `XX001`) |
| &nbsp;&nbsp;`fk_ref_table_len` | u16 |
| &nbsp;&nbsp;`fk_ref_table` | UTF-8 (original case) — the referenced (parent) table name |
| &nbsp;&nbsp;`fk_ref_count` | u16 — the referenced column count; must equal `fk_local_count` (else `XX001`) |
| &nbsp;&nbsp;`fk_ref_ordinal` ×`fk_ref_count` | u16 each — referenced-column ordinals into the **parent** table, in list order |
| &nbsp;&nbsp;`fk_actions` | u8 — bits 0–1 `on_delete`, bits 2–3 `on_update` (`0` = NO ACTION, `1` = RESTRICT; `2`/`3` reserved for CASCADE/SET NULL/SET DEFAULT, not written this slice); bits 4–7 reserved, written 0 (a set reserved bit, or an unknown 2-bit action, is `XX001`) |
| `excl_count` | u16 — the table's `EXCLUDE` constraint count (v21; 0 for a table with none) |
| &nbsp;&nbsp;`excl_name_len` | u16 |
| &nbsp;&nbsp;`excl_name` | UTF-8 (original case) — the constraint name |
| &nbsp;&nbsp;`excl_index_len` | u16 |
| &nbsp;&nbsp;`excl_index` | UTF-8 (original case) — the backing GiST index name (= `excl_name` for a GX3 constraint) |
| &nbsp;&nbsp;`excl_elem_count` | u16 — the constraint's element count (≥ 1, else `XX001`) |
| &nbsp;&nbsp;&nbsp;&nbsp;`excl_col_ordinal` | u16 — the element's column ordinal into **this** table (per element) |
| &nbsp;&nbsp;&nbsp;&nbsp;`excl_op` | u8 — the element's operator strategy (`0` = `&&`, `1` = `=`; other values are `XX001`) (per element, immediately after its `excl_col_ordinal`) |
| `root_data_page` | u32 — the **root B-tree node** of this table, or 0 if it has no rows |

Columns are emitted in declaration order. Checks are emitted in their **evaluation order** —
ascending byte order of the lowercased `check_name` ([../design/constraints.md
§4.4](../design/constraints.md)); a reader trusts that order (it never re-sorts). Indexes
are emitted in **ascending byte order of the lowercased `index_name`** (the catalog's
in-memory order and the planner's tie-break order — [../design/indexes.md
§5/§6](../design/indexes.md)); a reader trusts that order too. Foreign keys are emitted in
**ascending byte order of the lowercased `fk_name`** (the catalog's in-memory order and the
§6.4 child-side evaluation order — [../design/constraints.md §6.9](../design/constraints.md));
the reader trusts that order. A foreign key owns no B-tree, so it stores no root page; the
referenced table/column ordinals are resolved by name against the loaded catalog, and a
reference to a missing table or out-of-range parent column in an otherwise-valid file is
`XX001`. **Exclusion constraints** (v21) are emitted **after** the foreign-key list, in
**ascending byte order of the lowercased `excl_name`** (the catalog's in-memory order —
[../design/gist.md §7/§8](../design/gist.md)); the reader trusts that order. An exclusion
constraint owns no extra B-tree (its backing GiST index is in the index list); an out-of-range
`excl_col_ordinal` or an unknown `excl_op` in an otherwise-valid file is `XX001`.

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

An **IDENTITY column** (`flags` bit 4 `is_identity`, **v15**; [../design/sequences.md §13](../design/sequences.md))
adds **no new column-entry field** — it is an i16/i32/i64 column that also carries bit 1 (`not_null`)
and bit 3 (`default_is_expr`, whose persisted text is the `nextval('<table>_<col>_seq')` of its
**owned** sequence), plus bit 5 (`identity_always`) to distinguish `GENERATED ALWAYS` from `GENERATED
BY DEFAULT`. The backing sequence is an ordinary owned sequence entry (v14 `has_owner`), so `DROP
TABLE` auto-drops it and the column's default/owner bytes are exactly a `serial` column's; the
identity bits restore the INSERT/UPDATE gating (`428C9`) after a reopen.

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
| &nbsp;&nbsp;`varchar_max_len` | u32 — **only when `field_type_code == 4` (text)** (**new in v22**); `0` = unbounded, `1 … 10485760` = the `varchar(n)` length limit |

Fields are emitted in **declaration order** (the order they appear in `CREATE TYPE`). A field
type code of `14` references another composite **by name** (nested composites); a field type code
of `15` is an **array-typed field** (`xs i32[]`, [../design/array.md §12](../design/array.md) —
the mirror of an array-of-composite element), carrying the inline element descriptor **before** the
flags byte (where a nested-composite name sits) so the element type is self-describing. The
loader's two-pass validation rejects a dangling reference or a definition cycle — including a
composite reached **through an array field** — as `XX001` (a v10 additive extension; no
`format_version` bump, since an array element descriptor is already a v10 shape).

### Sequence entry (`entry_kind = 2`, v12; owner added v13)

A sequence entry records a `CREATE SEQUENCE name [options]` definition
([../design/sequences.md §3](../design/sequences.md)). The six i64 fields + flags are **fixed-width**
(every one always present, no presence tags); the v13 owner reference is a **conditional tail** gated
by the `has_owner` flag bit:

| field | encoding |
|---|---|
| `entry_kind` | u8 = `2` |
| `name_len` | u16 |
| `name` | `name_len` bytes UTF-8 (original case) |
| `increment` | i64 — 8 bytes **big-endian two's-complement, no sign-flip** |
| `min_value` | i64 — same encoding |
| `max_value` | i64 — same encoding |
| `start` | i64 — same encoding |
| `cache` | i64 — same encoding (stored for fidelity; behaves as `1`, sequences.md §7) |
| `last_value` | i64 — same encoding (the mutable counter; `= start` on a fresh sequence) |
| `flags` | u8 — bit0 `cycle`, bit1 `is_called`, bit2 `has_owner` (**new in v13**); bits 3–7 reserved, written 0 (a set reserved bit is `XX001`) |
| `owner_table_len` | u16 — **only present when `flags` bit2 (`has_owner`)** |
| `owner_table` | `owner_table_len` bytes UTF-8 — the owning table name (original case) |
| `owner_column` | u16 — the owning column's **0-based ordinal** in the table |

The six `i64` fields use the **interval-body encoding** (plain big-endian two's-complement) rather
than the order-preserving sign-flip, because a catalog entry is a *value*-codec context, not a sorted
key. `last_value` + `is_called` are the only mutable state; a `nextval` rewrites them, and the whole
catalog is rewritten copy-on-write at the next commit (transactions.md §4.5). A sequence entry owns
no B-tree (no `root_data_page`), like a foreign key. The owner reference (`has_owner`) records the
`OWNED BY` link a `serial` column establishes ([../design/sequences.md §12](../design/sequences.md)),
so a reopened database still auto-drops the owned sequence on `DROP TABLE`; a plain `CREATE SEQUENCE`
is non-owned (`has_owner = 0`, no tail).

### Collation reference entry (`entry_kind = 3`, v18)

A collation reference entry records that the database **references** a collation, by name + version
pin — it carries **no table** (the reference-only pivot, [../design/collation.md §2/§5/§9](../design/collation.md)).
The compiled table is **vendored into the binary** (`spec/collation/fixtures/*.coll`) and resolved by
name on open; the entry is metadata only:

| field | encoding |
|---|---|
| `entry_kind` | u8 = `3` |
| `flags` | u8 — bit0 `is_default` (this collation is the per-database default); bits 1–7 reserved 0 (a set reserved bit is `XX001`) |
| `name` | u16 length + UTF-8 — the collation name (e.g. `dev-root`) |
| `unicode_version` | u16 length + UTF-8 — the Unicode version pin the keys were built under |
| `cldr_version` | u16 length + UTF-8 — the CLDR version pin (`""` if none) |
| `description` | u16 length + UTF-8 — optional provenance (`""` if none) |

On open the engine reads the metadata, resolves the table from the binary's **vendored** set by name,
and (until the graded verdict of [../design/compatibility.md](../design/compatibility.md) lands) fails
legibly — `42704` — if this build does not vendor that collation. A collation entry owns **no B-tree**
(no `root_data_page`), like a sequence or foreign key. The **per-database default collation** is
whichever entry carries `is_default` (`C` ⇒ none flagged); on load the engine restores it from that
bit. Entries are emitted in ascending **case-sensitive name** order, after sequences and before tables,
so a collated table entry is read after the entry it references. `db.SetDefaultCollation` flips the bit.

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
| 1 | `i16` |
| 2 | `i32` |
| 3 | `i64` |
| 4 | `text` |
| 5 | `boolean` |
| 6 | `decimal` |
| 7 | `bytea` |
| 8 | `uuid` |
| 9 | `timestamp` |
| 10 | `timestamptz` |
| 11 | `interval` |
| 12 | `f64` |
| 13 | `f32` |
| 14 | composite (a user-defined row type — followed by the type name, **not** a fixed body; v9) |
| 15 | array (a structural `T[]` — followed by the element type descriptor, **not** a fixed body; v10) |
| 16 | `date` (a calendar date — i32 days since the Unix epoch, fixed 4-byte body; v12) |
| 17 | range (a structural range over a scalar element — followed by the element type descriptor, **not** a fixed body; v16 — [../design/ranges.md](../design/ranges.md)) |
| 18 | `json` (validated text stored verbatim, a `text`-shaped variable-length body; v19 — [../design/json.md §4](../design/json.md)) |
| 19 | `jsonb` (a self-describing tagged-node tree body, **not** a fixed body; v19 — [../design/json.md §2](../design/json.md)) |
| 20 | `jsonpath` (**RESERVED, not yet landed** — a compiled-path type stored as normalized source text, a `text`-shaped body; [../design/jsonpath.md §1](../design/jsonpath.md)) |

> **Codes 18–20 are allocations reserved by the JSON design** ([../design/json.md](../design/json.md),
> [../design/jsonpath.md](../design/jsonpath.md)) to prevent collisions with concurrent work; their
> value layouts are specified in those docs and become normative here when the first storable JSON
> slice lands (the `format_version` bumps `v18 → v19` then). A `json`/`jsonpath` value rides the
> `text` value codec; a `jsonb` value is the tagged-node body of [../design/json.md §2](../design/json.md),
> which reserves a `has_jsonb_dict` column-entry flag bit (clear today, the string-dictionary door)
> exactly as `has_collation` (below) reserves text's.

A **`f64`** value (`type_code == 12`) is the **8 IEEE 754 bytes, big-endian**, and a
**`f32`** value (`type_code == 13`) is the **4 IEEE 754 bytes, big-endian** — both behind the
presence tag, fixed-width, no length prefix, the `i64`/`uuid`/`timestamp` shape. The stored bits
are preserved verbatim for every value **except `NaN`**: a stored `-0.0` keeps its sign bit and
`±Infinity`/finite values keep theirs, but a `NaN` is **canonicalized to the single quiet pattern**
`0x7FF8000000000000` (`f64`) / `0x7FC00000` (`f32`) on write. A NaN's payload bits are
core-specific (Go's `math.NaN()` is `0x7FF8…001`, hardware `Inf − Inf` is the negative `0xFFF8…`),
so this NaN-only step is what keeps a stored NaN byte-identical across cores; everything else is
verbatim (the `-0 = +0` collapse is a comparison/key concern only — [../design/float.md](../design/float.md)
§3/§10). The on-disk bytes are byte-identical across cores (the float types are exempt from
cross-core identity only for *computed/rendered values*, not for *storage* — determinism.md §6).

A **text** column's effective collation is stored **only when non-`C`** (**new in v17**,
[../design/collation.md §5](../design/collation.md)): `flags` bit6 `has_collation` set, with the
name (`u16` length + UTF-8) appended after the default. A `C` (byte-order) column — the common case —
leaves the bit clear and writes nothing, so a non-collated column is byte-unchanged. `bytea` and
`uuid` have no collation. A non-integer PRIMARY KEY needs no extra catalog field — the key bytes live
in the data-page record (below), not the catalog.

A **decimal** column carries a **typmod** (the `numeric(p,s)` precision/scale) appended to the
column entry **only when `type_code == 6`** — two big-endian `u16`s, `precision` then `scale`.
`precision == 0` means **unconstrained** `numeric` (`scale` then `0` and ignored); a
constrained `numeric(p,s)` stores `precision = p` (`1 … 1000`) and `scale = s`.

A **text** column carries a **`varchar(n)` typmod** (**new in v22**, [../design/types.md §15](../design/types.md))
appended to the column entry **only when `type_code == 4`** — one big-endian `u32`, `varchar_max_len`.
`varchar_max_len == 0` means **unbounded** (`text` / `varchar` / `string`); `1 … 10485760` is the
`varchar(n)` / `string(n)` length limit (code points). The same `u32` rides a composite type's `text`
field. The limit is enforced before a value is encoded, so the value codec is unchanged.

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

## The per-table data B+tree

Each non-empty table's rows are an **ordered B+tree** keyed by the row's encoded storage key
(memcmp order — [encoding.md](../design/encoding.md)), rooted at the table's `root_data_page`.
It is the on-disk image of the in-memory copy-on-write B+tree (transactions.md §3, pmap):
node ↔ page, one-to-one. **All records live in leaves** (v24 —
[../design/bplus-reshape.md](../design/bplus-reshape.md)); an **interior** node is a record-free
routing skeleton of **separator keys** and child pointers. A separator is a **copy of a boundary
key** — the first key of the right subtree at the moment of the split (*Fan-out* below) — and may
go **stale** after later deletes (it keeps routing correctly: every key in the left subtree is
`< sep` and every key in the right is `≥ sep`, forever). Lookups always descend to a leaf; a key
**equal** to a separator lies in the **right** subtree.

### Record (a key and its row)

A **record** is a key and its row value, stored only in leaves, column-major (*Leaf node* below).
The key is **stored, not derived** (a no-PK synthetic rowid is not reconstructable from row data).
There is no per-record payload length — a value's width comes from its type and its region's
directory/stride. The **on-disk size of a record** — the **split weight** the fan-out rules below
measure, and the byte basis of the temp-table budget ([../design/temp-tables.md §7](../design/temp-tables.md)) —
is its actual attributable bytes:

```
record_size = key_len + Σ_columns value_size
value_size  = width(type)          for a fixed-width column (NULL included — a NULL occupies a slot)
            = 0                    for a NULL variable-width value (a zero-length span)
            = its stored codec bytes (presence tag + body, or a pointer form — *Large values*)
                                   for a present variable-width value
```

The v23 phantom `2 +` (the long-gone per-record `key_len u16`) is dropped: `record_size` is exactly
the bytes the record contributes to its leaf beyond `leafOverhead`.

**Column classes.** Every column type is either **fixed-width** — `i16` (2), `i32` (4), `i64` (8),
`boolean` (1), `uuid` (16), `timestamp`/`timestamptz` (8), `date` (4), `interval` (16), `f64` (8),
`f32` (4) — or **variable-width**: `text`, `bytea`, `decimal`, `json`, `jsonb`, composite, array,
range (exactly the spillable set of *Large values*). The class decides the column's leaf region
shape (*Leaf node* below).

**Index trees (v5).** A secondary index ([../design/indexes.md](../design/indexes.md)) is a
B+tree of exactly this shape, rooted at the catalog's `index_root_page`, whose records have
**zero value columns**: a record is its entry key alone (indexes.md §3), `record_size = key_len`.
Every node, split/merge, allocation, commit, and reclamation rule below applies to an index tree
unchanged — a GIN index's entry tree too ([../design/gin.md §4](../design/gin.md)).

**`RECORD_MAX`.** A single record's **stored** on-disk size must be ≤ `RECORD_MAX(K) =
(C − max(12, 12+16·K))/2` (floor), `K` the value-column count. The cap is what makes every leaf
split clean (see *Why the record cap* below); its **value is deliberately kept from v23**
([../design/bplus-reshape.md §4.2](../design/bplus-reshape.md)) so the overflow/spill thresholds of
[../design/large-values.md](../design/large-values.md) do not churn. Since **v3**, a record over
the cap is **not** rejected — its large values are **compressed** and/or **spill out-of-line** (the
*Large values* section), so the *stored* record (with pointers) falls back under `RECORD_MAX`. Only a
record that can't be reduced below the cap even after compressing and externalizing every spillable
value remains a write-side `feature_not_supported` (**`0A000`**). At the 8192 default a `K = 1` table
caps a stored record at `RECORD_MAX(1) = (8176−28)/2 = 4074`; the 256-byte fixtures cap a `K = 1` record
at `(240−28)/2 = 106` bytes (a `K = 2` record at 98), which is what makes `overflow_table.jed`'s
~600/300-byte values spill. `K = 0` (index trees) is `RECORD_MAX = (C-12)/2`, so an index entry
key may be up to `(C-12)/2` bytes — the bound the interior separator rules below inherit.

### Leaf node (`page_type = 2`)

Header (`item_count = N`, `next_page = 0`) followed by a **column-major (PAX)** payload. The
`N` records' keys are in **ascending key order**. The payload, in order:

1. **key directory** — `N` big-endian `u32` **end offsets**: `keyOff[i]` is the byte offset one
   past key `i` within the key blob; key `i` is `keyBlob[keyOff[i−1] : keyOff[i]]` with
   `keyOff[−1] = 0`. (`keyOff[N−1]` = total key bytes.)
2. **key blob** — the `N` keys concatenated in ascending order (`Σ key_len` bytes).
3. **column directory** — `K+1` big-endian `u32`: `colStart[c]` is the **absolute payload offset**
   of column `c`'s region; `colStart[K]` is the payload end (the authoritative content length — the
   page body beyond it is zero-fill). For an index tree (`K = 0`) this is the single value
   `colStart[0]` = payload end, and there are no column regions.
4. **column regions**, `c = 0 … K−1` in declaration order. Every region leads with a **flags byte**
   (v24): all bits reserved, written `0`, validated `0` on read (`XX001` if set) — the door a later
   column string dictionary will claim ([../design/json.md §3](../design/json.md)). The rest of the
   region depends on the column's **class** (*Record* above):
   - **Fixed-width column** — a **null bitmap** of `ceil(N/8)` bytes (**MSB-first**: record `i`'s
     NULL bit is `0x80 >> (i mod 8)` of byte `i / 8`; a set bit = NULL — the composite/array bitmap
     convention), then a **dense slot array** of `N × width` bytes: slot `i` is record `i`'s value
     body (the type's fixed-width inline body, **no presence tag**) at byte offset `i·width`; a
     NULL's slot is present but **zero-filled** (never read — the bitmap is the sole authority).
     There is **no value directory** — the stride is the width. This is the vectorizable
     gather/SIMD layout ([../design/bplus-reshape.md §4.1](../design/bplus-reshape.md)).
   - **Variable-width column** — a **value directory** of `N` big-endian `u32` **end offsets**
     (`valOff[i]` = one past value `i`'s bytes within the region's value blob; value `i` spans
     `[valOff[i−1], valOff[i])` with `valOff[−1] = 0`), then the value blob: each **present** value's
     stored codec bytes exactly as v23 (**presence tag `0x00`/`0x02`/`0x03`/`0x04` + body** — the
     *Value codec* / *Large values* forms). A **NULL is a zero-length span** (`valOff[i] ==
     valOff[i−1]`) — no bitmap, no tag byte; a present value's span is never empty (every present
     form is ≥ 1 byte), so span-emptiness is exact. The tag `0x01` **never appears** inside a v24
     leaf region.

A leaf's payload size is `Σ record_size + leafOverhead(N, cols)` (*Page model* above). A
single-column scan reads one contiguous run (`colStart[c] … colStart[c+1]`) — the columnar-scan
enabler, now tag-free and dense for fixed-width columns. To parse: read `N` from the header, the
table's column classes from the catalog, then the directories in order.

### Interior node (`page_type = 3`)

Header (`item_count = N`, `next_page = 0`) followed by the **record-free** payload (v24):

1. **`N + 1` child pointers** — each a big-endian `u32` page index, the roots of the `N + 1`
   subtrees. Child `i` holds keys in `[sep[i−1], sep[i])` (with `sep[−1] = −∞`, `sep[N] = +∞`);
   a key equal to `sep[i]` lies in child `i+1`.
2. **separator directory** — `N` big-endian `u32` **end offsets** into the key blob (the same
   end-offset convention as the leaf key directory).
3. **key blob** — the `N` separator keys concatenated, ascending, each the raw order-preserving
   encoded bytes of a boundary key ([encoding.md](../design/encoding.md)) — descent is
   byte-memcmp, no decode.

An interior node's payload size is `4·(N+1) + 4·N + Σ sep_len = 8·N + 4 + Σ sep_len`. To parse:
read `N` from the header, `N+1` child pointers, the `N`-entry directory, then the blob.

**`N = 0` interior nodes are legal** (one child pointer, an empty directory and blob — payload 4):
the degenerate product of splitting a two-separator interior whose separators are so large no
2-way split can keep a separator on each side (*Fan-out* below; only reachable with near-cap keys —
the `max_sep_table.jed` fixture pins the shape). A 0-key interior routes everything to its single
child, is always underfull, and is merged away by the next rebalance that touches it. The
**minimum-fanout invariant**: any single separator + two pointers always fits —
`12 + sep_len ≤ 12 + RECORD_MAX(0) = 12 + (C−12)/2 ≤ C` — so every interior split can leave at
least one side with a separator.

An empty table has `root_data_page = 0` and no node pages. A one-row table is a single leaf
with `N = 1`. The root may be a leaf (small table) or an interior node (taller tree); it is
distinguished by its `page_type`, never by its index.

### Fan-out: the size-driven split/merge byte contract

Fan-out is governed by **page fit**: a node may hold any number of entries whose serialized form
fits the page payload `C`, and it splits when it would overflow. This makes the node
boundaries — and therefore the on-disk bytes — a deterministic function of the **key set and
the order of mutations** (not of any in-RAM tuning constant). Every core and the Ruby
reference run the identical rules, so the trees are byte-identical. The rules (v24 — restated
for the two node kinds; [../design/bplus-reshape.md §4.2](../design/bplus-reshape.md)):

**A node "fits"** iff its payload size ≤ `C`. The invariant the writer maintains: every
committed node fits; every committed **leaf** is non-empty (`N ≥ 1`); an **interior** node has
`N ≥ 0` separators and `N+1` children (`N = 0` only in the degenerate case above); and every
non-root node is **at least half full** where it can be (`payload ≥ C/2`) — "where it can be"
because a record near `RECORD_MAX` (or a merge abandoned by the interior guard below) may leave
an underfull node, which is correct, just not compact.

**Insert.** Descend to the target leaf (interior nodes only route — `partition_point(sep ≤ key)`),
insert the record in key order, then walk back up: whenever a node overflows (`payload > C`),
**split it 2-way** and hand one separator to the parent (which may then overflow and split, etc.;
a root split grows the height by one, the new root an interior node with one separator).

**Split point (shared machinery).** For an overflowing node with entries `e[0 … N)`, define
`leftpayload(m)` / `rightpayload(m)` as the payload of the would-be left / right node under the
kind's split shape (below), and:

```
m_min      = the smallest m in the kind's range with rightpayload(m) ≤ C
m_max      = the largest  m in the kind's range with leftpayload(m)  ≤ C
m_balanced = the smallest m in the kind's range with 2·leftpayload(m) ≥ payload
m          = right-edge edit ? m_max : clamp(min(m_balanced, m_max), m_min, m_max)
```

A **right-edge edit** means the entry whose insert/replace triggered the rebuild is the node's
last (`index N−1`; for an interior node, the separator the child split just handed up landed at
the right edge) — sequential ascending loads land here every time and keep packing left nodes
~full. The delete path's **merge-overflow** split (below) has no edited position and always takes
the balanced arm. `m_min ≤ m_max` always holds on the insert path (the cap arithmetic below); the
one reachable exception — an interior **merge**-overflow with near-cap separators — **abandons the
merge** instead (below).

> Why two rules: largest-left-fit alone is optimal for ascending appends but degenerates to
> a `[N-2 | 1]` splinter for **any other** insert position — under random-order inserts
> (secondary-index maintenance, a future random pk source) leaves converge on a few-percent
> fill (the 2026-06 benchmark finding, [../design/benchmarks.md](../design/benchmarks.md)).
> The position hint keeps the ascending fast path byte-for-byte unchanged while random
> inserts settle at the classic ~66-70% B-tree fill.

**Leaf split (copy-up).** Range `m ∈ [1, N−1]`. The left leaf gets records `[0, m)`, the right
leaf gets `[m, N)` — **no record leaves the leaf level** — and the parent gains a separator that is
a **copy of `key[m]`** (the right leaf's first key). `leftpayload(m) = Σ_{i<m} record_size(i) +
leafOverhead(m, cols)`; `rightpayload(m)` symmetrically over `[m, N)`. Both sides are non-empty by
the range; both fit by the cap (*Why the record cap*).

**Interior split (push-up).** The median separator **moves up** (it is a pure routing key —
nothing below owns it): the left node gets separators `[0, m)` + children `[0, m]`, separator
`sep[m]` is handed to the parent, the right node gets separators `[m+1, N)` + children
`[m+1, N]`. `leftpayload(m) = 8·m + 4 + Σ_{i<m} sep_len(i)`; `rightpayload(m) = 8·(N−1−m) + 4 +
Σ_{i>m} sep_len(i)`. Range: `m ∈ [1, N−2]` when `N ≥ 3` (both sides keep a separator); when
`N = 2` (only reachable when two separators + pointers overflow `C` — near-cap separators) the
split is **pinned to `m = 1`**: the left node keeps `sep[0]` (fits, by the minimum-fanout
invariant), `sep[1]` moves up, and the right node is the degenerate `N = 0` interior.

**Delete.** Descend to the holding **leaf** (a separator equal to the key just routes right — it
is never deleted or replaced; separators may go stale), remove the record, then walk back up
rebalancing by **merge-then-maybe-split** (no borrow rotation — merge subsumes it):

- A non-root child is **underfull** when its `payload < C/2`.
- When a child is underfull, **merge** it with an adjacent sibling — prefer the **right**
  sibling (`child[i+1]`) if it exists, else the **left** (`child[i-1]`):
  - **Leaf merge:** concatenate the two leaves' records into one leaf `M`; the parent **removes**
    the separator between them and the absorbed child (nothing is pulled down — the separator was
    only a routing copy).
  - **Interior merge:** concatenate `left separators ‖ parent separator ‖ right separators` (the
    separator **pulls down** — the merged node's children on either side need a routing key
    between them) and the two children lists into one node `M`; the parent removes that separator
    and the absorbed child.
  - If `M` fits (`payload(M) ≤ C`): `M` replaces the pair; the **parent loses one key**, so
    the parent may itself become underfull — handled when it returns to *its* parent.
  - If `M` overflows: **split `M` 2-way** by the balanced rule above; the two halves and the new
    separator replace the pair, so the **parent's key count is unchanged**. A **leaf** `M` always
    splits cleanly (`payload(M) ≤ ~1.5·C` — an underfull leaf + a fitting leaf). An **interior**
    `M` (which absorbed a pulled-down separator, `payload(M) < 2·C`) can, with near-cap
    separators, admit **no** valid 2-way split (`m_min > m_max`): then the merge is **abandoned** —
    the two children and the parent separator stay exactly as they were (an underfull interior is
    tolerated; correctness is unaffected, only compactness). The abandon rule is deterministic
    and part of this byte contract.
- **Root collapse:** a leaf root that drains to `N = 0` empties the table
  (`root_data_page = 0`); an interior root that drains to `N = 0` is replaced by its single
  child (height − 1).

**Why the record cap.** Capping a **leaf** record at `RECORD_MAX(K) = (C − max(12, 12+16·K))/2`
makes a two-record leaf never exceed `C`, for every column mix: the worst case is all-variable
columns (`V = K`), where a two-record leaf's overhead is `leafOverhead(2, cols) = 8 + 4·(K+1) +
K·(1 + 8) = 12 + 13·K ≤ 12 + 16·K`, so `2·RECORD_MAX(K) + leafOverhead(2, cols) ≤ (C − 12 − 16K) +
(12 + 13K) = C − 3K ≤ C`; an index leaf (`K = 0`) is exact: `2·(C−12)/2 + 12 = C`. (This fit is
what the v24 end-offset directories buy — with v23's `N+1`-entry prefix sums the same cap would
overflow by `4 + 2K`.) So a leaf overflows only at `N ≥ 3` records and the split point always
lands in `[1, N−1]` with both halves non-empty and fitting. Without the cap, a leaf could overflow
on its **last**, oversized record, forcing an empty sibling or a multi-way spill — both of which
complicate the byte contract across four implementations. Interior nodes carry no records, so the
cap does not bind them; their guarantee is the **minimum-fanout invariant** (one separator + two
pointers always fit — *Interior node* above) plus the pinned `N = 2` degenerate split and the
merge-abandon guard, which together make every interior operation total. The cap buys an
all-2-way scheme at the cost of a tighter (and later-liftable) oversized-row limit.

### Value codec

A row value is encoded behind a named `encode_value`/`decode_value` seam, by column type. All
forms begin with a 1-byte **presence tag**: `0x00` **present-inline-plain**, `0x01` **NULL** (the
tag alone), `0x02` **present-external-plain** (the body is an overflow pointer), `0x03`
**present-inline-compressed**, `0x04` **present-external-compressed** (the `0x02`–`0x04` bodies
are in *Large values* below). Any other tag is `data_corrupted`. `0x00` and `0x01` are **unchanged
from v1**. **Inside a v24 leaf** the tags are partly elided (*Leaf node* above): a fixed-width
column's region stores bare bodies in dense slots (presence from the region bitmap — no tag at
all), and a variable-width column's region stores the tagged forms for present values but encodes
NULL as a zero-length span — so `0x01` never appears in a leaf. This full tagged codec — including
`0x01` — remains normative for every **single-value context**: a catalog constant `DEFAULT`,
overflow-chain content reconstruction, and the recursive composite/array element bodies below.
The present-**inline-plain** body depends on the type:

- **Integers** (`i16`/`i32`/`i64`) — the **same order-preserving bytes as keys**
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

- **`timestamp` / `timestamptz`** — both store an **`i64` microsecond instant** via the
  **same 8-byte order-preserving integer body as `i64`** (the two type codes 9/10 differ in
  semantics, not bytes); the `±infinity` sentinels are the extreme `i64` values.

- **`interval`** — a **fixed 16-byte** body: **`i32` months**, **`i32` days**, **`i64` micros**,
  each **big-endian two's-complement** (plain — **no** sign-flip; this is the **value** codec,
  storing the three raw fields). No length prefix; the three fields are independent (PG's
  representation). interval **is** a key (the separate `interval-span-i128` **key** encoding — the
  16-byte canonical span, [../design/encoding.md §2.10](../design/encoding.md)), so its key body and
  value body genuinely differ: comparison / ordering / the stored key go through the canonical
  128-bit span, never these value bytes.

- **composite** (`type_code 14`, v9 — [../design/composite.md §4](../design/composite.md)) — a
  **null bitmap** of `ceil(field_count / 8)` bytes (**MSB-first**: field *i*'s NULL bit is
  `0x80 >> (i mod 8)` of byte `i / 8`; a set bit = that field is NULL and contributes **zero** body
  bytes), then each **present** field's value-codec body **in declaration order**, written
  **without its own presence tag** (the bitmap carries presence). A field that is itself a
  composite **recurses** (its body is another bitmap + field bodies). A **whole-value-NULL**
  composite is the lone `0x01` tag (no bitmap). The field types come from the column's composite
  type in the catalog, so the body is self-delimiting. Worked example,
  `addr AS (street text, zip i32)`: `('Main', 90210)` → `00`(tag) `00`(bitmap) `00 04 4D 61 69 6E`
  (text body) `80 01 60 62` (i32 body) — an 11-byte body; `('Main', NULL)` → `00`(tag) `40`
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
  elements pay **no** per-element prefix. Worked example, `i32[]`: `{1,2,3}` → `00`(tag) `01`(ndim)
  `00`(flags) `00 00 00 03`(len) `00 00 00 01`(lb) `80 00 00 01 80 00 00 02 80 00 00 03`(three i32
  bodies); `{1,NULL,3}` → `00 01`(HAS_NULLS) `00 00 00 03 00 00 00 01` `40`(bitmap: elem 1 NULL) +
  the bodies for elements 0 and 2.

**Rowid reconstruction (no-PK tables).** The synthetic rowid is allocated from a **monotonic
counter** that is never reused. It is **not stored** — on load it is set to `max(rowid) + 1`
over the table's persisted keys (0 for an empty table), exact because a no-PK key is a bare
`i64` rowid and the rowids issued are `0, 1, 2, …`. Walking the B-tree in key order yields
the rowids in ascending order; the largest is the rightmost leaf's last key.

### Large values (overflow pages + compression, v3)

When a record would exceed `RECORD_MAX`, the engine **compresses** its largest variable-length
values and, where that is not enough, stores them **out-of-line**, so the record falls back under
the cap (the design rationale and decisions are in
[../design/large-values.md](../design/large-values.md) §12/§13). The mechanism:

- **Disposition decision (deterministic, a §8 contract).** Compute the all-inline-plain record
  size `R = key_len + Σ value_size` (the v24 `record_size` basis — *Record* above: a fixed-width
  column contributes its width, a NULL variable-width value 0, a present variable-width value its
  tagged inline-plain encoding); if `R ≤ RECORD_MAX`, every value stays inline-plain — a record
  that fits is **never** compressed or spilled. Otherwise run two passes over the spillable
  values (the variable-width class; fixed-width types never compress or spill), each
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
  first, then each external value's chain is allocated in **(record, column) order** — records in
  ascending key order, and within a record its columns in declaration order: the writer visits
  values in (record, column) order to encode them (allocating chains as it goes), then assembles
  the leaf payload column-major. Only **leaves** allocate chains in v24 — an interior node carries
  no values, so no overflow page ever hangs off an interior separator. This fixes the byte layout
  the goldens pin (`overflow_table.jed`).

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
| `pk_table.jed` | an int PK table whose rows force a **3-node tree** (record-free interior root + two leaves holding **all** the records — the v24 B+tree shape) at page 256 — the load-bearing interior-node + copy-up split proof; includes a NULL value in a row (exercising the fixed-width region's null bitmap + zero-filled slot) |
| `max_sep_table.jed` | **degenerate interior fan-out** ([../design/bplus-reshape.md §4.2](../design/bplus-reshape.md)) — a secondary index over near-`RECORD_MAX(0)` text keys, sized so two separators + pointers overflow `C` and the pinned `N = 2 → m = 1` interior split produces a legal **`N = 0` interior node**; pins the minimum-fanout invariant and the degenerate split shape |
| `text_table.jed` | a text column — the value codec's text branch; empty string, embedded quote, multi-byte + astral chars, a NULL (single leaf) |
| `varchar_table.jed` | a `varchar(n)` column — the v22 text-column `u32 varchar_max_len` typmod slot (a bounded `varchar(5)` beside an unbounded `text` column, in one catalog); stored values are within-limit text |
| `bool_table.jed` | a boolean column — the `bool-byte` value branch (single leaf) |
| `bool_pk_table.jed` | a **boolean PRIMARY KEY** (the second non-integer stored key — the `bool-byte` key, `00`/`01`, no presence tag) + a nullable boolean column; rows in key order (false then true) |
| `decimal_table.jed` | a `decimal` column — the decimal branch + per-column `numeric(p,s)` typmod; positive/negative/zero/multi-group/NULL |
| `bytea_table.jed` | a bytea column — the bytea branch; empty value, embedded `0x00`, a high byte, a NULL |
| `uuid_table.jed` | a **uuid PRIMARY KEY** (the first non-integer stored key — the §8 cross-core key-path proof) + a nullable uuid column |
| `default_table.jed` | columns with a **constant** `DEFAULT` — the `has_default` flag (bit2) + default value codec written after the typmod |
| `default_expr_table.jed` | columns with an **expression** `DEFAULT` (v8) — the `default_is_expr` flag (bit3) + the expr-text after the typmod: a `uuid DEFAULT uuidv7()`, an `i32 DEFAULT 1 + 1`, beside a plain no-default column and a constant-default column (bit2) in the same catalog (empty table — the row eval is covered by the conformance corpus) |
| `timestamp_table.jed` | a timestamp column — the 8-byte i64 branch; epoch, pre-1970, BC-era, `±infinity`, NULL |
| `timestamptz_table.jed` | a timestamptz column — the same 8-byte branch under type code 10 |
| `interval_table.jed` | an interval column — the fixed 16-byte branch (i32 months ‖ i32 days ‖ i64 micros, big-endian); a positive multi-field value, a negative value, the zero interval, a months-only/`'1 mon'` value (vs a `'30 days'` value that is span-equal but byte-distinct), and a NULL (single leaf) |
| `nopk_table.jed` | a no-PK table — the stored synthetic `i64` rowid key |
| `composite_pk_table.jed` | a **composite PRIMARY KEY** (`i32` ‖ `i16`) — the concatenated key encoding (encoding.md §2.3) + the v5 `pk_ordinal` list; negative first component and tie-breaking second |
| `index_table.jed` | **secondary indexes** (v5) — a table whose PK list order differs from declaration order (`PRIMARY KEY (b, a)` — the lifted narrowing), one single-column index over a **nullable** column holding a NULL (the encoding.md §2.2 presence tag in stored index order, NULL last) and one auto-named two-column index; empty-payload index records |
| `unique_table.jed` | **unique indexes** (v6) — the per-index `index_flags` byte: a `UNIQUE` constraint's auto-named `t_v_key` (over a nullable column holding two NULLs — *NULLS DISTINCT* stored side by side), a named two-column constraint, a `CREATE UNIQUE INDEX`, and one plain index (`index_flags` 0) in the same catalog |
| `gin_array_table.jed` | **GIN inverted index** (v13) — the per-index `index_kind` byte: a `USING gin` index over an `i32[]` column (rows with multi-element, duplicate-element, empty, and NULL arrays — exercising term dedup and the zero-entry cases; entries are `encode(elem) ‖ storage-key`, empty payload — [../design/gin.md §4](../design/gin.md)) beside one ordinary ordered index (`index_kind = 0`) in the same catalog |
| `gin_uuid_table.jed` | **GIN over a non-integer element type** (no version bump — `uuid` is a fixed-width key encoding already on disk) — a `USING gin` index over a `uuid[]` column: each GIN term is the element's 16-byte `uuid-raw16` key encoding, so entries are `encode_uuid(elem) ‖ storage-key` (empty payload — [../design/gin.md §3/§4](../design/gin.md)). Same row shape as `gin_array_table` (term dedup, empty/NULL arrays, a NULL element), pinning that a uuid-element GIN serializes byte-identically across cores |
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
