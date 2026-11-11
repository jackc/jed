# Leaf pages: structural size reductions (proposal)

**Status: proposal. Steps 1 to 6 are prototyped in the Go core only**, behind `JED_LEAF_NARROW=on`,
`JED_LEAF_ELIDE_KEY=on`, `JED_LEAF_FIXED_KEY=on`, `JED_LEAF_BARE_STRING=on`,
`JED_LEAF_KEY_PREFIX=on` and `JED_LEAF_FOR=on`. Steps 5 and 6 are encoding steps: they make a leaf's
size depend on its contents (§7, §8).

**Correction to §4 to §7.** Until step 6 the prototype's `leafOverhead` charged each elided key
column a value directory (`1 + 2·N` bytes) it does not have, so table leaves filled 1% to 4% short.
The results in §4 to §7 were measured with that error. With it fixed, steps 1 to 5 give a 122.4 MB
file (15,596 leaves), not 126.1 MB; §8 compares against the corrected figure. Nothing here is in `format.md`, the Ruby reference, or the Rust and TS cores, and the prototype does not bump
the format version.

## 1. Where the bytes are

A leaf page ([../fileformat/format.md](../fileformat/format.md) *Leaf node*) spends bytes on
structure that carries no information the page does not already hold. Measured on the 188 MB test
database (1.3M rows over five tables and five indexes, 8 KiB pages, compacted; 158.2 MB of leaf
payload):

| component | bytes | share of leaf payload |
|---|---|---|
| primary-key columns stored as values as well as in the key | 14.9 MB | 9.4% |
| key directory, where every key of the tree has one length | 9.6 MB | 6.1% |
| value directories: 4-byte entries where 2 suffice | 5.6 MB | 3.5% |
| `text`/`bytea` u16 length prefix (the directory span gives the length) | 3.4 MB | 2.1% |
| presence tag on variable-width values, in pages with only plain values | 2.6 MB | 1.6% |
| key directory, variable-length keys: 4-byte entries where 2 suffice | 0.8 MB | 0.5% |
| column directory: 4-byte entries where 2 suffice | 0.2 MB | 0.2% |
| **total** | **37.1 MB** | **23.4%** |

For scale: keys are 58.7 MB of the leaf payload (37%). Shrinking the keys themselves (a shared
prefix per page) is an encoding step, outside this document.

## 2. The constraint

The split rules measure a leaf by `Σ record_size + leafOverhead(N, schema)` (format.md *Fan-out*).
A saving shrinks the file only if it lets more records into a page, so it must show up in that
formula; and the formula must stay a function of the records and the schema, or a split could no
longer be planned from record weights. So each step below is admitted only in a form that is fixed
by the schema, never by what one page happens to contain:

- Directory width is fixed by the page-size limit (64 KiB): static.
- A key's length is fixed when the key's column types are all fixed-width: static, from the schema.
- A length prefix is redundant for every inline `text`/`bytea` value: static.
- A primary-key column's value is derivable from the key when the key encoding is invertible for its
  type: static, from the schema.
- The presence tag is the exception. It distinguishes inline, compressed, and external forms, and a
  NULL from an empty string. Dropping it "when a page has only plain values" depends on the page.
  A static form needs a null bitmap for variable columns and a per-region list of the non-plain
  values, each charged to its own record. Its net saving is smaller than the 1.6% above.

## 3. Step 1: 16-bit directories (prototyped)

A narrow leaf starts with a **layout byte** (`1`), then the v50 payload with every directory entry
(key directory, column directory, value directories) a big-endian `u16`. A v50 leaf's first payload
byte is the high byte of a `u32` offset and is always 0, so the layout byte tells the two apart and
both read. `record_size` is unchanged, and

```
leafOverhead(N, cols) = 1 + 2·N + 2·(K+1) + F·(1 + ceil(N/8)) + V·(1 + 2·N)
```

`RECORD_MAX` is unchanged.

**Result** on the test database:

| | v50 | narrow |
|---|---|---|
| compacted file | 187.7 MB | 168.8 MB |
| leaf pages | 23,910 | 21,501 |
| leaf payload | 150.8 MB | 139.9 MB |
| bytes written during the bulk load | 6,444 MB | 6,251 MB |

Query timings did not move outside run-to-run noise (resident and first-touch scans, point lookups,
single-row commits; one run each).

**What was run**, Go core, switch on: the unit tests; the conformance corpus; `mise run sim:sweep 1..60
all go` (1,320 traces), alone and together with the deferred-interior and CRC-32-manifest switches;
the randomized comparison test. With the switch off the unit tests pass.

**What fails with the switch on, as a layout change must:**

- golden byte comparisons (`TestWriteMatchesGoldens`, the encrypted goldens, one round-trip test);
- three unit tests that pin page counts or directory offsets;
- 8 of 752 corpus files in memory mode: seven pin a cost or an `EXPLAIN` estimate that counts pages
  (each is 1 to 3 units lower), and `resource/storage_memory.test` pins a storage-limit trip point to
  the exact page. The disk-mode corpus pass was not reached.

## 4. Step 2: primary-key columns (prototyped)

A table column is **key-derivable** when it is a primary-key member whose value body is, byte for
byte, its key encoding: the integers, `date`, `timestamp`, `timestamptz` (whose value body is already
the order-preserving sign-flipped form), `boolean`, and `uuid`. Floats and `interval` are excluded,
because their keys canonicalize. The leading run of key-derivable members of the primary key is
elided; the run stops at the first member that is not, since key offsets after it are not fixed.

- **Page.** The layout byte becomes a set of flags: bit 0 narrow directories, bit 1 key columns
  elided. An elided column keeps its column-directory entry and has an empty region.
- **Weights.** An elided column contributes 0 to `record_size`, and `leafOverhead` counts it only in
  the column directory. Both remain functions of the schema.
- **Write.** The encoder checks, for every row, that the key bytes equal the value body it is not
  storing, and refuses the page otherwise.
- **Read.** The parser rebuilds the column's dense slots from the keys when it parses the page, so
  every reader above it (row decode, lazy decode, columnar scans) is unchanged. The rebuilt slots are
  a separate allocation: a resident leaf is larger than its page by the bytes the page saved.
- A store's column types carry the key offset (`colType.keyAt`), set when the table is registered, so
  every caller of the leaf parser has it. It is set whether or not the switch is on, so any process
  reads an elided page.

**Result**, with both switches on:

| | v50 | narrow | narrow + key columns |
|---|---|---|---|
| compacted file | 187.7 MB | 168.8 MB | 157.3 MB |
| leaf pages | 23,910 | 21,501 | 20,039 |
| leaf payload | 150.8 MB | 139.9 MB | 125.7 MB |

Scans did not move outside noise, first-touch scans (which parse every leaf, and so pay the rebuild)
included. Point lookups looked slower in the first run (primary-key lookup 1.7 → 2.0 µs); that was
noise. A resident lookup does not parse a leaf (0.00 leaf parses per lookup after the first pass,
counted), so the rebuild is not on its path, and two alternating rounds of nine passes each gave
medians of 1.67 against 1.70 µs for the primary-key lookup and 4.77 against 4.69 µs for the index
lookup in the quieter round, with the other round differing by more than that between identical
configurations.

**What was run**, Go core, both switches on: the unit tests (the same layout-pinned failures as step
1, no others); `mise run sim:sweep 1..100 all go` (2,200 traces), and 1,320 more together with the
deferred-interior and CRC-32-manifest switches; the randomized comparison test. The memory-mode corpus
fails 15 of 752 files, all pins: twelve costs or `EXPLAIN` estimates that count pages, and three
resource trip points (`storage_memory`, and two query-memory tests whose limit is no longer reached
because staged records are smaller). No file returned wrong rows. The disk-mode pass was not reached.

## 5. Step 3: no key directory for fixed-length table keys (prototyped)

A table's storage keys all have one length when every primary-key member is a scalar with a
fixed-width key encoding (the integers, `boolean`, `uuid`, the date/time types, floats, `interval`),
and when the table has no primary key (the 8-byte synthetic rowid). Such a table's leaves set layout
bit 2, follow the layout byte with the key length as a `u16`, and carry no key directory: key `i` is
at `i · key_len` in the key blob.

- The page names its own key length, so a reader needs no schema. The writer takes the length from
  the schema and refuses a page holding a key of another length.
- `leafOverhead` replaces the key directory's `2·N` with 2. `record_size` is unchanged.
- **Index trees keep their directory.** An index entry's key is a nullable slot per element, and a
  NULL slot is one byte, so the length is fixed only while every key column is `NOT NULL`, which
  `ALTER TABLE … DROP NOT NULL` can end. Taking this for indexes needs that statement to rebuild the
  index (its leaves were planned with the smaller overhead). On the test database the index trees
  whose keys happen to share one length hold 2.2 MB of key directory.

## 6. Step 4: inline strings without a length (prototyped)

An inline `text`, `bytea` or `json` value in a leaf is `0x00`, a `u16` length, and the bytes. The
value directory already gives the span, so the length is redundant. The new form is the presence
tag `0x05` followed by the bytes; the rest of the span is the payload.

- Each value names its own form, so both read, and no layout bit is needed.
- `record_size` counts such a value as `1 + len` instead of `3 + len`; the disposition plan
  (compress, then externalize) uses the same number. `RECORD_MAX` is unchanged.
- An inline value read from a page stays a view of the page in either form. The reader records which
  form it came from, and the writer emits whichever form the process writes.
- The compressed and external forms are unchanged. The inline-compressed form's `u16` block length
  is redundant in the same way and was left alone.
- The filter that compares stored bytes in place (`col = 'literal'`) reads both forms.

**Result**, all four switches on:

| | v50 | steps 1–2 | steps 1–4 |
|---|---|---|---|
| compacted file | 187.7 MB | 157.3 MB | 151.2 MB |
| leaf pages | 23,910 | 20,039 | 19,259 |
| leaf payload | 150.8 MB | 125.7 MB | 120.0 MB |
| bytes written during the bulk load | 6,444 MB | 6,215 MB | 6,180 MB |

Scans, single-row commits and the bulk load did not move outside noise (two alternating runs of each
configuration). Point lookups: five alternating rounds of nine passes each gave a best-pass
primary-key lookup of 1.37 to 1.55 µs with steps 1–4 against 1.39 to 1.51 µs with steps 1–2. One
round of the new layout ran at half speed on every query, index lookups included, and is excluded as
a whole-process effect.

**What was run**, Go core, all four switches on: the unit tests (the same layout-pinned failures as
steps 1 and 2, no others); `mise run sim:sweep 1..100 all go` (2,200 traces), and 1,320 more together
with the deferred-interior and CRC-32-manifest switches; the randomized comparison test with every
switch on. The memory-mode corpus fails 16 of 752 files, all pins: thirteen costs or `EXPLAIN`
estimates and the three resource trip points. No file returned wrong rows. The disk-mode pass was not
reached. With the switches off the unit tests pass.

## 7. Step 5: shared key prefix (prototyped)

The first step whose saving depends on a page's contents, and so the first to change the split rule.

**Page.** Layout bit 3. After the layout byte (and the fixed key length, when present) come a `u16`
prefix length `p` and the prefix; the key directory and key blob then hold each key without its
first `p` bytes (a fixed-length tree's keys are `key_len − p` bytes each, still with no directory).
The prefix is the common prefix of the leaf's first and last key, which every key between them
shares because keys are sorted. A leaf stores it only when that saves bytes (`N·p > p + 2`);
otherwise the bit is clear and the leaf is as in steps 1 to 4.

**Size.** `record_size` and `leafOverhead` are unchanged. A leaf's payload is

```
Σ record_size + leafOverhead(N, shape) − saving(N, first key, last key)
saving = N·p − p − 2 when the prefix is stored, else 0
```

Every place that measures a leaf uses this: the overflow test, the split-point scan (each candidate
half is measured with its own prefix), and the underfull test that triggers a merge. Consequences:

- The size is a function of the weights and of two keys, so a split is still planned without
  encoding anything. The split-point scan's requirement holds: a half's payload is monotone in the
  split position, since a key added to a half adds at least its suffix and the half's prefix can
  only shorten.
- A two-record leaf still fits (the saving is never negative), so `RECORD_MAX` is unchanged.
- An insert at either end of a leaf can shorten the prefix, growing the page by more than the new
  record. A delete can only shrink it.
- Two underfull leaves can merge into a leaf that overflows, because the merged prefix is shorter
  than either's. The existing merge path already splits an overflowing merge result.
- The choice is deterministic: the prefix is the longest common one, stored exactly when it saves
  bytes.

**Read.** The parser keeps the prefix and the suffixes as views of the page. A leaf search compares
the probe with the prefix once and then with suffixes, a key copied out for a mutation is assembled
from the two parts, and a record's weight adds the two lengths, so point lookups and inserts never
materialize the keys. Any other reader of a key (range bounds, scans that return keys, serialization
of a clean leaf) rebuilds all of the leaf's keys into one allocation on first use, and the leaf keeps
them; such a leaf is larger than its page by the bytes the prefix saved.

**Result**, all five switches on:

| | v50 | steps 1–4 | steps 1–5 |
|---|---|---|---|
| compacted file | 187.7 MB | 151.2 MB | 126.1 MB |
| leaf pages | 23,910 | 19,259 | 16,066 |
| interior pages | | 95 | 69 |
| leaf payload | 150.8 MB | 120.0 MB | 101.5 MB |
| bytes written during the bulk load | 6,444 MB | 6,180 MB | 5,794 MB |
| bytes written per single-row update commit | | 107,151 | 100,765 |

The 18.5 MB of payload saved is less than the 21.7 MB estimated, because index leaves are filled by
random inserts and stay about 60% full: the estimate counted bytes, and the file shrinks only by
whole pages. Index trees gain the most (`orders_status_created` 1,364 → 645 leaves,
`orders_customer` 939 → 540); a table keyed by a random token (`sessions`) gains nothing.

Timings, two alternating runs of each configuration, plus four alternating rounds of the lookup test:

- Scans (resident and first-touch) and single-row commits did not move outside noise.
- Primary-key lookup, best pass over four rounds: 1.39 to 1.54 µs against 1.41 to 1.49 µs. Index
  lookup: 3.47 to 4.06 µs against 3.54 to 4.02 µs.
- **The bulk load is about 12% slower**: 46.9 to 48.7 s against 42.4 to 42.8 s, in every run. The
  cause is the density itself. The load inserts 500 rows per commit, and index inserts land on random
  leaves; a commit decodes, re-encodes and walks for overflow pointers every record of every leaf it
  touches. With the prefix, 6% fewer leaves were encoded during the load (687,580 against 729,373) but
  they held 20% more records (226.1M against 188.9M), and those three per-record passes are about
  19 s of the load's 53 s of CPU time. Materializing keys at parse cost a further 2 s in a first version and was removed
  (see *Read*); removing it did not change the load time measurably. Any change that puts more
  records in a leaf has this cost on a random-insert load, as a larger page size would. Steps 1 to 4 did not show it measurably;
  why not was not investigated.

**What was run**, Go core, all five switches on: the unit tests (the layout-pinned failures of steps 1
to 4, plus three tests that require a tree to span several pages and no longer get one);
`mise run sim:sweep 1..100 all go` (2,200 traces), and 1,320 more together with the deferred-interior
and CRC-32-manifest switches; the randomized comparison test with every switch on. The corpus fails
19 of 752 files in memory mode and 18 in disk mode, all pins: costs, `EXPLAIN` estimates and resource
trip points. Against steps 1 to 4 the new failures are two costs and one `EXPLAIN` estimate. With the
switches off the unit tests pass.

**Not done.** Interior pages keep whole separators. Range-bound searches and scans still rebuild a
leaf's keys. The integrity check does not verify that a stored prefix is the longest one.

## 8. Step 6: narrow integers (prototyped)

A fixed-width column whose value body is the big-endian image of its order (the integers, `date`,
`timestamp`, `timestamptz`; not floats, `boolean`, `uuid` or `interval`, and not an elided key
column) may be written as a base plus narrow offsets.

**Page.** The region's flags byte is `1`. After the null bitmap come the base (the smallest present
value, at the column's width `w`), a `u8` offset width `bw` (0, 1, 2 or 4, below `w`: the narrowest
that holds largest − smallest), and `N` offsets of `bw` bytes; a NULL's offset is 0. `bw = 0` is a
column with one value. The region is written this way only when smaller: `N·(w − bw) > w + 1`.

**Size.** As in §7, with one more term per such column:

```
saving = N·(w − bw) − (w + 1) when positive, else 0
```

- The split-point scan needs each column's smallest and largest present value over every prefix and
  every suffix of the leaf's rows: two passes over the rows per column, done only when a leaf
  overflows. Each candidate half is then measured in O(columns). A half's payload stays monotone in
  the split position: a row added to a half adds at least its offsets, and `bw` can only widen.
- The overflow and underfull tests first compare the size without any content-dependent saving,
  and compute the savings only when that does not decide.
- One insert can widen `bw` for every row of a column, growing the page by up to `N·(w − 1)` bytes.
  The leaf then splits, as for any overflow.

**Read.** The parser rebuilds the dense full-width slots into one allocation per column, so every
reader (row decode, lazy decode, columnar scans, in-place filters) is unchanged. A resident leaf is
larger than its page by the bytes saved. Not done: reading the narrow slots in place, and comparing a
scan's literal against `literal − base`.

**Result**, all six switches on, against steps 1 to 5 with the overhead correction:

| | steps 1–5 (corrected) | steps 1–6 |
|---|---|---|
| compacted file | 122.4 MB | 114.3 MB |
| leaf pages | 15,596 | 14,562 |
| leaf payload | 101.5 MB | 93.8 MB |
| bytes written during the bulk load | 5,779 MB | 5,753 MB |

The 7.7 MB of payload saved compares with 8.6 MB estimated. Per row: `events.seq` 8.1 → 1.2 bytes,
`events.ts` and `orders.created_at` 8.1 → 4.2, the 4-byte foreign keys 4.1 → 2.2. These widths
reflect the generator's value ranges.

Timings, two alternating runs each, and three alternating rounds of the lookup test:

- Bulk load: 48.2 and 49.5 s against 46.5 and 48.4 s. Not separable from noise.
- Resident scans, single-row commits and point lookups (best pass 1.39 to 1.46 µs against 1.38 to
  1.53 µs for the primary key) did not move.
- One first-touch scan, of `line_items`, took 40 ms in both runs against 25 and 27 ms. Earlier
  baseline runs of the same scan ranged from 25 to 49 ms, so this is not established; if real it is
  the slot rebuild at parse. The other two first-touch scans did not move.

**What was run**, Go core, all six switches on: the unit tests (layout pins only; two more than
step 5, a page-count pin and a corruption test whose table now fits in one leaf); `mise run sim:sweep
1..100 all go` (2,200 traces), and 1,320 more with the deferred-interior and CRC-32-manifest switches;
the randomized comparison test with every switch on. The corpus fails 20 of 752 files in memory mode
and 19 in disk mode, all pins; the one new failure is an `EXPLAIN` estimate. With the switches off the
unit tests pass.

## 9. Remaining steps

Structural:

1. **Key directory of index trees.** See §5. Worth less after step 5, which already removed the
   shared bytes of those keys.
2. **Presence tag (under 1%).** See §2.
3. **Resident size.** Rebuild an elided column lazily, and give range scans the prefix-aware
   comparison point lookups have, so a resident leaf stays the size of its page.
4. **Commit cost per touched leaf.** A commit re-encodes every record of a leaf it changes, so denser
   leaves make random-insert loads slower (§7). Each further density gain adds to this.

Encodings still to do, each one more saving term in the size rule of §7: string dictionaries (about
12 MB of the 120.0 MB payload before step 5, inferred) and decimals as scaled integers (6.5 MB). A
dictionary's saving depends on the distinct values of a region, so the split-point scan cannot use
running minima as §8 does; it needs a distinct count per candidate half.

## 10. Work if adopted

`format.md` (*Leaf node*, *Record*, *Value codec*, `leafOverhead`), `bplus-reshape.md` §4, `packed-leaf.md`,
`lazy-record.md`; every golden; the cost and storage-limit pins above; the Ruby reference and
`verify.rb`; the three cores; the bench dataset `generator_version`.
