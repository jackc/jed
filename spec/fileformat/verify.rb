#!/usr/bin/env ruby
# frozen_string_literal: true

# Independent reference implementation of the on-disk file format (format.md).
# It encodes the golden fixtures from a declarative description and decodes them
# back, so the goldens in fixtures/ are pinned by a THIRD implementation, not just
# self-certified by the two cores that also read/write them (CLAUDE.md S8). Pure
# Ruby, ASCII-only, no gem dependency; test-time only (CLAUDE.md S5).
#
#   ruby spec/fileformat/verify.rb              # verify fixtures/ match the reference
#   ruby spec/fileformat/verify.rb --generate   # (re)write fixtures/ from the reference
#   (or: rake verify)
#
# Exit 0 = all fixtures conform; nonzero = mismatch (prints the offending case).

MAGIC = "JEDB".b
VERSION = 7 # format_version 7: a per-page CRC-32 on every body page (the header grows 12→16
# bytes — format.md *Version scope*), atop v6's per-index unique flags byte (indexes.md §8)
PAGE_HEADER = 16 # v7: the 12-byte v6 header (page_type/item_count/next_page) + a 4-byte per-page crc32 at offset 12
ROOT_PAGE = 2  # the catalog root of a *fresh empty* db; relocatable thereafter (meta.root_page)
TXID = 1
PAGE_CATALOG = 1
PAGE_LEAF = 2
PAGE_INTERIOR = 3
PAGE_OVERFLOW = 4 # an out-of-line value slab, chained by next_page (large-values.md §12)

# Value-codec presence tags beyond 0x00 present-inline-plain / 0x01 NULL (large-values.md §12/§13;
# format.md "Large values"): 0x02 external-plain (u32 first_page + u32 payload_len), 0x03
# inline-compressed (u32 raw_len + u16 comp_len + LZ4 block), 0x04 external-compressed
# (u32 first_page + u32 stored_len + u32 raw_len; the chain carries the COMPRESSED block).
# The *_LEN constants are each form's full in-record size (tag included).
TAG_EXTERNAL = 0x02
TAG_INLINE_COMP = 0x03
TAG_EXTERNAL_COMP = 0x04
EXTERNAL_PTR_LEN = 1 + 4 + 4
INLINE_COMP_OVERHEAD = 1 + 4 + 2 # + comp_len bytes
EXTERNAL_COMP_PTR_LEN = 1 + 4 + 4 + 4
S_COMPRESS = 32 # payloads below this are never fed to the encoder (large-values.md §13)

require_relative "lz4"

WIDTH = { "int16" => 2, "int32" => 4, "int64" => 8, "timestamp" => 8, "timestamptz" => 8 }.freeze
TYPECODE = { "int16" => 1, "int32" => 2, "int64" => 3, "text" => 4, "boolean" => 5, "decimal" => 6,
             "bytea" => 7, "uuid" => 8, "timestamp" => 9, "timestamptz" => 10 }.freeze
CODETYPE = TYPECODE.invert.freeze

# uuid-raw16 (encoding.md §2.7): the 16 raw bytes of the canonical 8-4-4-4-12 form. Used both
# as the value-codec body (fixed 16 bytes, no length prefix) and as the PRIMARY-KEY bytes
# (uuid is the first non-integer key — no sign-flip, escape, or terminator).
def uuid_to_bytes(s) = [s.delete("-")].pack("H*")

def uuid_from_bytes(bytes)
  h = bytes.unpack1("H*")
  "#{h[0, 8]}-#{h[8, 4]}-#{h[12, 4]}-#{h[16, 4]}-#{h[20, 12]}"
end

# --- declarative fixtures (mirror what the cores build via SQL) --------------

# A column. `precision`/`scale` are the decimal typmod (only meaningful for type "decimal";
# nil = an unconstrained `numeric` column, or a non-decimal column). `not_null` defaults to the
# pk flag (a PRIMARY KEY is implicitly NOT NULL). `default` is the column's DEFAULT value as the
# cores store it (already type-coerced): the sentinel :none = no default (flags bit2 off), `nil`
# = an explicit DEFAULT NULL, any other value = that default. Always carried so the decode-side
# column hash compares equal (format.md stores the typmod/default only when their bit is set).
def col(name, type, pk: false, not_null: nil, precision: nil, scale: nil, default: :none)
  { name: name, type: type, pk: pk, not_null: not_null.nil? ? pk : not_null,
    precision: precision, scale: scale, default: default }
end

# A table with a COMPOSITE primary key (constraints.md §3): the stored key is the
# concatenation of the members' encodings in key order (a 4-byte int32 then a
# 2-byte int16 — mixed widths, ../design/encoding.md §2.3), pinning the cross-core
# composite key bytes; the catalog persists the v5 pk ordinal list [0, 1].
# Rows include a negative first component (sign-flip ordering) and
# first-component ties broken by the second. Listed in ascending tuple order — the
# cores build this via `CREATE TABLE t (a int32, b int16, v int16, PRIMARY KEY (a, b))`
# and insert in this order (the tree shape is order-sensitive).
COMPOSITE_PK_TABLE = {
  name: "t",
  columns: [col("a", "int32", pk: true), col("b", "int16", pk: true), col("v", "int16")],
  rows: [[-2, 5, 10], [1, 1, 20], [1, 2, 30], [1, 3, 40],
         [2, 0, 50], [2, 1, 60], [3, 7, 70], [3, 9, 80]]
}.freeze

# A table with CHECK constraints (constraints.md §4): exercises the v4 catalog check list —
# an auto-named single-column check, an explicitly-named multi-column check, and a check
# whose text exercises the token rendering (a doubled-quote string literal, decimal
# literals, >= / <=). Stored in evaluation (name) order: price_range < t_b_check <
# t_note_check. The cores build this via
#   CREATE TABLE t (a int PRIMARY KEY, b int CHECK (b > 0), price numeric(8,2),
#     CONSTRAINT price_range CHECK (price >= 0.50 AND price <= 9999.99), note text,
#     CHECK (note = 'ok' OR note = 'a''b'))
# and insert rows in ascending key order.
CHECK_TABLE = {
  name: "t",
  columns: [col("a", "int32", pk: true), col("b", "int32"),
            col("price", "decimal", precision: 8, scale: 2), col("note", "text")],
  checks: [
    { name: "price_range", expr: "price >= 0.50 AND price <= 9999.99" },
    { name: "t_b_check", expr: "b > 0" },
    { name: "t_note_check", expr: "note = 'ok' OR note = 'a''b'" }
  ],
  rows: [[1, 5, "1.00", "ok"], [2, nil, "9999.99", "a'b"], [3, 100, "0.50", "ok"]]
}.freeze

PK_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("v", "int16")],
  # 20 rows: each record is 14 bytes, so a 256-byte page (cap 240, v7) overflows at 18 rows and
  # the tree becomes interior-root + two leaves (the load-bearing interior-node + split proof).
  # id 3 has a NULL value. Inserted in ascending key order (the tree shape is order-sensitive).
  rows: (1..20).map { |i| [i, i == 3 ? nil : i * 10] }
}.freeze

# A table whose rows force a HEIGHT-2 tree (an interior node whose children are themselves
# interior nodes) at page_size 256. A wide text padding column makes each record ~66 bytes, so a
# leaf holds 3 records and the root interior overflows after ~5 leaves -> a two-level interior.
# 18 rows, ascending int32 PK. Exercises interior-of-interior child pointers + post-order
# page allocation across a deeper tree.
TALL_TREE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("pad", "text")],
  rows: (1..18).map { |i| [i, format("row-%02d-%s", i, "x" * 48)] }
}.freeze

# A table with a text column: exercises the value codec's text branch (u16 byte-length +
# UTF-8 bytes), the empty string (a distinct non-NULL value), an embedded quote, a 2-byte
# UTF-8 char (U+00E9), a NULL text value, and a 4-byte astral char (U+1F600). The PK is an
# int32 (text is not allowed in a key this slice). \u escapes keep this source ASCII-only.
TEXT_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("s", "text")],
  rows: [[1, "alice"], [2, ""], [3, "O'Brien"], [4, "caf\u{E9}"], [5, nil], [6, "\u{1F600}"]]
}.freeze

# A table with a boolean column: exercises the value codec's boolean branch (a single
# bool-byte, 0x00 false / 0x01 true) plus a NULL boolean (the tag alone). The PK is an int32
# (boolean is not allowed in a key this slice — spec/design/types.md §9).
BOOL_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("flag", "boolean")],
  rows: [[1, true], [2, false], [3, nil]]
}.freeze

# A table with a decimal column: exercises the value codec's decimal branch (flags + u16 scale
# + u16 ndigits + base-10^4 groups), positive/negative/zero, a multi-group coefficient, a NULL,
# AND the catalog typmod (an unconstrained `numeric` column `d` and a constrained numeric(10,2)
# column `m`). The `m` values are already at scale 2, so storing them is a no-op coercion — the
# stored bytes equal what the cores write when they INSERT the same literals. PK is an int32
# (decimal is not allowed in a key this slice).
DECIMAL_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("d", "decimal"), col("m", "decimal", precision: 10, scale: 2)],
  rows: [[1, "1.50", "1.50"], [2, "-12345.6789", "-12.34"], [3, "0.00", "0.00"],
         [4, "100000000.000001", "100.00"], [5, nil, nil]]
}.freeze

# A table with a bytea column: exercises the value codec's bytea branch (u16 byte-length +
# RAW bytes, no UTF-8 validation). Covers a multi-byte value with a-f hex (\xdeadbeef), the
# empty byte string (a distinct non-NULL value), embedded 0x00 bytes, a high byte (0xFF), a
# NULL bytea, and a lone 0x00 byte. The PK is an int32 (bytea is not allowed in a key this
# slice). All byte values are forced to ASCII-8BIT (.b) so they round-trip verbatim.
BYTEA_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("b", "bytea")],
  rows: [[1, "\xDE\xAD\xBE\xEF".b], [2, "".b], [3, "\x00\x01\x02".b],
         [4, "\xFF".b], [5, nil], [6, "\x00".b]]
}.freeze

# A table with a uuid PRIMARY KEY (the first golden with a NON-integer stored key — the
# load-bearing §8 cross-core key-path proof) plus a nullable uuid column. Exercises the value
# codec's fixed-16-byte uuid branch (no length prefix), the uuid key encoding (bare 16 bytes,
# uuid-raw16), a present and a NULL uuid value, and the nil/max boundary UUIDs. Rows are written
# in key (byte) order. The cores build this via `CREATE TABLE t (id uuid PRIMARY KEY, ref uuid)`.
UUID_TABLE = {
  name: "t",
  columns: [col("id", "uuid", pk: true), col("ref", "uuid")],
  rows: [["00000000-0000-0000-0000-000000000000", "550e8400-e29b-41d4-a716-446655440000"],
         ["550e8400-e29b-41d4-a716-446655440000", nil],
         ["f47ac10b-58cc-4372-a567-0e02b2c3d479", "00000000-0000-0000-0000-000000000000"],
         ["ffffffff-ffff-ffff-ffff-ffffffffffff", "ffffffff-ffff-ffff-ffff-ffffffffffff"]]
}.freeze

# A table exercising the DEFAULT column constraint on disk (format.md): the catalog flags bit2
# + the column's pre-evaluated default value via the value codec, written AFTER the decimal
# typmod. Covers an int default, a text default, a DEFAULT NULL (the lone 0x01 tag), a NOT NULL
# column with a default (bit1 + bit2), a decimal default coerced to numeric(6,2), and a plain
# no-default column (bit2 off, no extra bytes). The stored defaults and row values are exactly
# what the cores write when they CREATE the table and INSERT (row 1 takes every default; row 2
# provides all values). PK is an int32.
DEFAULT_TABLE = {
  name: "t",
  columns: [
    col("id", "int32", pk: true),
    col("n", "int32", default: 0),
    col("note", "text", default: "none"),
    col("maybe", "int32", default: nil),
    col("req", "int32", not_null: true, default: 7),
    col("amt", "decimal", precision: 6, scale: 2, default: "1.50"),
    col("plain", "int16")
  ],
  rows: [[1, 0, "none", nil, 7, "1.50", nil],
         [2, 42, "hi", 5, 9, "2.00", 100]]
}.freeze

# A table with a timestamp column: exercises the value codec's timestamp branch (the int64
# microsecond instant, the same 8-byte int-be-signflip body as int64 — type code 8). Covers a
# positive instant (2024-01-01 12:00:00), a pre-1970 negative one (1969-12-31 23:59:59.5), a
# BC-era one (0001-01-01 00:00:00 BC), the -infinity/+infinity sentinels (i64::MIN/MAX), and a
# NULL. Values are the raw micros the cores compute from the corresponding literals. PK is int32.
TIMESTAMP_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("ts", "timestamp")],
  rows: [[1, 1_704_110_400_000_000], [2, -500_000], [3, -62_167_219_200_000_000],
         [4, -9_223_372_036_854_775_808], [5, 9_223_372_036_854_775_807], [6, nil]]
}.freeze

# A table with a timestamptz column (type code 10): same 8-byte int64 body. The +05 literal
# normalizes to UTC (12:00+05 -> 07:00Z -> 1_704_092_400_000_000).
TIMESTAMPTZ_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("ts", "timestamptz")],
  rows: [[1, 1_704_110_400_000_000], [2, 1_704_092_400_000_000], [3, -500_000],
         [4, -9_223_372_036_854_775_808], [5, 9_223_372_036_854_775_807], [6, nil]]
}.freeze

# Incompressible filler (format.md "Fixtures"): xorshift32(seed "JEDB") mapped to a 64-char
# alphabet (text) or raw bytes (bytea). High-entropy output has no 4-byte repeats, so the LZ4
# encoder never wins store-smaller and the value deterministically stays PLAIN. Mirrored in the
# cores' golden/cost tests. Each call restarts at the seed (one fixed stream per length).
ALPHA64 = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/"
FILLER_SEED = 0x4A454442

def filler_step(x)
  x ^= (x << 13) & 0xFFFFFFFF
  x ^= x >> 17
  (x ^ ((x << 5) & 0xFFFFFFFF)) & 0xFFFFFFFF
end

def filler_text(n)
  x = FILLER_SEED
  out = +""
  n.times do
    x = filler_step(x)
    out << ALPHA64[x % 64]
  end
  out
end

def filler_bytes(n)
  x = FILLER_SEED
  out = +"".b
  n.times do
    x = filler_step(x)
    out << (x % 256).chr
  end
  out
end

# A table with large INCOMPRESSIBLE text + bytea values that must spill OUT-OF-LINE PLAIN to
# overflow pages (large-values.md §12). At page_size 256 the per-record cap is
# RECORD_MAX = (256-16-12)/2 = 114 (v7), so a value of ~600/300 bytes exceeds it: compression is
# attempted first (Slice B) but rejected by store-smaller (the filler is high-entropy), so the
# record holds a fixed 0x02 pointer (u32 first_page + u32 len) and the raw bytes live in a chain
# of page_type-4 slabs (240 bytes each). Row 1's text (600 B → 3 slabs) and bytea (300 B → 2
# slabs) both spill; row 2's values stay inline; row 3 is NULL/NULL. Exercises multi-page chains,
# multi-column spill, and the inline+external mix in one leaf. The PK stays int32.
OVERFLOW_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("body", "text"), col("blob", "bytea")],
  rows: [[1, filler_text(600), filler_bytes(300)], [2, "small", ["cafe"].pack("H*")], [3, nil, nil]]
}.freeze

# A table with large COMPRESSIBLE values exercising Slice B's forms (large-values.md §13,
# format.md "Large values", lz4.md). At page_size 256 (RECORD_MAX 114, C = 240, v7):
# row 1's 600-char "x" run compresses to a few bytes → 0x03 inline-compressed text — and its
# 200-byte 0xAB bytea run → 0x03 inline-compressed bytea (two compressed values, one record);
# row 2's 400-char half-filler/half-run text compresses to ~200 B — smaller than plain but still
# over RECORD_MAX → 0x04 external-compressed (a chain carrying the COMPRESSED block);
# row 3 stays fully inline-plain; row 4 is NULL/NULL. PK int32.
COMPRESSED_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("body", "text"), col("blob", "bytea")],
  rows: [[1, "x" * 600, (["ab"].pack("H*") * 200)],
         [2, filler_text(200) + ("y" * 200), nil],
         [3, "tiny", ["cafe"].pack("H*")],
         [4, nil, nil]]
}.freeze

# A table with SECONDARY INDEXES (v5 — indexes.md): pins the catalog reshape and the index
# trees. The PK list order (b, a) DIFFERS from declaration order (the lifted composite-PK
# narrowing — pk ordinals [1, 0]); index `i_u` covers a NULLABLE uuid column holding a NULL
# (the encoding.md §2.2 presence tag in stored index order — present values first, NULL
# last), and `t_a_b_idx` is the auto-named two-column index. Index records have EMPTY
# payloads (key only). Indexes listed in ascending lowercased-name order (the catalog
# order). The cores build this via
#   CREATE TABLE t (a int32, b int32, u uuid, PRIMARY KEY (b, a));
#   CREATE INDEX i_u ON t (u);  CREATE INDEX ON t (a, b);
# and insert rows in ascending storage-key order.
INDEX_TABLE = {
  name: "t",
  columns: [col("a", "int32", pk: true), col("b", "int32", pk: true), col("u", "uuid")],
  pk_order: [1, 0], # PRIMARY KEY (b, a)
  indexes: [
    { name: "i_u", cols: [2] },
    { name: "t_a_b_idx", cols: [0, 1] }
  ],
  rows: [[1, 10, "550e8400-e29b-41d4-a716-446655440000"],
         [2, 10, nil],
         [3, 20, "00000000-0000-0000-0000-000000000000"]]
}.freeze

# A table with UNIQUE indexes (v6 — indexes.md §8, constraints.md §5): pins the per-index
# flags byte. `t_v_key` is the UNIQUE constraint's auto-named index over a NULLABLE column
# holding two NULLs (NULLS DISTINCT — both entries stored, side by side after the present
# value); `wv` is a named two-column UNIQUE constraint; `uq` is a CREATE UNIQUE INDEX; `nu`
# is a plain index over the same column as `t_v_key` (flags 0 beside flags 1). The cores
# build this via
#   CREATE TABLE t (id int32 PRIMARY KEY, v int32, w int32,
#                   UNIQUE (v), CONSTRAINT wv UNIQUE (w, v));
#   CREATE INDEX nu ON t (v);  CREATE UNIQUE INDEX uq ON t (w);
UNIQUE_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("v", "int32"), col("w", "int32")],
  indexes: [
    { name: "nu", cols: [1] },
    { name: "t_v_key", cols: [1], unique: true },
    { name: "uq", cols: [2], unique: true },
    { name: "wv", cols: [2, 1], unique: true }
  ],
  rows: [[1, 10, 100], [2, nil, 200], [3, nil, 300]]
}.freeze

FIXTURES = [
  { file: "empty_db.jed",        page_size: 256, tables: [] },
  { file: "overflow_table.jed",  page_size: 256, tables: [OVERFLOW_TABLE] },
  { file: "compressed_table.jed", page_size: 256, tables: [COMPRESSED_TABLE] },
  { file: "one_table_empty.jed", page_size: 256,
    tables: [{ name: "t", columns: [col("id", "int32", pk: true), col("v", "int16")], rows: [] }] },
  { file: "pk_table.jed",        page_size: 256, tables: [PK_TABLE] },
  { file: "text_table.jed",      page_size: 256, tables: [TEXT_TABLE] },
  { file: "bool_table.jed",      page_size: 256, tables: [BOOL_TABLE] },
  { file: "decimal_table.jed",   page_size: 256, tables: [DECIMAL_TABLE] },
  { file: "bytea_table.jed",     page_size: 256, tables: [BYTEA_TABLE] },
  { file: "uuid_table.jed",      page_size: 256, tables: [UUID_TABLE] },
  { file: "default_table.jed",   page_size: 256, tables: [DEFAULT_TABLE] },
  { file: "timestamp_table.jed",   page_size: 256, tables: [TIMESTAMP_TABLE] },
  { file: "timestamptz_table.jed", page_size: 256, tables: [TIMESTAMPTZ_TABLE] },
  { file: "nopk_table.jed",      page_size: 256,
    tables: [{ name: "r", columns: [col("a", "int16"), col("b", "int64")],
               rows: [[7, 70], [8, 80], [9, 90]] }] },
  { file: "composite_pk_table.jed", page_size: 256, tables: [COMPOSITE_PK_TABLE] },
  { file: "check_table.jed", page_size: 256, tables: [CHECK_TABLE] },
  { file: "index_table.jed", page_size: 256, tables: [INDEX_TABLE] },
  { file: "unique_table.jed", page_size: 256, tables: [UNIQUE_TABLE] },
  { file: "tall_tree.jed",       page_size: 256, tables: [TALL_TREE] },
  # Torn-write fallback: same image as pk_table, with one meta slot's CRC smashed.
  { file: "torn_meta_slot0.jed", page_size: 256, tables: [PK_TABLE], corrupt_slot: 0 },
  { file: "torn_meta_slot1.jed", page_size: 256, tables: [PK_TABLE], corrupt_slot: 1 }
].freeze

# --- primitives -------------------------------------------------------------

def u16(v) = [v].pack("n")
def u32(v) = [v].pack("N")
def u64(v) = [v].pack("Q>")

# CRC-32/IEEE (reflected, poly 0xEDB88320). crc32("123456789") == 0xCBF43926.
def crc32(data)
  crc = 0xFFFFFFFF
  data.each_byte do |b|
    crc ^= b
    8.times do
      mask = (crc & 1).zero? ? 0 : 0xFFFFFFFF
      crc = (crc >> 1) ^ (0xEDB88320 & mask)
    end
  end
  crc ^ 0xFFFFFFFF
end

# The per-page checksum (v7, format.md *Page header*): CRC-32/IEEE over a body page's bytes
# EXCLUDING its own 4-byte crc32 field at [12,16) — i.e. [0,12) then [16,ps). crc32 is linear over
# the byte stream, so checksumming the concatenation matches the cores' streaming page_crc exactly.
def page_crc(page)
  crc32(page.byteslice(0, 12) + page.byteslice(PAGE_HEADER, page.bytesize - PAGE_HEADER))
end

# int-be-signflip: add bias 2^(bits-1), emit unsigned big-endian (encoding.md).
def encode_int(width, value)
  bias = 1 << (width * 8 - 1)
  u = value + bias
  Array.new(width) { |i| (u >> (8 * (width - 1 - i))) & 0xFF }.pack("C*")
end

def decode_int(width, bytes)
  bytes.bytes.reduce(0) { |acc, b| (acc << 8) | b } - (1 << (width * 8 - 1))
end

# value codec: presence tag + (when present) the type's body. 0x01 = NULL; 0x00 = present.
# Integers reuse the order-preserving int bytes; text and bytea diverge to a compact u16
# byte-length + bytes (text: UTF-8 collation-C bytes; bytea: raw bytes — byte-identical here,
# only the source encoding / read-side UTF-8 assertion differs); boolean is a single bool-byte
# 0x00 false / 0x01 true (format.md "Value codec").
def encode_value(type, v)
  return "\x01".b if v.nil?

  case type
  when "text", "bytea"
    bytes = v.b
    "\x00".b + u16(bytes.bytesize) + bytes
  when "boolean"
    "\x00".b + (v ? "\x01".b : "\x00".b)
  when "decimal"
    "\x00".b + encode_decimal(v)
  when "uuid"
    "\x00".b + uuid_to_bytes(v) # fixed 16 bytes, NO length prefix
  else
    "\x00".b + encode_int(WIDTH.fetch(type), v)
  end
end

# Parse a decimal STRING ("[-]int[.frac]") into (neg, scale, coefficient). The coefficient is a
# Ruby integer (arbitrary precision); scale is the fractional digit count. No negative zero.
def parse_decimal(s)
  neg = s.start_with?("-")
  body = neg ? s[1..] : s
  int, frac = body.split(".", 2)
  frac ||= ""
  coeff = (int + frac).to_i # leading zeros are harmless to_i
  { neg: neg && coeff != 0, scale: frac.length, coeff: coeff }
end

# Render (neg, scale, coefficient) to the canonical decimal string (spec/design/decimal.md §6).
def render_decimal(neg, scale, coeff)
  digits = coeff.to_s # "0" for zero, no leading zeros
  sign = neg ? "-" : ""
  return sign + digits if scale.zero?

  digits = digits.rjust(scale + 1, "0")
  point = digits.length - scale
  "#{sign}#{digits[0...point]}.#{digits[point..]}"
end

# Decimal value codec body (format.md): flags (bit0 sign), u16 scale, u16 ndigits, then that
# many big-endian base-10^4 coefficient groups, most-significant first. Zero carries no groups.
def encode_decimal(s)
  d = parse_decimal(s)
  groups = []
  c = d[:coeff]
  while c.positive?
    groups.unshift(c % 10_000)
    c /= 10_000
  end
  [d[:neg] ? 1 : 0].pack("C") + u16(d[:scale]) + u16(groups.size) + groups.map { |g| u16(g) }.join
end

# --- encoding (reference serializer) ----------------------------------------

# The primary-key member ordinals in KEY order (v5): an explicit pk_order when the fixture
# declares one (key order != declaration order), else the pk-flagged columns in declaration
# order. [] = no PK (synthetic rowid keys).
def pk_order(table)
  table[:pk_order] || table[:columns].each_index.select { |i| table[:columns][i][:pk] }
end

def table_entry_bytes(table, root_data_page, index_roots)
  out = +"".b
  out << u16(table[:name].bytesize) << table[:name].b
  out << u16(table[:columns].size)
  table[:columns].each do |c|
    out << u16(c[:name].bytesize) << c[:name].b
    out << [TYPECODE.fetch(c[:type])].pack("C")
    has_default = c[:default] != :none
    # bit0 (primary_key through v4) is RETIRED in v5 — the pk ordinal list below is the
    # single authority; the bit is reserved, written 0 (format.md).
    flags = 0
    flags |= 0b10 if c[:not_null]
    flags |= 0b100 if has_default
    out << [flags].pack("C")
    # A decimal column appends its typmod (precision, scale) — only for type_code 6, so
    # non-decimal entries are byte-unchanged. precision 0 = unconstrained numeric.
    out << u16(c[:precision] || 0) << u16(c[:scale] || 0) if c[:type] == "decimal"
    # A column with a DEFAULT (flags bit2) appends its pre-evaluated default value via the same
    # value codec rows use — AFTER the typmod, presence-gated. A DEFAULT NULL is one 0x01.
    out << encode_value(c[:type], c[:default]) if has_default
  end
  # The primary key (v5): count, then the member column ordinals in KEY order.
  pk = pk_order(table)
  out << u16(pk.size)
  pk.each { |i| out << u16(i) }
  # CHECK constraints (v4): count, then (name, expression text) per check, in the catalog's
  # evaluation order — ascending byte order of the lowercased name (constraints.md §4.4/§4.5).
  checks = table[:checks] || []
  out << u16(checks.size)
  checks.each do |ck|
    out << u16(ck[:name].bytesize) << ck[:name].b
    out << u16(ck[:expr].bytesize) << ck[:expr].b
  end
  # Secondary indexes (v5): count, then per index the name, key-column ordinals (index-key
  # order), the v6 flags byte (bit0 unique — indexes.md §8), and the index tree's root
  # page — in ascending lowercased-name order (indexes.md).
  indexes = table[:indexes] || []
  out << u16(indexes.size)
  indexes.each_with_index do |ix, k|
    out << u16(ix[:name].bytesize) << ix[:name].b
    out << u16(ix[:cols].size)
    ix[:cols].each { |i| out << u16(i) }
    out << [ix[:unique] ? 1 : 0].pack("C")
    out << u32(index_roots[k])
  end
  out << u32(root_data_page)
  out
end

# (key, row) pairs in stored (encoded-key) order. PK tables key on the PK member columns —
# the key is the CONCATENATION of the members' encodings in KEY order (a composite
# PRIMARY KEY, ../design/encoding.md §2.3; a single-column key is the one-member case).
# A no-PK table keys on a synthetic int64 rowid = insertion index (executor.rs).
def table_entries(table)
  pk_idxs = pk_order(table)
  pairs = table[:rows].each_with_index.map do |row, i|
    key = if pk_idxs.empty?
            encode_int(8, i)
          else
            pk_idxs.map do |pi|
              pk_type = table[:columns][pi][:type]
              # uuid is the bare 16 bytes (uuid-raw16), not the sign-flipped int encoding;
              # a PK member is NOT NULL, so no presence tag either way.
              if pk_type == "uuid"
                uuid_to_bytes(row[pi])
              else
                encode_int(WIDTH.fetch(pk_type), row[pi])
              end
            end.join.b
          end
    [key, row]
  end
  pairs.sort_by { |key, _| key } # String#<=> is bytewise == memcmp order
end

# A secondary-index entry key (indexes.md §3): each indexed column as the encoding.md §2.2
# NULLABLE SLOT (0x00 + bare order-preserving bytes when present, the lone 0x01 for NULL —
# always tagged, even for a NOT NULL column), then the row's storage key as the suffix.
def index_entry_key(table, ix, storage_key, row)
  out = +"".b
  ix[:cols].each do |ci|
    v = row[ci]
    if v.nil?
      out << "\x01".b
    else
      type = table[:columns][ci][:type]
      out << "\x00".b
      out << (type == "uuid" ? uuid_to_bytes(v) : encode_int(WIDTH.fetch(type), v))
    end
  end
  out << storage_key
  out
end

# An index's (entry_key, record-plan) pairs in entry-key order. An index record has an EMPTY
# payload (key only — format.md "Index trees"), so its plan is trivially inline.
def index_entries(table, ix)
  table_entries(table).map do |storage_key, row|
    key = index_entry_key(table, ix, storage_key, row)
    [key, { key: key, row: [], table: { columns: [] }, forms: [], comps: [], size: 2 + key.bytesize }]
  end.sort_by { |key, _| key }
end

# --- out-of-line large values (large-values.md §12) -------------------------

def spillable?(type) = %w[text bytea decimal].include?(type)
def record_max(cap) = [(cap - 12) / 2, 0].max # RECORD_MAX = (C-12)/2 (format.md "Why the record cap")

# A value's content payload P(v) — the bytes stored in the overflow chain when externalized: raw
# UTF-8 / raw bytes for text/bytea, the decimal body for decimal (large-values.md §12).
def value_payload(type, v)
  case type
  when "text", "bytea" then v.b
  when "decimal" then encode_decimal(v) # the body (no presence tag)
  else raise "only spillable values are externalized"
  end
end

# Decide each column's disposition for a record: [forms, comps, on-disk record size], where
# forms[i] is :inline / :inline_comp / :external / :external_comp and comps[i] holds the LZ4
# block for a compressed form. Spill only when forced (record > RECORD_MAX); then COMPRESS the
# largest eligible values first (store-smaller rule), then EXTERNALIZE the largest remaining —
# both passes ties-by-column-index. Deterministic, mirrors the cores' plan_dispositions
# (large-values.md §12/§13; format.md "Large values").
def plan_record(table, key, row, cap)
  cols = table[:columns]
  inline = cols.each_with_index.map { |c, i| encode_value(c[:type], row[i]).bytesize }
  forms = Array.new(cols.size, :inline)
  comps = Array.new(cols.size)
  cur = inline.dup
  size = 2 + key.bytesize + inline.sum
  max = record_max(cap)
  return [forms, comps, size] if size <= max

  # Pass 1 — compress (lz4.md): payload >= S_COMPRESS, largest inline-plain encoded size first,
  # ties by ascending index; adopt iff the encoded compressed form is strictly smaller.
  cand = (0...cols.size).select do |i|
    spillable?(cols[i][:type]) && !row[i].nil? && value_payload(cols[i][:type], row[i]).bytesize >= S_COMPRESS
  end
  cand.sort_by! { |i| [-inline[i], i] }
  cand.each do |i|
    break if size <= max

    comp = LZ4.compress(value_payload(cols[i][:type], row[i]))
    next unless INLINE_COMP_OVERHEAD + comp.bytesize < inline[i]

    forms[i] = :inline_comp
    comps[i] = comp
    size += INLINE_COMP_OVERHEAD + comp.bytesize - cur[i]
    cur[i] = INLINE_COMP_OVERHEAD + comp.bytesize
  end
  return [forms, comps, size] if size <= max

  # Pass 2 — externalize: anything whose current encoded size beats its pointer, largest first.
  cand = (0...cols.size).select do |i|
    spillable?(cols[i][:type]) &&
      cur[i] > (forms[i] == :inline_comp ? EXTERNAL_COMP_PTR_LEN : EXTERNAL_PTR_LEN)
  end
  cand.sort_by! { |i| [-cur[i], i] }
  cand.each do |i|
    break if size <= max

    ptr = forms[i] == :inline_comp ? EXTERNAL_COMP_PTR_LEN : EXTERNAL_PTR_LEN
    forms[i] = forms[i] == :inline_comp ? :external_comp : :external
    size += ptr - cur[i]
    cur[i] = ptr
  end
  [forms, comps, size]
end

# Write a payload across a chain of overflow pages (cap-byte slabs, in order), allocating each via
# `alloc` and linking with next_page (0 terminates). Returns the first page index for the pointer.
def write_overflow_chain(payload, cap, alloc, pages)
  n = (payload.bytesize + cap - 1) / cap
  indices = Array.new(n) { alloc.call }
  n.times do |j|
    slab = payload.byteslice(j * cap, cap)
    nxt = j + 1 < n ? indices[j + 1] : 0
    pages[indices[j]] = [PAGE_OVERFLOW, slab.bytesize, slab, nxt]
  end
  indices[0]
end

# Emit one record's on-disk bytes (key_len | key | values), spilling external columns to overflow
# pages drawn via `alloc` (large-values.md §12/§13). `rec` is a plan hash
# { key:, row:, table:, forms:, comps: } from plan_record.
def emit_record(rec, cap, alloc, pages)
  out = +"".b
  out << u16(rec[:key].bytesize) << rec[:key]
  rec[:table][:columns].each_with_index do |c, i|
    case rec[:forms][i]
    when :external
      payload = value_payload(c[:type], rec[:row][i])
      first = write_overflow_chain(payload, cap, alloc, pages)
      out << [TAG_EXTERNAL].pack("C") << u32(first) << u32(payload.bytesize)
    when :inline_comp
      payload = value_payload(c[:type], rec[:row][i])
      comp = rec[:comps][i]
      out << [TAG_INLINE_COMP].pack("C") << u32(payload.bytesize) << u16(comp.bytesize) << comp
    when :external_comp
      payload = value_payload(c[:type], rec[:row][i])
      comp = rec[:comps][i]
      first = write_overflow_chain(comp, cap, alloc, pages) # the chain carries the COMPRESSED block
      out << [TAG_EXTERNAL_COMP].pack("C") << u32(first) << u32(comp.bytesize) << u32(payload.bytesize)
    else
      out << encode_value(c[:type], rec[:row][i])
    end
  end
  out
end

def pack(sizes, cap)
  groups = []
  cur = []
  used = 0
  sizes.each_with_index do |sz, i|
    raise "item of size #{sz} exceeds page capacity #{cap}" if sz > cap

    if !cur.empty? && used + sz > cap
      groups << cur
      cur = []
      used = 0
    end
    cur << i
    used += sz
  end
  groups << cur
  groups
end

def write_meta(image, ps, slot, page_size, txid, root, page_count)
  off = slot * ps
  image[off, 4] = MAGIC
  image[off + 4, 2] = u16(VERSION)
  image[off + 8, 4] = u32(page_size)
  image[off + 12, 8] = u64(txid)
  image[off + 20, 4] = u32(root)
  image[off + 24, 4] = u32(page_count)
  image[off + 32, 4] = u32(crc32(image[off, 32]))
end

def write_page(image, ps, index, type, item_count, next_page, payload)
  off = index * ps
  image[off, 1] = [type].pack("C")
  image[off + 4, 4] = u32(item_count)
  image[off + 8, 4] = u32(next_page)
  image[off + PAGE_HEADER, payload.bytesize] = payload unless payload.empty?
  # The per-page checksum (v7) is computed last, over every byte but its own field at [12,16).
  image[off + 12, 4] = u32(page_crc(image.byteslice(off, ps)))
end

# --- size-driven B-tree (format.md "The per-table data B-tree") -------------
#
# Built the way the cores do: insert each (key, record) in ascending key order, splitting any
# node whose serialized payload exceeds the page capacity C. A node is a Hash
#   { keys: [binary-key, ...], recs: [record-bytes, ...], children: [node, ...] }
# A leaf has children == []; an interior has children.size == keys.size + 1. recs[i] is the full
# record bytes (key_len + key + values) for keys[i] — this B-tree stores a value with every key,
# separators included. Split point and the RECORD_MAX = C/2 cap mirror format.md exactly.

def node_leaf?(node) = node[:children].empty?

# `recs[i]` is the record PLAN for keys[i] — a hash { key:, row:, table:, ext:, size: } whose
# `size` is the on-disk (post-spill) record size (large-values.md §12). The tree splits on `size`;
# the actual pointer bytes (with allocated overflow pages) are emitted later by serialize_tree.
def node_payload(node)
  s = node[:recs].sum { |r| r[:size] }
  s += 4 * node[:children].size unless node_leaf?(node) # (N+1) child pointers
  s
end

# Split an overflowing node 2-way, promoting one separator: [:split, left, sep_key, sep_rec,
# right]. Split point (format.md "Split point"): right_edge (the just-inserted record / promoted
# separator is the node's last) takes the append rule m = min(m_append, N-2) with m_append =
# largest m in [1,N-1] with leftpayload(m) <= C; anywhere else splits balanced,
# m = min(m_balanced, m_append, N-2) with m_balanced = smallest m with 2*leftpayload(m) >= payload.
# This builder only inserts in ascending key order (build_tree), so right_edge is always true
# here — the balanced arm is implemented so the reference states the whole contract.
def split_node(node, cap, right_edge)
  payload = node_payload(node)
  return [:whole, node] if payload <= cap

  interior = !node_leaf?(node)
  n = node[:keys].size
  best = 1
  balanced = 0
  (1...n).each do |m|
    lp = (interior ? 4 * (m + 1) : 0) + node[:recs][0, m].sum { |r| r[:size] }
    best = m if lp <= cap
    balanced = m if balanced.zero? && 2 * lp >= payload
  end
  best = balanced if !right_edge && balanced.positive? && balanced < best
  m = [best, n - 2].min
  left = { keys: node[:keys][0, m], recs: node[:recs][0, m],
           children: interior ? node[:children][0, m + 1] : [] }
  right = { keys: node[:keys][(m + 1)..], recs: node[:recs][(m + 1)..],
            children: interior ? node[:children][(m + 1)..] : [] }
  [:split, left, node[:keys][m], node[:recs][m], right]
end

# Insert (key, rec) into the subtree, rebuilding (copy-on-write) and splitting up the path.
def tree_insert(node, key, rec, cap)
  i = node[:keys].bsearch_index { |k| k >= key } || node[:keys].size
  raise "duplicate key in fixture" if i < node[:keys].size && node[:keys][i] == key

  if node_leaf?(node)
    return split_node({ keys: node[:keys].dup.insert(i, key),
                        recs: node[:recs].dup.insert(i, rec), children: [] }, cap,
                      i == node[:keys].size)
  end
  res = tree_insert(node[:children][i], key, rec, cap)
  if res[0] == :split
    _, left, sk, sr, right = res
    children = node[:children].dup
    children[i] = left
    children.insert(i + 1, right)
    split_node({ keys: node[:keys].dup.insert(i, sk), recs: node[:recs].dup.insert(i, sr),
                 children: children }, cap, i == node[:keys].size)
  else
    children = node[:children].dup
    children[i] = res[1]
    [:whole, { keys: node[:keys], recs: node[:recs], children: children }]
  end
end

# Build a table's B-tree from its (key, record) pairs in key order. nil for an empty table.
def build_tree(pairs, cap)
  root = nil
  max = record_max(cap)
  pairs.each do |key, rec|
    # rec is a record plan; rec[:size] is the post-spill on-disk size. A record still over
    # RECORD_MAX after externalizing every spillable value is genuinely unsupported (0A000).
    raise "record of #{rec[:size]}B exceeds RECORD_MAX #{max} after spilling" if rec[:size] > max

    if root.nil?
      root = { keys: [key], recs: [rec], children: [] }
      next
    end
    res = tree_insert(root, key, rec, cap)
    root = res[0] == :split ? { keys: [res[2]], recs: [res[3]], children: [res[1], res[4]] } : res[1]
  end
  root
end

# Post-order page allocation: children before their parent (so a parent's child pointers
# reference already-allocated pages). Fills `pages[index] = [page_type, item_count, payload]`.
# Returns [root_page (0 for an empty tree), next free index].
def serialize_tree(root, next_index, cap, pages)
  return [0, next_index] if root.nil?

  child_pages = root[:children].map do |c|
    cp, next_index = serialize_tree(c, next_index, cap, pages)
    cp
  end
  index = next_index
  next_index += 1
  # Emit records, spilling external values to overflow pages allocated AFTER this node's index
  # (post-order traversal + column order → deterministic, golden-pinnable layout — large-values.md §12).
  alloc = lambda do
    i = next_index
    next_index += 1
    i
  end
  recs = root[:recs].map { |r| emit_record(r, cap, alloc, pages) }
  n = root[:keys].size
  pages[index] = if node_leaf?(root)
                   [PAGE_LEAF, n, recs.join.b, 0]
                 else
                   [PAGE_INTERIOR, n, child_pages.map { |cp| u32(cp) }.join.b + recs.join.b, 0]
                 end
  [index, next_index]
end

# A from-scratch image (format.md "Allocation & incremental commit"): the special case where
# every node is dirty — data B-trees post-order (per table, name order) from page 2, then the
# catalog chain, then both meta slots at txid 1.
def build_image(tables, page_size)
  ps = page_size
  cap = ps - PAGE_HEADER
  sorted = tables.sort_by { |t| t[:name].downcase }

  data_pages = {} # index => [page_type, item_count, payload]
  root_data = Array.new(sorted.size, 0)
  index_roots = Array.new(sorted.size) { [] }
  next_index = ROOT_PAGE
  sorted.each_with_index do |t, ti|
    pairs = table_entries(t).map do |key, row|
      forms, comps, size = plan_record(t, key, row, cap)
      [key, { key: key, row: row, table: t, forms: forms, comps: comps, size: size }]
    end
    root_data[ti], next_index = serialize_tree(build_tree(pairs, cap), next_index, cap, data_pages)
    # The table's index trees follow its data tree, in catalog (name) order (format.md
    # "Allocation & incremental commit" / "From-scratch image").
    (t[:indexes] || []).each do |ix|
      r, next_index = serialize_tree(build_tree(index_entries(t, ix), cap), next_index, cap, data_pages)
      index_roots[ti] << r
    end
  end

  cat_root = next_index
  cat_groups = pack(sorted.map.with_index { |t, ti| table_entry_bytes(t, root_data[ti], index_roots[ti]).bytesize }, cap)
  page_count = cat_root + cat_groups.size

  image = "\x00".b * (page_count * ps)
  write_meta(image, ps, 0, page_size, TXID, cat_root, page_count)
  write_meta(image, ps, 1, page_size, TXID, cat_root, page_count)

  data_pages.each { |index, (type, count, payload, nxt)| write_page(image, ps, index, type, count, nxt || 0, payload) }

  cat_groups.each_with_index do |group, gi|
    index = cat_root + gi
    nxt = gi + 1 < cat_groups.size ? index + 1 : 0
    payload = group.map { |ti| table_entry_bytes(sorted[ti], root_data[ti], index_roots[ti]) }.join.b
    write_page(image, ps, index, PAGE_CATALOG, group.size, nxt, payload)
  end

  image
end

# The bytes a fixture should contain (applying any torn-slot corruption).
def fixture_image(fx)
  image = build_image(fx[:tables], fx[:page_size])
  if fx[:corrupt_slot]
    off = fx[:corrupt_slot] * fx[:page_size] + 35 # last CRC byte of that slot
    image.setbyte(off, image.getbyte(off) ^ 0xFF)
  end
  image
end

# --- decoding (independent reader) ------------------------------------------

def take(buf, pos, n)
  raise "unexpected end of page data" if pos + n > buf.bytesize

  [buf.byteslice(pos, n), pos + n]
end

def read_meta(image, ps, slot)
  off = slot * ps
  return nil if off + ps > image.bytesize

  m = image.byteslice(off, ps)
  return nil unless m.byteslice(0, 4) == MAGIC
  return nil unless m.byteslice(4, 2).unpack1("n") == VERSION
  return nil unless m.getbyte(6).zero? && m.getbyte(7).zero?
  return nil unless m.byteslice(28, 4) == "\x00\x00\x00\x00".b
  return nil unless crc32(m.byteslice(0, 32)) == m.byteslice(32, 4).unpack1("N")

  { txid: m.byteslice(12, 8).unpack1("Q>"), root_page: m.byteslice(20, 4).unpack1("N") }
end

def select_meta(image, ps)
  a = read_meta(image, ps, 0)
  b = read_meta(image, ps, 1)
  return (b && b[:txid] > a[:txid] ? b : a) if a && b
  return a if a
  return b if b

  raise "no valid meta page"
end

def read_page(image, ps, index)
  off = index * ps
  raise "page index out of range" if off + ps > image.bytesize

  p = image.byteslice(off, ps)
  # Verify the per-page checksum (v7) before trusting any header field (format.md *Page header*).
  raise "page checksum mismatch (corrupted page)" unless page_crc(p) == p.byteslice(12, 4).unpack1("N")
  { type: p.getbyte(0), item_count: p.byteslice(4, 4).unpack1("N"),
    next_page: p.byteslice(8, 4).unpack1("N"), payload: p.byteslice(PAGE_HEADER, ps - PAGE_HEADER) }
end

def decode_table_entry(buf, pos)
  name_len, pos = take(buf, pos, 2)
  name, pos = take(buf, pos, name_len.unpack1("n"))
  col_count, pos = take(buf, pos, 2)
  columns = []
  col_count.unpack1("n").times do
    cnl, pos = take(buf, pos, 2)
    cname, pos = take(buf, pos, cnl.unpack1("n"))
    tc, pos = take(buf, pos, 1)
    flags, pos = take(buf, pos, 1)
    f = flags.getbyte(0)
    raise "reserved flag bit0 set (retired primary_key bit — v5)" if (f & 0b01) != 0

    type = CODETYPE.fetch(tc.getbyte(0))
    precision = nil
    scale = nil
    if type == "decimal"
      pb, pos = take(buf, pos, 2)
      sb, pos = take(buf, pos, 2)
      p = pb.unpack1("n")
      precision = p.zero? ? nil : p
      scale = p.zero? ? nil : sb.unpack1("n")
    end
    # The default value follows the typmod, present iff flags bit2 (same value codec as rows).
    default = :none
    default, pos = decode_value(type, buf, pos) if (f & 0b100) != 0
    columns << { name: cname, type: type, pk: false, not_null: (f & 0b10) != 0,
                 precision: precision, scale: scale, default: default }
  end
  # The primary key (v5): member ordinals in KEY order; pk membership marks the columns.
  pk = []
  pkc, pos = take(buf, pos, 2)
  pkc.unpack1("n").times do
    ob, pos = take(buf, pos, 2)
    pk << ob.unpack1("n")
  end
  pk.each { |i| columns[i][:pk] = true }
  # CHECK constraints (v4): (name, expression text) pairs in evaluation order.
  checks = []
  cc, pos = take(buf, pos, 2)
  cc.unpack1("n").times do
    nl, pos = take(buf, pos, 2)
    cname, pos = take(buf, pos, nl.unpack1("n"))
    el, pos = take(buf, pos, 2)
    expr, pos = take(buf, pos, el.unpack1("n"))
    checks << { name: cname, expr: expr }
  end
  # Secondary indexes (v5): name + key-column ordinals + the v6 flags byte (bit0 unique)
  # + root page, in name order.
  indexes = []
  ic, pos = take(buf, pos, 2)
  ic.unpack1("n").times do
    nl, pos = take(buf, pos, 2)
    iname, pos = take(buf, pos, nl.unpack1("n"))
    kc, pos = take(buf, pos, 2)
    cols = []
    kc.unpack1("n").times do
      ob, pos = take(buf, pos, 2)
      cols << ob.unpack1("n")
    end
    fb, pos = take(buf, pos, 1)
    raise "reserved index flag set (only bit0 unique is defined — v6)" if (fb.getbyte(0) & ~0b01) != 0

    rb, pos = take(buf, pos, 4)
    indexes << { name: iname, cols: cols, unique: (fb.getbyte(0) & 1) != 0, root_page: rb.unpack1("N") }
  end
  root, pos = take(buf, pos, 4)
  [{ name: name, columns: columns, pk: pk, checks: checks, indexes: indexes,
     root_data_page: root.unpack1("N") }, pos]
end

# Read one value via the value codec (inverse of encode_value): a presence tag, then — when
# present — the type's body. 0x01 = NULL. Shared by row records and the catalog default.
# Decode a decimal value's body (flags + u16 scale + u16 ndigits + base-10^4 groups). Shared by the
# inline decimal branch and by external reconstruction (a spilled decimal's payload is this body).
def decode_decimal_body(buf, pos)
  fb, pos = take(buf, pos, 1)
  scb, pos = take(buf, pos, 2)
  ndb, pos = take(buf, pos, 2)
  coeff = 0
  ndb.unpack1("n").times do
    gb, pos = take(buf, pos, 2)
    coeff = coeff * 10_000 + gb.unpack1("n")
  end
  [render_decimal((fb.getbyte(0) & 1) != 0, scb.unpack1("n"), coeff), pos]
end

# Reconstruct a value from the P(v) content gathered from its overflow chain (large-values.md §12).
def value_from_payload(type, payload)
  case type
  when "text" then payload.dup.force_encoding("UTF-8")
  when "bytea" then payload.dup.force_encoding("ASCII-8BIT")
  when "decimal" then decode_decimal_body(payload, 0).first
  else raise "a non-spillable type was stored external"
  end
end

# Gather `len` bytes of an external value's payload by following its overflow chain from `first`:
# each page is page_type 4, carries item_count payload bytes, chained via next_page (0 terminates).
def read_overflow_chain(first, len, fetch)
  out = +"".b
  p = first
  while out.bytesize < len
    raise "overflow chain ended before the value length" if p.zero?

    pg = fetch.call(p)
    raise "expected an overflow page" unless pg[:type] == PAGE_OVERFLOW

    n = pg[:item_count]
    raise "overflow page slab out of range" if n.zero? || n > pg[:payload].bytesize || out.bytesize + n > len

    out << pg[:payload].byteslice(0, n)
    p = pg[:next_page]
  end
  out
end

# Read one value via the value codec (inverse of encode_value/emit_record): a presence tag, then —
# when present-inline — the type's body, or — when external (0x02) — a pointer whose payload is
# gathered from the overflow chain via `fetch` and reconstructed by type (large-values.md §12).
# `fetch` is nil only where no value can be external (a catalog default). 0x01 = NULL.
def decode_value(type, buf, pos, fetch = nil)
  tag, pos = take(buf, pos, 1)
  t = tag.getbyte(0)
  return [nil, pos] if t == 0x01
  if t == TAG_EXTERNAL
    fpb, pos = take(buf, pos, 4)
    lnb, pos = take(buf, pos, 4)
    raise "external value with no overflow reader" if fetch.nil?

    return [value_from_payload(type, read_overflow_chain(fpb.unpack1("N"), lnb.unpack1("N"), fetch)), pos]
  end
  if t == TAG_INLINE_COMP
    rlb, pos = take(buf, pos, 4)
    clb, pos = take(buf, pos, 2)
    comp, pos = take(buf, pos, clb.unpack1("n"))
    return [value_from_payload(type, LZ4.decompress(comp, rlb.unpack1("N"))), pos]
  end
  if t == TAG_EXTERNAL_COMP
    fpb, pos = take(buf, pos, 4)
    slb, pos = take(buf, pos, 4)
    rlb, pos = take(buf, pos, 4)
    raise "external value with no overflow reader" if fetch.nil?

    comp = read_overflow_chain(fpb.unpack1("N"), slb.unpack1("N"), fetch)
    return [value_from_payload(type, LZ4.decompress(comp, rlb.unpack1("N"))), pos]
  end
  raise "invalid value presence tag" unless t.zero?

  case type
  when "text"
    len, pos = take(buf, pos, 2)
    sb, pos = take(buf, pos, len.unpack1("n"))
    [sb.dup.force_encoding("UTF-8"), pos]
  when "boolean"
    bb, pos = take(buf, pos, 1)
    [bb.getbyte(0) == 1, pos]
  when "decimal"
    decode_decimal_body(buf, pos)
  when "bytea"
    len, pos = take(buf, pos, 2)
    bb, pos = take(buf, pos, len.unpack1("n"))
    [bb.dup.force_encoding("ASCII-8BIT"), pos] # raw bytes, no UTF-8 assertion
  when "uuid"
    ub, pos = take(buf, pos, 16) # fixed 16 bytes, no length prefix
    [uuid_from_bytes(ub), pos]
  else
    vb, pos = take(buf, pos, WIDTH.fetch(type))
    [decode_int(WIDTH.fetch(type), vb), pos]
  end
end

def decode_record(columns, buf, pos, fetch)
  key_len, pos = take(buf, pos, 2)
  _key, pos = take(buf, pos, key_len.unpack1("n"))
  row = []
  columns.each do |c|
    v, pos = decode_value(c[:type], buf, pos, fetch)
    row << v
  end
  [row, pos]
end

# In-order walk of a table's B-tree -> rows in ascending key order (format.md interior layout:
# (N+1) child pointers, then N records). Independent of how the tree was built. An external value's
# chain is followed through `fetch` (large-values.md §12).
def read_tree_rows(image, ps, root_page, columns)
  rows = []
  fetch = ->(p) { read_page(image, ps, p) }
  walk = lambda do |idx|
    return if idx.zero?

    pg = read_page(image, ps, idx)
    case pg[:type]
    when PAGE_LEAF
      pos = 0
      pg[:item_count].times do
        row, pos = decode_record(columns, pg[:payload], pos, fetch)
        rows << row
      end
    when PAGE_INTERIOR
      n = pg[:item_count]
      children = (0..n).map { |i| pg[:payload].byteslice(i * 4, 4).unpack1("N") }
      pos = 4 * (n + 1)
      n.times do |i|
        walk.call(children[i])
        row, pos = decode_record(columns, pg[:payload], pos, fetch)
        rows << row
      end
      walk.call(children[n])
    else
      raise "expected a B-tree node page, got type #{pg[:type]}"
    end
  end
  walk.call(root_page)
  rows
end

# In-order walk of an INDEX B-tree -> entry keys in ascending order. An index record has an
# empty payload, so only the key is read (format.md "Index trees").
def read_tree_keys(image, ps, root_page)
  keys = []
  walk = lambda do |idx|
    return if idx.zero?

    pg = read_page(image, ps, idx)
    read_rec = lambda do |pos|
      klb, pos = take(pg[:payload], pos, 2)
      key, pos = take(pg[:payload], pos, klb.unpack1("n"))
      keys << key
      pos
    end
    case pg[:type]
    when PAGE_LEAF
      pos = 0
      pg[:item_count].times { pos = read_rec.call(pos) }
    when PAGE_INTERIOR
      n = pg[:item_count]
      children = (0..n).map { |i| pg[:payload].byteslice(i * 4, 4).unpack1("N") }
      pos = 4 * (n + 1)
      n.times do |i|
        walk.call(children[i])
        pos = read_rec.call(pos)
      end
      walk.call(children[n])
    else
      raise "expected a B-tree node page, got type #{pg[:type]}"
    end
  end
  walk.call(root_page)
  keys
end

def decode_image(image)
  ps = image.byteslice(8, 4).unpack1("N")
  meta = select_meta(image, ps)
  tables = []
  cat = meta[:root_page]
  while cat != 0
    pg = read_page(image, ps, cat)
    raise "expected a catalog page" unless pg[:type] == PAGE_CATALOG

    pos = 0
    pg[:item_count].times do
      entry, pos = decode_table_entry(pg[:payload], pos)
      rows = read_tree_rows(image, ps, entry[:root_data_page], entry[:columns])
      indexes = entry[:indexes].map do |ix|
        { name: ix[:name], cols: ix[:cols],
          entries: read_tree_keys(image, ps, ix[:root_page]).map { |k| k.unpack1("H*") } }
      end
      tables << { name: entry[:name], columns: entry[:columns], pk: entry[:pk],
                  checks: entry[:checks], indexes: indexes, rows: rows }
    end
    cat = pg[:next_page]
  end
  tables
end

# The logical content a fixture should decode to (torn fixtures decode to the
# underlying pk_table content via the valid slot).
def expected_tables(fx)
  fx[:tables].sort_by { |t| t[:name].downcase }.map do |t|
    { name: t[:name],
      columns: t[:columns].map do |c|
        { name: c[:name], type: c[:type], pk: c[:pk], not_null: c[:not_null],
          precision: c[:precision], scale: c[:scale], default: c[:default] }
      end,
      pk: pk_order(t),
      checks: (t[:checks] || []).map { |ck| { name: ck[:name], expr: ck[:expr] } },
      indexes: (t[:indexes] || []).map do |ix|
        { name: ix[:name], cols: ix[:cols],
          entries: index_entries(t, ix).map { |key, _| key.unpack1("H*") } }
      end,
      rows: table_entries(t).map { |_key, row| row } }
  end
end

# --- LZ4 byte vectors (lz4.md §4) --------------------------------------------
#
# Pins `input -> exact compressed bytes` for the lz4.md §2 encoder. Generated into
# lz4_vectors.toml; every core's codec test must reproduce each `compressed` byte-for-byte
# and decode it back to `input`. Inputs chosen to cover each encoder/format branch.
LZ4_VECTORS = [
  { name: "empty", input: "".b },                                  # -> the lone 0x00 token
  { name: "one_byte", input: "a".b },
  { name: "twelve_below_mflimit", input: "abcdefghijkl".b },       # 12 B: literals only
  { name: "thirteen_run", input: ("a" * 13).b },                   # smallest matchable input
  { name: "run_32", input: ("a" * 32).b },                         # overlapping match (offset 1)
  { name: "pattern_abc", input: ("abc" * 40).b },                  # multi-byte period
  { name: "long_literals_then_run", input: (filler_text(40) + ("z" * 60)).b }, # lit-extension + match
  { name: "long_run_extension", input: ("y" * 1000).b },           # match-length 255-extensions
  { name: "incompressible_64", input: filler_bytes(64) },          # high entropy: expands
  { name: "mixed_text", input: ("the quick brown fox jumps over the lazy dog " * 6).b },
  { name: "trailing_tail", input: (("b" * 20) + "QRSTUVWX").b }    # run ending inside the 12-byte tail
].freeze

def lz4_vectors_path = File.join(__dir__, "lz4_vectors.toml")

def lz4_vectors_toml
  out = +"# LZ4 block codec byte vectors — GENERATED by `ruby spec/fileformat/verify.rb --generate`\n"
  out << "# (the reference encoder in lz4.rb; the algorithm is pinned in lz4.md §2). Each core's\n"
  out << "# codec must reproduce `compressed_hex` from `input_hex` byte-for-byte, and decode it\n"
  out << "# back. Do not edit by hand.\n\nschema_version = 1\n"
  LZ4_VECTORS.each do |v|
    comp = LZ4.compress(v[:input])
    out << "\n[[vector]]\n"
    out << "name = \"#{v[:name]}\"\n"
    out << "input_hex = \"#{v[:input].unpack1('H*')}\"\n"
    out << "compressed_hex = \"#{comp.unpack1('H*')}\"\n"
  end
  out
end

def verify_lz4_vectors
  fail!("lz4_vectors.toml: missing (run `ruby spec/fileformat/verify.rb --generate`)") unless File.exist?(lz4_vectors_path)
  unless File.read(lz4_vectors_path) == lz4_vectors_toml
    fail!("lz4_vectors.toml: differs from the reference encoder (regenerate or fix the codec)")
  end

  LZ4_VECTORS.each do |v|
    comp = LZ4.compress(v[:input])
    round = LZ4.decompress(comp, v[:input].bytesize)
    fail!("lz4 vector #{v[:name]}: decompress(compress(x)) != x") unless round == v[:input]
  end
end

# --- driver -----------------------------------------------------------------

def fail!(msg)
  warn "FAIL: #{msg}"
  exit 1
end

def fixtures_dir = File.join(__dir__, "fixtures")

def generate
  dir = fixtures_dir
  Dir.mkdir(dir) unless Dir.exist?(dir)
  FIXTURES.each do |fx|
    File.binwrite(File.join(dir, fx[:file]), fixture_image(fx))
    puts "wrote #{fx[:file]} (#{fixture_image(fx).bytesize} bytes)"
  end
  File.write(lz4_vectors_path, lz4_vectors_toml)
  puts "wrote lz4_vectors.toml (#{LZ4_VECTORS.size} vectors)"
  puts "Generated #{FIXTURES.size} fixtures in #{dir}"
end

def verify
  fail!("CRC32 self-test failed") unless crc32("123456789") == 0xCBF43926

  FIXTURES.each do |fx|
    path = File.join(fixtures_dir, fx[:file])
    fail!("#{fx[:file]}: missing (run `ruby spec/fileformat/verify.rb --generate`)") unless File.exist?(path)

    on_disk = File.binread(path)
    reference = fixture_image(fx)
    unless on_disk == reference
      fail!("#{fx[:file]}: bytes differ from the reference encoder " \
            "(disk #{on_disk.bytesize}B vs reference #{reference.bytesize}B)")
    end

    decoded = decode_image(on_disk)
    want = expected_tables(fx)
    fail!("#{fx[:file]}: decoded #{decoded.size} tables, expected #{want.size}") unless decoded.size == want.size
    decoded.each_with_index do |t, i|
      fail!("#{fx[:file]}: table #{i} mismatch\n  got:  #{t.inspect}\n  want: #{want[i].inspect}") unless t == want[i]
    end
  end

  verify_lz4_vectors
  puts "OK: #{FIXTURES.size} file-format fixtures verified (byte-exact + independent decode); " \
       "#{LZ4_VECTORS.size} LZ4 vectors verified"
end

if ARGV.include?("--generate")
  generate
else
  verify
end
