# Leaf pages: structural size reductions (proposal)

**Status: proposal. Steps 1 to 7 are prototyped in the Go core only**, behind `JED_LEAF_NARROW=on`,
`JED_LEAF_ELIDE_KEY=on`, `JED_LEAF_FIXED_KEY=on`, `JED_LEAF_BARE_STRING=on`,
`JED_LEAF_KEY_PREFIX=on`, `JED_LEAF_FOR=on` and `JED_LEAF_DICT=on`. Steps 5 to 7 are encoding steps:
they make a leaf's size depend on its contents (§7 to §9).

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

## 9. Step 7: string dictionaries (prototyped)

A `text`, `bytea` or `json` column of a leaf may be written as a dictionary.

**Page.** The region's flags byte is `2`. Then a `u16` entry count `D` (1 to 256), `D` `u16` end
offsets, the entries, and `N` one-byte codes. An entry is a value's whole span, tag included; the
empty span is NULL. Entries are in order of first use.

**Which values share an entry.** Two NULLs, or two inline-plain strings with equal content. A
compressed or external value always has its own entry, even if its bytes equal another's. This keeps
the rule a function of each value's stored form and content, which both a decoded leaf and a page
can supply.

**Size.** One more saving term per such column, where `dup` is the total bytes of the spans that
repeat an earlier one:

```
saving = N − 2 − 2·D + dup when positive and D ≤ 256, else 0
```

- A page supplies each value's form and content from its span. A decoded row supplies them from its
  values and its disposition plan; the plan is computed only when the all-inline record exceeds
  `RECORD_MAX`, since otherwise every value is inline-plain.
- The split-point scan makes one forward and one backward pass per column, each with a set of the
  contents seen, to get `D` and `dup` for every prefix and suffix. A half's payload stays monotone:
  it is the smaller of two nondecreasing sizes, and passing 256 entries only raises it.
- One insert can add the 257th entry and return the whole region to its plain size.

**Read.** In place, with no rebuild: the region's directory and blob are the entries', and value `i`
is entry `codes[i]`. The three accessors that read a variable region (null test, span, span length)
take that one indirection; nothing above them changed, and an inline value is still a view of the
page.

**Result**, all seven switches on:

| | steps 1–6 | steps 1–7 |
|---|---|---|
| compacted file | 114.3 MB | 95.8 MB |
| leaf pages | 14,562 | 12,202 |
| leaf payload | 93.8 MB | 80.0 MB |
| bytes written during the bulk load | 5,753 MB | 5,568 MB |

Per row: `sessions.user_agent` 115.8 → 27.5 bytes, `orders.status` 11.1 → 1.3, `orders.currency`
6.0 → 1.1, `users.country` 5.0 → 2.0. Columns of distinct values (`email`, `sku`, `ip`) do not
change. Over half the saving is `user_agent`, which the generator draws from a short list; real
data decides this number more than any other in this document.

Timings, two alternating runs each, and three alternating rounds of the lookup test:

- **The bulk load is about 8% slower**: 52.7 to 54.5 s against 47.3 to 50.5 s. A first version was
  25% slower (61.4 and 61.8 s): it recomputed a whole leaf's dictionary saving on every insert once
  the leaf was past its plain capacity, 8.7 s of the load's CPU. A dirty decoded leaf now keeps its
  size facts (each dictionary column's entry set, each narrow-integer column's value span) and an
  insert adds one row to them; a replaced row drops them. The file is byte-for-byte the same size
  with the same leaf counts either way. What remains, by profile: about 2 s of dictionary sizing
  (the first change to a clean leaf in each commit still measures it whole), about 2 s building
  dictionaries in the encoder, and the denser leaves of §7.
- Scans and single-row commits did not move outside noise.
- Point lookups: best pass 1.36 µs in two rounds against 1.38 to 1.50 µs. In one round the
  dictionary configuration ran at 2.9 µs for the primary-key lookup with the index lookup near
  normal; not reproduced in the other two rounds and not explained.

**What was run**, Go core, all seven switches on: `mise run sim:sweep 1..100 all go` (2,200 traces),
and 1,320 more with the deferred-interior and CRC-32-manifest switches; the randomized comparison
test with every switch on; the corpus, which fails 21 of 752 files in memory mode and 19 in disk mode,
all pins (against step 6: one storage-limit test more, and another whose trip point moved the other
way). The unit tests fail 29 layout-pinned tests, against 14 at step 6. The 15 new ones read as pins from their
messages: tests that pad rows with a repeated string to fill pages and then assert a page count, a
storage limit, a manifest overflow or a file size. They were not examined beyond their messages.
With the switches off the unit tests pass.

**Open points.**

- A value stored in a large form whose record now fits inline is counted as its own entry by a
  decoded leaf, because its content is not at hand, but would share an entry once written inline and
  read back. The two sizes then differ for the same leaf. Whether such a value is in fact rewritten
  inline was not checked.
- Only strings are eligible. `decimal` and the other variable-width types are not.
- A scan comparing a column with a literal could compare entries once instead of every value.

## 10. Step 8: symbol-table regions (prototyped)

A variable-width column of a leaf may be written with a symbol table: `text`, `bytea`, `json`,
`jsonb`, `decimal`, and arrays, ranges and composites other than the integer arrays of §11. The
measurements that led here are in [leaf-encodings-experiment.md](leaf-encodings-experiment.md) §8.

**Page.** The region's flags byte is `3`. Then:

```
esc u8, K u8, K × (code u8, length u8), the K symbols
raw bitmap, ceil(N/8) bytes, MSB-first
N u16 end offsets of the stored bodies
the stored bodies
```

- A symbol is 2 to 8 bytes and stands for one code byte. `K` is at most 255. Codes and `esc` are
  distinct byte values.
- A value whose raw bit is clear is inline-plain. Its body is its content payload in codes: a code
  byte is its symbol, `esc` is followed by one literal byte, and any other byte is itself. A literal
  equal to `esc` or to a code is written escaped. A reader rebuilds the value's header (the tag and,
  without step 4, the `u16` length) from the column type.
- A value whose raw bit is set stores its span unchanged: a compressed or external form, or nothing
  for NULL.

**Training.** Five passes over the column's inline payloads. Each pass parses them with the current
table, counts symbols and adjacent pairs, and keeps the candidates that save the most bytes after
paying for their table entry; ties break by the symbol's bytes. Codes are the byte values the
payloads use least, unused ones first, at least 16; the first is `esc`. The table is a function of
the payloads in key order.

**Size, and why the table travels with the leaf.** A table's size is known only by building it, and
a split needs the size of every prefix and suffix of a leaf. Under a fixed table the size is a sum
over the values, so the table is fixed per leaf:

```
saving = Σ over inline values (header + payload − coded length) − table bytes − ceil(N/8)
```

- A leaf that fits the page as plain regions is written without tables, and measured without them.
- When a decoded leaf first does not fit, tables are trained from its rows and stay with the node.
  Later inserts are measured, and the leaf written, under those tables.
- When the leaf overflows under its tables it splits, both halves measured under them. A half keeps
  them only if it does not fit without them, so the small right half of an append trains its own
  when it fills.
- A leaf read from a page and then changed keeps the page's tables while it fits under them, and
  trains again when it overflows (a page does not record how much its table was trained from).
- A string column takes the larger of the dictionary saving (§9) and this one.

A leaf's bytes therefore depend on when its tables were trained, not only on its rows. Two cores
agree only if they train at the same points, which this rule states in terms of the implementation's
insert path. That has to become a rule about rows and pages before adoption (open point).

**Read.** The region is kept as its page bytes and rebuilt into the plain form (end offsets and
spans) the first time a value of the column is read; the rebuilt form stays with the resident leaf.
The walk for overflow chains reads the raw bitmap and never rebuilds. A scan that reads whole rows
asks for every column's span, so it rebuilds every such region of each leaf the first time it visits
it, whether or not the query uses the column (see the result). Reading one value in place, and
deferring the rebuild until a value is used, are both possible in this format and not built.

**Result.** The database of the earlier steps plus the two tables of
[leaf-encodings-experiment.md](leaf-encodings-experiment.md) §8.1 (prose, varied-key `jsonb`, arrays),
8 KiB pages (`TestRegionEngineExperiment`). The first and last columns are two alternating runs
each; the middle two are one run each of the first version of this step, before the training changes
below.

| | steps 1–7 | + step 8, first version | + step 9 only | + both |
|---|---|---|---|---|
| compacted file | 177.0 MB | 134.2 MB | 160.4 MB | 126.9 MB |
| file after the load | 191.6 MB | 147.0 MB | 191.6 MB | 152.1 MB |
| bulk load, earlier tables | 52.7, 55.3 s | 60.1 s | 54.1 s | 54.5, 55.3 s |
| bulk load, the two new tables | 5.7, 5.7 s | 26.5 s | 7.6 s | 10.6, 10.8 s |
| one-row update commit (`activity.title`) | 229, 231 µs | 425 µs | 227 µs | 375, 384 µs |
| one-row append commit (`activity`) | 328, 339 µs | 476 µs | 316 µs | 504, 510 µs |
| first scan of `activity` after open, `count(*), sum(id)` | 152, 166 ms | | | 294, 298 ms |

- **Size:** both steps take 28% off the file; the symbol table alone 24%, typed arrays alone 9%.
- **Loading text is about twice as slow:** the two new tables load in 10.7 s against 5.7 s. The
  earlier tables did not move. Training is 3.9 s of the difference: 34,511 tables over 86 MB of
  payload, about 45 ns a byte.
- **A one-row commit is 50% to 65% slower.** By profile the coding itself (measuring the leaf under
  its tables, then coding it when written, and rebuilding its regions) is about 15% of the commit;
  the rest is that the leaf holds more rows to decode and encode (§12, item 4).
- **The first scan of a table after open is up to twice as slow** when it reads whole rows: every
  symbol-table region of every leaf is rebuilt, about 45 µs a leaf here. Later scans are resident
  and did not move: text and `jsonb` scans ran within 10% either way, point lookups 1.1 to 2.5 µs in
  both configurations. The ten aggregate queries return the same values in all runs.

**Training cost.** The first version took 26.5 s to load the two new tables. Three changes:

- **The trainer** keeps candidates as integers and reuses its working memory; it sorted strings and
  allocated per candidate. Same tables, 2.2 times faster.
- **No retraining at overflow.** The first version trained a leaf's tables again every time it
  overflowed under them, about two trainings per column per leaf. Keeping the first table for the
  leaf's life halves the trainings, and the file came out 0.5% smaller, not larger
  (`JED_LEAF_SYM_RETRAIN` sets a growth percentage at which to train again; 25 and 50 were no
  smaller).
- **Rows are classified once per rebuild**, not once for sizing and again for the split scan.

Fewer passes were tried and rejected: three passes in place of five made the file 4% larger for 0.3 s
of the load at 0.3 scale (`JED_LEAF_SYM_PASSES`).

**What was run**, Go core, all nine switches on: `mise run sim:sweep 1..100 all go` (2,200 traces
pass). The corpus in memory mode fails 22 of 752 files against 21 at step 7; the new one is an
`EXPLAIN` estimate. The unit tests fail 33 against 30. Two of the new ones assert a leaf or page count. The third,
`TestEncryptedTamperingAndReplayAreDetected`, replays onto every seventh page and requires one to be
live; with this layout none of the sampled pages is, and replaying onto every page is detected at
each of the 6 live ones.
With the switches off the unit tests fail one, `TestConcatThreeForms`, which also fails at the
commit before this step. The disk-mode corpus and the randomized comparison test were not run.

**Open points.**

- The training points above are stated in terms of the insert path. Bytes must be a function of
  rows and pages for two cores to agree.
- The first scan after open rebuilds regions the query does not use. A value that defers its
  rebuild until read would remove that.
- The commit path codes a touched leaf twice, once to measure it and once to write it.
- The table is per leaf. A table per column in the catalog would remove the per-page table and train
  on more data ([leaf-encodings-experiment.md](leaf-encodings-experiment.md) §8.2: the per-page table
  is why 64 KiB pages compress 30% better); not measured.
- A rebuilt region is extra resident memory outside the page-cache budget.
- A decoded leaf measures a value as inline-plain from the process's own header form. A page written
  under other switches is read correctly but could be measured differently.

## 11. Step 9: typed integer arrays (prototyped)

A column of `i16[]`, `i32[]` or `i64[]` may be written as counts and bit-packed offsets.

**Page.** The region's flags byte is `4`. Then `N` count bytes (255 is NULL, 0 the empty array), the
smallest element at the element width, a `u8` bit width `b` (0 to the element's bits), and every
element of every array in order as `element − smallest` in `b` bits, MSB-first, padded to a byte.

**When.** Every value is NULL, the empty array, or an inline-plain array with one dimension, lower
bound 1, no NULL element and at most 254 elements; and the region is smaller this way. One other
value returns the whole region to its plain form.

**Size.** With `e` empty arrays, `a` other arrays holding `M` elements of width `w`:

```
saving = (2·N + 3·e + 11·a + M·w) − (N + w + 1 + ceil(M·b/8)) when positive, else 0
```

These are sums and a value span, kept per leaf like the narrow-integer facts of §8 and recorded at
every boundary by the split scan. No table is involved, so the size is a function of the rows.

**Read.** Rebuilt on first use, as in §10.

**Result.** In the table above. Per value in the measurement of
[leaf-encodings-experiment.md](leaf-encodings-experiment.md) §8.2: small values 63.0 → 12.1 bytes,
sorted ids 73.0 → 23.8, full-range values 93.0 → 81.2. Load and commit times did not move.

**Open points.** Delta coding within an array (sorted ids), a per-array minimum, a wider count, and a
per-value fallback in place of abandoning the region were not tried. `text[]` takes the symbol table.

## 12. Remaining steps

Structural:

1. **Key directory of index trees.** See §5. Worth less after step 5, which already removed the
   shared bytes of those keys.
2. **Presence tag (under 1%).** See §2.
3. **Resident size.** Rebuild an elided column lazily, and give range scans the prefix-aware
   comparison point lookups have, so a resident leaf stays the size of its page.
4. **Commit cost per touched leaf.** A commit re-encodes every record of a leaf it changes, so denser
   leaves make random-insert loads slower (§7). Each further density gain adds to this.

Encodings still to do: decimals as scaled integers (estimated 6.5 MB of the 120.0 MB payload
before step 5).

## 13. Work if adopted

`format.md` (*Leaf node*, *Record*, *Value codec*, `leafOverhead`), `bplus-reshape.md` §4, `packed-leaf.md`,
`lazy-record.md`; every golden; the cost and storage-limit pins above; the Ruby reference and
`verify.rb`; the three cores; the bench dataset `generator_version`.
