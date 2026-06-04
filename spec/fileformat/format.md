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
`rust-bytes == golden == go-bytes` by construction, so each core reads the other's output.

## Step-5b scope

This is the **whole-image** format: a commit serializes the entire database into one byte
image (data is RAM-first — CLAUDE.md §9). Deliberately **deferred** until `UPDATE`/`DELETE`
create pressure (CLAUDE.md §11): incremental copy-on-write, free-list / page reclamation,
and B-tree interior pages. The double-meta page and root pointer below are the
forward-looking hooks the live commit model (storage.md §4) will use.

## Conventions

- **All multi-byte integers are big-endian, unsigned** unless stated, consistent with the
  key encoding's MSB-first rule ([encoding.md](../design/encoding.md)).
- **Reserved fields are written as zero and required to be zero on read.** A nonzero
  reserved field, bad magic, unsupported version, or bad checksum is a structured
  `data_corrupted` (SQLSTATE `XX001`) error.
- A page index of **0** means "none"/absent (`next_page`, `root_data_page`); real catalog
  and data pages live at index ≥ 2, so 0 is an unambiguous sentinel.

## Page layout

The file is a flat array of fixed-size **pages**; the page size is a format parameter
recorded in the meta page (**default 8192**; the golden fixtures use **256** so the hex
stays reviewable). Page roles:

| page index | role |
|---|---|
| 0 | meta slot 0 |
| 1 | meta slot 1 |
| 2 … | catalog page chain (root = page 2) |
| … | data page chains (one per non-empty table) |

`page_count = file_size / page_size`. Every page is zero-filled to exactly `page_size`.

## Meta page (pages 0 and 1)

Two slots for torn-write-safe atomic publish (the bbolt model — storage.md §4). Fields:

| offset | size | field |
|---|---|---|
| 0  | 4 | `magic` = `4A 45 44 42` (ASCII `JEDB`, for the engine `jed`) |
| 4  | 2 | `format_version` (u16) — current = `1` |
| 6  | 2 | reserved (0) |
| 8  | 4 | `page_size` (u32) |
| 12 | 8 | `txid` (u64) — commit counter; the highest valid slot wins on open |
| 20 | 4 | `root_page` (u32) — catalog root, = `2` |
| 24 | 4 | `page_count` (u32) |
| 28 | 4 | reserved (0) |
| 32 | 4 | `crc32` (u32) — CRC-32/IEEE over meta bytes `[0, 32)` (excludes this field and the zero-fill tail) |

`page_size` lives at a fixed offset so a reader can learn it before it knows where page 1
begins (page 1 starts at byte `page_size`).

**Checksum.** CRC-32/IEEE (reflected, polynomial `0xEDB88320`, init `0xFFFFFFFF`, final XOR
`0xFFFFFFFF`) — the standard zlib CRC32, hand-rolled identically in every core (no runtime
dependency). Pinned by the vector `crc32("123456789") == 0xCBF43926`.

**Writing (whole image).** A whole-image commit writes the **same current meta into both
slots** (a fresh image has no distinct prior version). The slot-alternation that storage.md
§4 describes belongs to the future *incremental* commit path, not here.

**Opening (slot selection).** Validate each slot independently (magic, `format_version`,
reserved == 0, `crc32`). Choose the **valid** slot with the **highest `txid`**; on a tie,
slot 0. Exactly one valid → use it (torn-write fallback). Neither valid → `data_corrupted`.

## Page header (catalog and data pages, 12 bytes)

| offset | size | field |
|---|---|---|
| 0 | 1 | `page_type` (u8) — `1` = catalog, `2` = data |
| 1 | 1 | reserved (0) |
| 2 | 2 | reserved (0) |
| 4 | 4 | `item_count` (u32) — entries/records on this page |
| 8 | 4 | `next_page` (u32) — next page of this chain, or 0 |

The payload (entries/records) follows immediately and is zero-filled to `page_size`.

## Catalog (page chain rooted at page 2)

The catalog is a chain of `page_type = 1` pages. Tables are emitted in **ascending order of
the lowercased table name** (the engine stores tables in a hash map keyed by lowercased
name; sorting by that key removes any iteration-order leak — CLAUDE.md §8; names are unique
after lowercasing, so there are no ties). Each page's `item_count` is the number of table
entries it holds; the total table count is the sum across the chain.

Each **table entry**:

| field | encoding |
|---|---|
| `name_len` | u16 |
| `name` | `name_len` bytes UTF-8 (original case — round-trips what the user typed) |
| `col_count` | u16 |
| per column (×`col_count`): | |
| &nbsp;&nbsp;`col_name_len` | u16 |
| &nbsp;&nbsp;`col_name` | UTF-8 (original case) |
| &nbsp;&nbsp;`type_code` | u8 (stable, see below) |
| &nbsp;&nbsp;`flags` | u8 — bit0 `primary_key`, bit1 `not_null`, bit2 `has_default` (reader trusts the bits) |
| &nbsp;&nbsp;`precision` | u16 — **only present when `type_code == 6` (decimal)**; `0` = unconstrained |
| &nbsp;&nbsp;`scale` | u16 — **only present when `type_code == 6` (decimal)** |
| &nbsp;&nbsp;`default` | value-codec bytes — **only present when `flags` bit2 (`has_default`)**; written *after* the typmod |
| `root_data_page` | u32 — first data page of this table's chain, or 0 if it has no rows |

Columns are emitted in declaration order.

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

A column's collation is **not** stored: there is one collation (`C`) for all text this slice
(../design/types.md §11). A per-column collation field is a forward extension that will claim a
spare `flags` bit or a new field under a `format_version` bump when multi-collation lands.
`bytea` has no collation (it is raw bytes, not text), so the same field-free encoding applies.

A **decimal** column carries a **typmod** (the `numeric(p,s)` precision/scale) that constrains
future writes, so it **must** persist. It is appended to the column entry **only when
`type_code == 6`** — two extra big-endian `u16`s, `precision` then `scale` — so non-decimal
column entries are byte-unchanged (existing int/text fixtures are untouched). `precision == 0`
means **unconstrained** `numeric` (no typmod; `scale` is then `0` and ignored); a constrained
`numeric(p,s)` stores `precision = p` (`1 … 1000`) and `scale = s`. The reader, having read
`type_code == 6`, reads the two `u16`s; for any other type code it reads neither.

A column with a **`DEFAULT`** (../design/constraints.md §2) persists its pre-evaluated default
value. When `flags` **bit2 (`has_default`)** is set, the default is appended **after** the typmod
(so a decimal-with-default reads typmod then default), encoded with the **same value codec rows
use** (presence tag + body — see below): a present default is `0x00` + the type body, a
`DEFAULT NULL` is the lone `0x01`. The field is presence-gated, so a column without a default is
byte-unchanged — every fixture predating defaults is untouched. This is an **additive,
backward-compatible extension** at `format_version == 1`, exactly like the decimal typmod: an old
file (no `has_default` bit anywhere) still loads. The one asymmetry to note: a v1 file that *does*
carry a default is not readable by a core built before defaults existed — the writer's surface
grew, the version did not. The reader keys entirely off bit2; a column whose bit2 is clear reads
no default bytes.

## Data pages (one chain per non-empty table)

A chain of `page_type = 2` pages rooted at the table's `root_data_page`. Records are emitted
in **ascending encoded-key order** (the store iterates in key order; the reader rebuilds it
by inserting in file order). Each **record**:

| field | encoding |
|---|---|
| `key_len` | u16 |
| `key` | `key_len` bytes — the row's storage key, exactly as the engine encoded it |
| `payload` | each column's value, in declaration order, via the value codec |

The key is **stored, not derived**: a table without a primary key uses a synthetic
`int64` rowid that is not reconstructable from row data, so the key bytes are persisted
verbatim. There is no per-record payload length — the reader walks the columns in declaration
order and takes each value's width from its type: fixed for the integers, and for `text` /
`bytea` from the `u16` length the value carries (see the value codec below).

**Rowid reconstruction (no-PK tables).** The synthetic rowid is allocated from a
**monotonic counter** that is never reused (so a `DELETE` followed by an `INSERT` cannot
collide with a freed key). The counter is **not stored** — on load it is set to
`max(rowid) + 1` over the table's persisted keys (0 for an empty table), which is exact
because a no-PK key is a bare `int64` rowid and the rowids issued are `0, 1, 2, …`. This
needs no format change; it is a pure load-time derivation.

### Value codec

A row value is encoded behind a named `encode_value`/`decode_value` seam, by column type. All
forms begin with a 1-byte **presence tag** (`0x00` present, `0x01` NULL); a NULL is the tag
alone. The present-value body depends on the type:

- **Integers** (`int16`/`int32`/`int64`) — the **same order-preserving bytes as keys**
  ([encoding.md §2.1](../design/encoding.md)): fixed-width big-endian, sign-bit flipped. The
  sign-flip is unnecessary for a stored value but harmless, and reusing the key codec keeps one
  verified, byte-pinned ([../encoding/integers.toml](../encoding/integers.toml)) integer codec.

- **`text`** — the seam **diverges** here (its whole purpose): a stored text value uses a
  **compact length-prefixed** form, NOT the order-preserving key encoding (encoding.md §2.4),
  because a stored value never needs to sort. The body is a **`u16` byte-length** (big-endian,
  like every other length in this format) followed by exactly that many **UTF-8 bytes** (the
  `C` collation's bytes, verbatim — no escaping, no terminator). The empty string is
  `00`(tag)`00 00`(len), zero content bytes. A value whose UTF-8 length exceeds `0xFFFF` is a
  write-side `feature_not_supported` (`0A000`) — and in practice such a value also exceeds a
  page, tripping the oversized-item rule below. The reader reads the tag, then (if present) the
  `u16` length, then that many bytes — the only value whose width is not fixed by the column
  type alone.

- **`boolean`** — a single **`bool-byte`** body: `00` false, `01` true (any other byte is
  `data_corrupted`). A present boolean is two bytes total — `00`(tag)`00`/`01`(value); a NULL
  boolean is the tag alone. This is the same order-preserving `bool-byte` the key encoding uses
  (false `<` true), but as a stored value it never needs to sort (a boolean PRIMARY KEY is
  rejected this slice — types.md §9).

- **`decimal`** — also self-describing (its whole point is exactness, not ordering), so like
  text it uses a compact codec, **not** the order-preserving key encoding (encoding.md §2.5).
  The present body is, in order: a **`u8` `flags`** (bit0 = sign, `1` = negative; bits 1–7
  reserved `0`); a **`u16` `scale`** (big-endian, the value's display scale `s`); a **`u16`
  `ndigits`** (big-endian, the number of base-10⁴ digit groups in the coefficient); then
  **`ndigits` × `u16`** (big-endian) coefficient groups, **most-significant group first**, each
  `0 … 9999` (base 10000 — PG's on-disk digit size, and `u16`-clean / reviewable in the
  fixtures). The in-core base-10⁹ coefficient is regrouped to base-10⁴ on write and back on
  read (a pure power-of-ten split). **Canonical zero** is `flags=0, scale=s, ndigits=0` (no
  groups; the sign bit is forced `0` when the coefficient is zero — there is no negative zero),
  so a given scale has exactly one byte form for zero. The reader reads the tag, then flags,
  scale, ndigits, then that many `u16` groups — its width is fully self-describing. A value
  exceeding the page (impossible under the 1000-digit cap for a 256-byte fixture only at the
  extreme) trips the oversized-item rule below.

- **`bytea`** — the **same compact length-prefixed** form as text (a stored value never needs
  to sort, so not the order-preserving key encoding, encoding.md §2.6): a **`u16` byte-length**
  (big-endian) followed by exactly that many **raw bytes**. The one difference from text is that
  the bytes are written and read **verbatim with no UTF-8 validation** — any byte, including
  `0x00`, is allowed. The empty value is `00`(tag)`00 00`(len). The `> 0xFFFF` and oversized-item
  rules are identical to text.

There is no per-record payload length: the reader walks the columns in declaration order,
deriving each value's width from its type (fixed for integers and the 1-byte boolean;
self-describing via the `u16` length for text and bytea, and via `ndigits` for decimal).

## Packing and page allocation (must be byte-identical across cores)

Records and table entries are variable-length, so the packing rule is pinned: **greedily**
append an item to the current page; when the next item's full on-disk size would exceed
`page_size − 12` (the page header), start a new page and link it via `next_page`. Pages of a
chain are **contiguous and ascending**; `next_page` is the literal next index, and the last
page in a chain has `next_page = 0`.

Allocation order within the image: meta (0, 1), then the catalog chain (from page 2), then
each non-empty table's data chain in table-sort order. A table's `root_data_page` is the
first index of its data chain (or 0 if it has no rows).

**Oversized item.** A single table entry or record that does not fit one page is **not**
split (no overflow pages in step-5b) — it is a write-side `feature_not_supported`
(`0A000`). Fixtures stay within one item per... within the page limit.

## Edge cases

- **Empty database** (no tables): one catalog page with `item_count = 0`; `page_count = 3`
  (two meta slots + the catalog page).
- **Empty table** (no rows): `root_data_page = 0`; no data pages.

## Fixtures

[fixtures/](fixtures/) holds byte-exact goldens at `page_size = 256`, generated and checked
by the independent Ruby reference in [verify.rb](verify.rb) (run via `rake verify`):

| fixture | exercises |
|---|---|
| `empty_db.jed` | zero tables; catalog `item_count = 0` |
| `one_table_empty.jed` | one table, zero rows (`root_data_page = 0`) |
| `pk_table.jed` | a PK table with rows spanning **>1** data page; a NULL value in a row |
| `text_table.jed` | a text column — the value codec's text branch (`u16` len + UTF-8 bytes); empty string, embedded quote, multi-byte + astral chars, a NULL |
| `bool_table.jed` | a boolean column — the value codec's `bool-byte` branch (`00` false / `01` true) and a NULL boolean |
| `decimal_table.jed` | a `decimal` column — the value codec's decimal branch (flags + scale + base-10⁴ groups), the per-column `numeric(p,s)` typmod, and positive/negative/zero/multi-group/NULL |
| `bytea_table.jed` | a bytea column — the value codec's bytea branch (`u16` len + raw bytes); empty value, embedded `0x00`, a high byte, a NULL |
| `default_table.jed` | columns with `DEFAULT` — the `has_default` flag (bit2) + the default value codec written after the typmod; an int/text/decimal default, a `DEFAULT NULL`, a NOT NULL column with a default, a plain no-default column |
| `nopk_table.jed` | a table with no PK — exercises the stored synthetic `int64` rowid key |
| `torn_meta_slot0.jed` | slot 0 checksum corrupted → loader falls back to slot 1 |
| `torn_meta_slot1.jed` | slot 1 checksum corrupted → loader falls back to slot 0 |

The "highest `txid` wins" selection (vs. the torn-write fallback) is covered by per-core
unit tests that craft two valid slots with differing `txid`, since a fresh whole-image write
gives both slots the same `txid`.
