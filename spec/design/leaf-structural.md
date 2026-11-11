# Leaf pages: structural size reductions (proposal)

**Status: proposal. Steps 1 to 4 are prototyped in the Go core only**, behind `JED_LEAF_NARROW=on`,
`JED_LEAF_ELIDE_KEY=on`, `JED_LEAF_FIXED_KEY=on` and `JED_LEAF_BARE_STRING=on`. Nothing here is in `format.md`, the Ruby reference, or the Rust and TS cores, and the prototype does not bump
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

## 7. Remaining steps

1. **Key directory of index trees (1.7% of the remaining leaf bytes).** See §5.
2. **Presence tag (under 1%).** See §2.
3. **Resident size.** Rebuild an elided column lazily, on first touch, so a leaf whose key columns
   are never read stays the size of its page.

After these the structural list is spent. What is left in a leaf is content: keys are 40% of the
leaf bytes, and the experiment's encoding estimates (a shared key prefix, frame-of-reference
integers, dictionaries), taken before steps 3 and 4, put the leaf payload at 47 to 71 MB against
125.7 MB. (The estimator does not yet read the string form of step 4.)

## 8. Work if adopted

`format.md` (*Leaf node*, *Record*, *Value codec*, `leafOverhead`), `bplus-reshape.md` §4, `packed-leaf.md`,
`lazy-record.md`; every golden; the cost and storage-limit pins above; the Ruby reference and
`verify.rb`; the three cores; the bench dataset `generator_version`.
