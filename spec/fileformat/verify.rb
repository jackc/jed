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
VERSION = 3 # format_version 3: + out-of-line overflow pages for large values (large-values.md §12)
PAGE_HEADER = 12
ROOT_PAGE = 2  # the catalog root of a *fresh empty* db; relocatable thereafter (meta.root_page)
TXID = 1
PAGE_CATALOG = 1
PAGE_LEAF = 2
PAGE_INTERIOR = 3
PAGE_OVERFLOW = 4 # an out-of-line value slab, chained by next_page (large-values.md §12)

# Value-codec presence tag for a present EXTERNAL value: the body is a pointer (u32 first_page +
# u32 payload_len) into an overflow chain (large-values.md §12). 0x00 present-inline / 0x01 NULL
# unchanged; 0x03 / 0x04 reserved for compression. EXTERNAL_PTR_LEN is the pointer's in-record size.
TAG_EXTERNAL = 0x02
EXTERNAL_PTR_LEN = 1 + 4 + 4

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

PK_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("v", "int16")],
  # 20 rows: each record is 14 bytes, so a 256-byte page (cap 244) overflows at 18 rows and
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

# A table with large text + bytea values that must spill OUT-OF-LINE to overflow pages
# (large-values.md §12). At page_size 256 the per-record cap is RECORD_MAX = (256-12-12)/2 = 116,
# so a value of ~600/300 bytes exceeds it and is externalized: the record holds a fixed pointer
# (tag 0x02 + u32 first_page + u32 len) and the bytes live in a chain of page_type-4 slabs (244
# bytes each). Row 1's text (600 B → 3 slabs) and bytea (300 B → 2 slabs) both spill; row 2's
# values stay inline; row 3 is NULL/NULL. Exercises multi-page chains, multi-column spill, and the
# inline+external mix in one leaf. The PK stays int32.
OVERFLOW_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("body", "text"), col("blob", "bytea")],
  rows: [[1, "x" * 600, (["ab"].pack("H*") * 300)], [2, "small", ["cafe"].pack("H*")], [3, nil, nil]]
}.freeze

FIXTURES = [
  { file: "empty_db.jed",        page_size: 256, tables: [] },
  { file: "overflow_table.jed",  page_size: 256, tables: [OVERFLOW_TABLE] },
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

def table_entry_bytes(table, root_data_page)
  out = +"".b
  out << u16(table[:name].bytesize) << table[:name].b
  out << u16(table[:columns].size)
  table[:columns].each do |c|
    out << u16(c[:name].bytesize) << c[:name].b
    out << [TYPECODE.fetch(c[:type])].pack("C")
    has_default = c[:default] != :none
    flags = 0
    flags |= 0b01 if c[:pk]
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
  out << u32(root_data_page)
  out
end

# (key, row) pairs in stored (encoded-key) order. PK tables key on the PK column;
# a no-PK table keys on a synthetic int64 rowid = insertion index (executor.rs).
def table_entries(table)
  pk_idx = table[:columns].index { |c| c[:pk] }
  pairs = table[:rows].each_with_index.map do |row, i|
    key = if pk_idx
            pk_type = table[:columns][pk_idx][:type]
            # uuid is the first non-integer key: its key is the bare 16 bytes (uuid-raw16),
            # not the sign-flipped int encoding. A PK is NOT NULL, so no presence tag.
            if pk_type == "uuid"
              uuid_to_bytes(row[pk_idx])
            else
              encode_int(WIDTH.fetch(pk_type), row[pk_idx])
            end
          else
            encode_int(8, i)
          end
    [key, row]
  end
  pairs.sort_by { |key, _| key } # String#<=> is bytewise == memcmp order
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

# Decide each column's disposition for a record: [is_external per column, on-disk record size].
# Spill only when forced (record > RECORD_MAX); externalize the largest spillable values first,
# ties by column index — deterministic, mirrors the cores' planDispositions (large-values.md §12).
def plan_record(table, key, row, cap)
  inline = table[:columns].each_with_index.map { |c, i| encode_value(c[:type], row[i]).bytesize }
  ext = Array.new(table[:columns].size, false)
  size = 2 + key.bytesize + inline.sum
  max = record_max(cap)
  return [ext, size] if size <= max

  cand = (0...table[:columns].size).select { |i| spillable?(table[:columns][i][:type]) && inline[i] > EXTERNAL_PTR_LEN }
  cand.sort_by! { |i| [-inline[i], i] } # largest first; ties by ascending index (stable, deterministic)
  cand.each do |i|
    break if size <= max

    ext[i] = true
    size = size - inline[i] + EXTERNAL_PTR_LEN
  end
  [ext, size]
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
# pages drawn via `alloc` (large-values.md §12). `rec` is a plan hash { key:, row:, table:, ext: }.
def emit_record(rec, cap, alloc, pages)
  out = +"".b
  out << u16(rec[:key].bytesize) << rec[:key]
  rec[:table][:columns].each_with_index do |c, i|
    if rec[:ext][i]
      payload = value_payload(c[:type], rec[:row][i])
      first = write_overflow_chain(payload, cap, alloc, pages)
      out << [TAG_EXTERNAL].pack("C") << u32(first) << u32(payload.bytesize)
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
# right]. Split point m = min(largest m in [1,N-1] with leftpayload(m) <= C, N-2).
def split_node(node, cap)
  return [:whole, node] if node_payload(node) <= cap

  interior = !node_leaf?(node)
  n = node[:keys].size
  best = 1
  (1...n).each do |m|
    lp = (interior ? 4 * (m + 1) : 0) + node[:recs][0, m].sum { |r| r[:size] }
    best = m if lp <= cap
  end
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
                        recs: node[:recs].dup.insert(i, rec), children: [] }, cap)
  end
  res = tree_insert(node[:children][i], key, rec, cap)
  if res[0] == :split
    _, left, sk, sr, right = res
    children = node[:children].dup
    children[i] = left
    children.insert(i + 1, right)
    split_node({ keys: node[:keys].dup.insert(i, sk), recs: node[:recs].dup.insert(i, sr),
                 children: children }, cap)
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
  next_index = ROOT_PAGE
  sorted.each_with_index do |t, ti|
    pairs = table_entries(t).map do |key, row|
      ext, size = plan_record(t, key, row, cap)
      [key, { key: key, row: row, table: t, ext: ext, size: size }]
    end
    root_data[ti], next_index = serialize_tree(build_tree(pairs, cap), next_index, cap, data_pages)
  end

  cat_root = next_index
  cat_groups = pack(sorted.map.with_index { |t, ti| table_entry_bytes(t, root_data[ti]).bytesize }, cap)
  page_count = cat_root + cat_groups.size

  image = "\x00".b * (page_count * ps)
  write_meta(image, ps, 0, page_size, TXID, cat_root, page_count)
  write_meta(image, ps, 1, page_size, TXID, cat_root, page_count)

  data_pages.each { |index, (type, count, payload, nxt)| write_page(image, ps, index, type, count, nxt || 0, payload) }

  cat_groups.each_with_index do |group, gi|
    index = cat_root + gi
    nxt = gi + 1 < cat_groups.size ? index + 1 : 0
    payload = group.map { |ti| table_entry_bytes(sorted[ti], root_data[ti]) }.join.b
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
    columns << { name: cname, type: type, pk: (f & 0b01) != 0, not_null: (f & 0b10) != 0,
                 precision: precision, scale: scale, default: default }
  end
  root, pos = take(buf, pos, 4)
  [{ name: name, columns: columns, root_data_page: root.unpack1("N") }, pos]
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
      tables << { name: entry[:name], columns: entry[:columns], rows: rows }
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
      rows: table_entries(t).map { |_key, row| row } }
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

  puts "OK: #{FIXTURES.size} file-format fixtures verified (byte-exact + independent decode)"
end

if ARGV.include?("--generate")
  generate
else
  verify
end
