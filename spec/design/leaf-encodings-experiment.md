# Leaf column encodings: what giving up direct access buys (experiment)

**Status: measurement only.** Nothing here is built into the engine. The experiment is
`impl/go/zz_colcodec_test.go` (Go core, `JED_FORMAT_EXP=1`, `TestColumnCodecExperiment`).

## 1. The question

[leaf-structural.md](leaf-structural.md) limits its encoding steps to forms that keep value `i` of a
column reachable without decoding the column. This experiment drops that limit and asks what it
costs and saves: the claim being tested is that many workloads are bound by I/O, a leaf is already
column-major, so decoding a whole column to reach one value may be cheap enough, and scans may gain.

## 2. Method

The test builds the format experiment's database (1.3M rows, five tables, five indexes, 8 KiB pages,
structural steps 1 to 5 on, compacted: 15,596 leaf pages). For every leaf it takes each column
region and the leaf's keys as stored, encodes them under each candidate codec, and checks that the
decode matches. Sizes are over every leaf. Timings are over about 150 leaves per column, best of
three 20 ms runs, and were taken twice with close agreement.

Codecs:

- **Integers** (also dates, timestamps, booleans, and decimals whose values in a leaf share one
  scale and fit 64 bits): plain; minimum plus 1/2/4-byte offsets; minimum plus bit-packed offsets;
  delta from the previous value, bit-packed; delta as zigzag varints; offsets as varints; a
  256-entry dictionary; LZ4 of the plain slots; DEFLATE as an entropy-coder reference.
- **Byte strings**: plain; a 256-entry dictionary; LZ4 of the region; LZ4 of the bodies with the
  directory left plain; DEFLATE.
- **Keys**: shared prefix (today); front coding with a full key every 16; front coding with no
  restarts; LZ4 and DEFLATE of the prefix form.

A codec is **direct** when value `i` is reachable without reading the values before it. Bit-packed
offsets are direct: value `i` is at bit `i × width`.

## 3. Size

Each leaf region takes the smallest codec its strategy allows.

| strategy | leaf bytes | of today |
|---|---|---|
| today (steps 1 to 5) | 100.9 MB | 100% |
| direct, byte-aligned: minimum plus byte offsets, dictionary | 75.2 MB | 74.5% |
| direct, bit-packed offsets added | 72.7 MB | 72.0% |
| sequential integers and keys added (delta, varint, front coding) | 67.8 MB | 67.2% |
| LZ4 per column added, where smaller | 47.1 MB | 46.6% |
| whole-page LZ4 of today's pages | 62.1 MB | 58% of 106.5 MB (payloads with directories) |
| whole-page DEFLATE of today's pages | 49.1 MB | 46% of 106.5 MB |

- Direct encodings take 28% off. Sequential integer and key encodings take 5 points more, and most
  of that is front-coded index keys.
- Delta coding beats bit-packed offsets only where a column follows key order (a timestamp: 2.3
  against 3.2 bytes per value). On columns in no particular order it is slightly larger.
- LZ4 per column takes a further 21 points, all from text: `user_agent` 116 → 14 bytes per value,
  `settings` 90 → 16, `email` 33 → 17, `sku` 18 → 10, `note` 17 → 9.

## 4. Cost

Time to reach one value of one leaf (leaves of 33 to 555 rows):

| codec | integer | text | key search |
|---|---|---|---|
| plain / shared prefix (today) | 2.6 ns | 3.5 ns | 33 ns |
| bit-packed offsets; dictionary | 3.2 ns | 4 to 6 ns | |
| delta, bit-packed | 14 to 90 ns | | |
| varint | 30 to 460 ns | | |
| front-coded keys, restart every 16 | | | 83 to 130 ns |
| front-coded keys, no restarts | | | 0.14 to 2.0 µs |
| LZ4 of the column | 0.13 to 1.6 µs | 0.13 to 4 µs | 0.07 to 5 µs |

Time per value to decode a whole column:

| codec | integer | text |
|---|---|---|
| plain | 0.7 ns | 0.7 to 1.7 ns |
| minimum plus byte offsets | 0.24 to 0.7 ns | |
| bit-packed offsets | 0.8 ns | |
| delta, bit-packed | 0.8 ns | |
| varint | 1 to 5 ns | |
| dictionary | 0.7 to 0.8 ns | 1.4 to 2.5 ns |
| LZ4 | 2 to 9 ns | 3 to 57 ns |

One whole row, summed over a table's columns with each column on its smallest codec:

| table | today | direct, bit-packed | + sequential | + LZ4 |
|---|---|---|---|---|
| `orders` | 50 ns | 55 ns | 224 ns | 3.8 µs |
| `line_items` | 44 ns | 49 ns | 357 ns | 4.4 µs |
| `users` | 44 ns | 48 ns | 148 ns | 6.1 µs |
| `orders_customer` (index) | 30 ns | 30 ns | 1.8 µs | 1.8 µs |

For scale, a resident primary-key lookup in the Go core is about 1.4 µs end to end and an index
lookup about 3.5 µs.

Per 8 KiB page on the test machine:

| operation | time |
|---|---|
| read from the device, random (`O_DIRECT`) | 105 µs |
| read from the device, sequential | 18 µs |
| read from the OS page cache | 0.8 µs |
| LZ4 decompress the page | 5.7 µs |
| LZ4 compress the page | 15 µs |
| DEFLATE decompress the page | 229 µs |
| CRC-32 | 0.2 to 0.3 µs |

## 5. Findings

1. **The cost half of the claim holds for integers.** Decoding a delta-coded column costs the same
   per value as reading plain slots, and reaching one value costs under 100 ns, a few percent of a
   lookup.
2. **The benefit half does not.** Sequential integer codecs save almost nothing over bit-packed
   offsets, which keep direct access. Giving up direct access for integers has no payoff.
3. **Keys: a small gain at a visible cost.** Front coding with restarts gets most of the key saving
   at about 100 ns per search, three to four times today's. Without restarts a search of a large
   index leaf costs 1 to 2 µs, about half an index lookup.
4. **Scans do not gain from sequential codecs.** Byte-aligned offsets are the fastest form to
   aggregate (up to three times plain), and they are direct.
5. **Only text compression is large**, and its cost is real: 2 to 6 µs to read one row's compressed
   columns, several times a resident lookup, and 3 to 57 ns per value in a scan.
6. **Where the I/O argument applies.** Against a device read (105 µs random, 18 µs sequential) every
   decode above is small, so a file half the size about halves a cold scan. It does not reduce the
   reads of a cold point lookup, which fetches one leaf either way. Against the OS page cache
   (0.8 µs) LZ4 makes a page fault several times dearer. jed's stated target is a dataset resident
   in RAM (CLAUDE.md §9), where the steady state pays decode and no I/O.
7. **Compressed columns force a memory choice.** Decompress when the page is loaded and lookups stay
   fast, but a resident leaf is its uncompressed size, which gives up the property that resident
   memory is about the page size. Decompress on access and memory stays small, but every read of
   that column pays finding 5.

## 6. Conclusion

- **Sequential integer encodings (delta, varint): not worth it.** Take bit-packed or byte-aligned
  offsets and dictionaries, which are direct.
- **Front-coded keys: marginal.** At most 5% of leaf bytes for slower searches; not recommended now.
- **Per-column LZ4 for text: a separate decision, not a default.** It is the one large saving left
  (about 30% of the remaining leaf bytes on this data), and it suits cold, scan-heavy, larger-than-RAM
  data. For the resident case it trades memory or lookup time for file size. If it is ever taken it
  should be opt-in per column or limited to wide columns.

## 7. Limits

- These are isolated codec timings, not engine runs. Commit time, bulk load, split planning for a
  column whose size is known only after compression, and cost-meter effects were not measured.
- The data is generated. User agents and settings are drawn from small template sets, which
  flatters dictionaries and LZ4; integer ranges reflect the generator.
- LZ4 is the core's own block codec. A tuned decoder would be faster, but the core cannot adopt one
  without giving up byte-identical encoding (large-values.md §6).
- One machine, one page size. The `O_DIRECT` figures are for its disk.

## 8. Follow-up: codecs for variable-width regions, and page size

Section 6 left per-column LZ4 as the one large saving. This follow-up asks three things: whether a
codec other than LZ4 suits text, JSON and arrays better; what integer arrays need; and how page size
changes the answer. Test: `TestRegionCodecExperiment` in `impl/go/zz_regioncodec_test.go`.

### 8.1 Method

The database is the format experiment's plus two tables whose values are not drawn from small
template sets:

- `activity` (120,000 rows): `title` (two or three random words), `path` (a structured URL),
  `message` (one line of this repository's design documents, up to 160 bytes), `attrs` (jsonb with 4
  to 12 keys drawn from 80, values of mixed kinds).
- `samples` (150,000 rows): `tags text[]` (1 to 6 words from 300), `scores i32[]` (5 to 20 values in
  0 to 100), `refs i64[]` (3 to 12 sorted ids below 10 million), `readings i32[]` (8 to 32 values
  over the full range).

Structural steps 1 to 5 are on and the narrow-integer and dictionary regions off, so every region is
read in its plain form. Each variable-width region of every leaf is encoded under each codec; every
eleventh leaf is decoded in full and compared. Timings are over about 150 leaves per column as in
section 2. The whole run is repeated at 4, 8, 16, 32 and 64 KiB pages.

Codecs added to section 2's plain, 256-entry dictionary, LZ4 and DEFLATE:

- **fsst**: a symbol table as in FSST (Boncz, Neumann, Leis 2020). Up to 255 symbols of 1 to 8
  bytes, built in five passes; code 255 escapes one literal byte. A value decodes on its own.
- **symtab**: the same construction, but a literal byte stands for itself and symbols take byte
  values the region does not use (the rarest are evicted behind an escape when fewer than 16 are
  free). A value decodes on its own.
- **huf**: order-0 canonical Huffman over the region, codes of at most 11 bits, each value starting
  on a byte. A value decodes on its own once the 2048-entry decode table is built.
- **lz4+huf**: the LZ4 block, then order-0 Huffman over the block.
- **arr-for-bytes, arr-for-bits** (integer arrays): one count byte per value, then every element of
  the region as a minimum plus byte-aligned or bit-packed offsets. Direct.
- **arr-shuf-lz4** (integer arrays): the elements byte-shuffled, then LZ4.

### 8.2 Size at 8 KiB

Bytes per value:

| column | plain | dict | LZ4 | fsst | symtab | huf | LZ4+huf | DEFLATE |
|---|---|---|---|---|---|---|---|---|
| `activity.title` | 23.1 | 24.1 | 22.6 | 25.4 | 22.4 | 17.2 | 20.0 | 15.9 |
| `activity.path` | 52.7 | 53.7 | 35.6 | 43.1 | 42.0 | 37.7 | 31.8 | 24.9 |
| `activity.message` | 88.3 | 89.3 | 77.9 | 84.1 | 80.4 | 63.1 | 64.4 | 55.5 |
| `activity.attrs` (jsonb, varied keys) | 184.2 | 185.2 | 129.5 | 137.0 | 129.0 | 123.2 | 106.7 | 89.1 |
| `users.settings` (jsonb, one template) | 90.0 | 83.9 | 16.4 | 24.5 | 27.2 | 57.6 | 14.0 | 10.2 |
| `users.email` | 32.5 | 33.6 | 16.7 | 22.0 | 23.1 | 23.1 | 15.6 | 12.6 |
| `orders.note` | 17.2 | 16.8 | 8.9 | 10.6 | 10.9 | 11.7 | 7.5 | 5.9 |
| `line_items.sku` | 18.0 | 16.7 | 10.0 | 8.6 | 8.9 | 10.2 | 7.1 | 6.4 |
| `sessions.ip` | 16.3 | 17.3 | 15.4 | 15.5 | 15.0 | 10.2 | 12.9 | 9.9 |
| `sessions.user_agent` | 115.8 | 48.0 | 13.6 | 30.4 | 32.0 | 81.5 | 14.5 | 10.8 |
| `samples.tags` | 43.1 | 44.1 | 32.9 | 36.9 | 35.1 | 27.6 | 27.6 | 22.1 |

Integer arrays, bytes per value:

| column | plain | LZ4 | fsst | symtab | huf | DEFLATE | arr-for-bytes | arr-for-bits | arr-shuf-lz4 |
|---|---|---|---|---|---|---|---|---|---|
| `scores` (small values) | 63.0 | 44.0 | 42.4 | 40.0 | 30.7 | 24.8 | 13.7 | 12.1 | 14.2 |
| `refs` (sorted ids) | 73.0 | 48.7 | 60.6 | 39.2 | 43.3 | 36.5 | 31.3 | 23.8 | 24.2 |
| `readings` (full range) | 93.0 | 89.8 | 105.2 | 89.7 | 94.5 | 88.7 | 81.2 | 81.2 | 81.4 |

All variable-width column bytes, each region on its smallest allowed codec:

| strategy | 4 KiB | 8 KiB | 16 KiB | 32 KiB | 64 KiB |
|---|---|---|---|---|---|
| plain | 129.5 MB | 129.5 | 129.5 | 129.5 | 129.5 |
| dictionary (today's direct form) | 119.8 | 116.7 | 114.9 | 112.4 | 110.7 |
| dictionary, symtab | 95.0 | 83.2 | 74.7 | 69.7 | 66.7 |
| dictionary, fsst | 98.9 | 88.4 | 77.9 | 68.5 | 61.8 |
| dictionary, huf | 89.4 | 83.0 | 79.3 | 77.1 | 75.7 |
| dictionary, symtab, huf, typed arrays (all direct) | 76.1 | 69.9 | 65.1 | 62.7 | 59.2 |
| dictionary, LZ4 | 85.6 | 80.1 | 76.0 | 72.9 | 70.5 |
| dictionary, LZ4, typed arrays | 75.4 | 70.3 | 66.5 | 63.6 | 61.5 |
| dictionary, LZ4, LZ4+huf, typed arrays | 68.6 | 61.9 | 57.8 | 55.3 | 53.8 |
| DEFLATE (reference) | 66.3 | 60.3 | 56.4 | 53.7 | 51.9 |

### 8.3 Read cost

Time to read one value of one leaf, with nothing cached (a table-based codec loads its table first):

| column | codec | 4 KiB | 8 KiB | 16 KiB | 32 KiB | 64 KiB |
|---|---|---|---|---|---|---|
| `activity.message` | LZ4 | 0.8 µs | 1.8 | 5.7 | 11.2 | 25.6 |
| | fsst | 0.7 | 1.2 | 1.3 | 1.3 | 1.3 |
| | symtab | 0.4 | 0.8 | 1.1 | 1.0 | 1.0 |
| | huf | 1.9 | 2.0 | 2.1 | 2.0 | 1.9 |
| | LZ4+huf | 6.6 | 11.0 | 19.9 | 36.4 | 74.8 |
| `activity.attrs` | LZ4 | 2.4 | 6.2 | 13.4 | 27.6 | 71.1 |
| | fsst | 1.2 | 1.3 | 1.3 | 1.3 | 1.4 |
| `orders.note` | LZ4 | 1.6 | 3.5 | 7.3 | 14.6 | 29.1 |
| | fsst | 0.4 | 0.5 | 0.6 | 0.8 | 1.0 |
| `orders.status` | fsst | 0.06 | 0.06 | 0.06 | 0.06 | 0.06 |
| `samples.scores` | arr-for-bits | 0.11 | 0.12 | 0.12 | 0.12 | 0.14 |

Time per value to decode a whole column at 8 KiB (plain is about 1 ns):

| column | LZ4 | fsst | symtab | huf | LZ4+huf |
|---|---|---|---|---|---|
| `activity.message` (88 bytes) | 85 ns | 91 | 182 | 366 | 490 |
| `activity.attrs` (184 bytes) | 273 | 142 | 308 | 682 | 871 |
| `users.email` (33 bytes) | 33 | 21 | 30 | 115 | 146 |
| `orders.note` (17 bytes) | 26 | 12 | 13 | 56 | 78 |
| `samples.scores` (63 bytes) | 96 | 46 | 51 | 214 | 326 |

The typed array codecs decode `scores` at 47 to 74 ns per value.

### 8.4 Page size in the engine

`TestFormatExperiment` with all seven structural and encoding steps on and no region compression,
fsync skipped:

| | 4 KiB | 8 KiB | 16 KiB | 32 KiB | 64 KiB |
|---|---|---|---|---|---|
| bulk load | 36 s | 53 s | 71 s | 87 s | 98 s |
| bytes written by the load | 3.5 GB | 5.6 GB | 8.0 GB | 10.3 GB | 11.8 GB |
| compacted file | 99.3 MB | 95.8 MB | 94.9 MB | 94.3 MB | 95.7 MB |
| one-row update, per commit | 0.41 ms | 0.65 ms | 0.95 ms | 1.72 ms | 3.16 ms |
| bytes written per update commit | 58 KB | 95 KB | 130 KB | 242 KB | 469 KB |
| one-row insert, per commit | 0.45 ms | 0.80 ms | 1.08 ms | 2.00 ms | 3.46 ms |
| primary-key lookup, resident | 2.5 µs | 1.6 µs | 2.2 µs | 1.4 µs | 1.4 µs |
| index lookup, resident | 6.5 µs | 5.8 µs | 3.8 µs | 5.3 µs | 5.9 µs |
| resident scan of `orders` | 48 ms | 37 ms | 35 ms | 36 ms | 36 ms |

Lookup times vary between runs by about as much as they vary across the row.

### 8.5 Findings

1. **A symbol table gets LZ4's size and keeps direct access.** At 8 KiB, dictionary plus symtab is
   83.2 MB against 80.1 MB for dictionary plus LZ4, and with Huffman and typed arrays the direct set
   is 69.9 MB against 70.3 MB. A point read costs 0.06 to 1.3 µs, most of it loading the table, and
   does not grow with the page. fsst decodes a whole column as fast as LZ4 or faster; symtab takes
   up to twice as long on long values.
2. **LZ4 point reads grow with the region.** 1.8 µs at 8 KiB becomes 25 µs at 64 KiB for `message`.
   A larger page makes a compressed region smaller and every uncached read of it dearer.
3. **Which symbol table depends on region size.** symtab is smaller up to 16 KiB; fsst is smaller
   from 32 KiB, where its 255-symbol table is paid for. symtab degrades on binary data in large
   regions (`attrs` at 64 KiB: 145 against 93 bytes), which leave it few free byte values.
4. **Order-0 Huffman helps prose and digits, at a price.** It is the best direct codec for `message`,
   `title`, `ip` and `tags`, where there is little to match, but it decodes at about 4 ns per byte
   and a point read is 1 to 3 µs for the table.
5. **An entropy stage after LZ4 buys 12%** (70.3 to 61.9 MB) and makes point reads several times
   LZ4's. DEFLATE is 3% beyond that.
6. **Integer arrays want a typed encoding.** Small values go from 63 to 12 bytes, sorted ids from 73
   to 24, with direct reads at 0.1 µs. Shuffled LZ4 reaches the byte-aligned size and is sequential.
   Full-range values save only the 10-byte array header. No general codec comes close.
7. **Region compression does not replace a jsonb key dictionary when keys vary.** One template
   (`settings`) compresses 82% under LZ4. Varied keys in a wide row (`attrs`, 22 rows per 8 KiB
   leaf) compress 30%, and 39% at 64 KiB. Key names are an estimated third of those bytes.
8. **Page size is a write-cost dial, not a size dial.** Without region compression the compacted
   file is the same at every page size. With it, 8 to 16 KiB saves 5 to 10% and 8 to 64 KiB saves
   12 to 30%. Against that, a one-row commit writes 1.4 times the bytes at 16 KiB and 4.9 times at
   64 KiB, and the bulk load takes 1.3 and 1.85 times as long. The dictionary gains from larger
   pages too (`user_agent`: 48 bytes at 8 KiB, 11.5 at 64 KiB).

### 8.6 Conclusion

- Integer arrays: adopt the typed encoding with the narrow-integer regions.
- Text and JSON regions: prefer a direct symbol table (symtab at the default page size) to LZ4. It
  gives the same size without the read cost or the resident-memory choice of section 5 finding 7.
- Huffman and LZ4+Huffman: not now. The extra 10 to 15% costs a slower scan or a much slower read.
- Page size: keep 8 KiB as the default. Larger pages suit cold, read-mostly data and are already a
  per-database choice.

### 8.7 Limits

- Codec timings are isolated, as in section 7. Encode cost is not reported: the symbol-table builder
  here uses hash maps and five passes and is not representative. Its cost on the commit path, and
  split planning for a region whose size is known only after encoding, are unmeasured.
- A point read here loads the codec's table each time. A table kept with the resident leaf would
  remove most of the fsst, symtab and huf point-read cost; that was not measured.
- `message` is this repository's design prose and `attrs` is generated; real application text and
  documents will differ. The jsonb key share in finding 7 is an estimate from the generator.
- The huf size charges a packed code-length table and a one-byte value length that the test buffer
  holds in a wider form.
