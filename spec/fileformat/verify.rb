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
VERSION = 1
PAGE_HEADER = 12
ROOT_PAGE = 2
TXID = 1

WIDTH = { "int16" => 2, "int32" => 4, "int64" => 8 }.freeze
TYPECODE = { "int16" => 1, "int32" => 2, "int64" => 3, "text" => 4, "boolean" => 5, "decimal" => 6 }.freeze
CODETYPE = TYPECODE.invert.freeze

# --- declarative fixtures (mirror what the cores build via SQL) --------------

# A column. `precision`/`scale` are the decimal typmod (only meaningful for type "decimal";
# nil = an unconstrained `numeric` column, or a non-decimal column). Always carried so the
# decode-side column hash compares equal (format.md stores the typmod only for type_code 6).
def col(name, type, pk: false, precision: nil, scale: nil)
  { name: name, type: type, pk: pk, not_null: pk, precision: precision, scale: scale }
end

PK_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("v", "int16")],
  # 20 rows so the data spans >1 page at page_size 256; id 3 has a NULL value.
  rows: (1..20).map { |i| [i, i == 3 ? nil : i * 10] }
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

FIXTURES = [
  { file: "empty_db.jed",        page_size: 256, tables: [] },
  { file: "one_table_empty.jed", page_size: 256,
    tables: [{ name: "t", columns: [col("id", "int32", pk: true), col("v", "int16")], rows: [] }] },
  { file: "pk_table.jed",        page_size: 256, tables: [PK_TABLE] },
  { file: "text_table.jed",      page_size: 256, tables: [TEXT_TABLE] },
  { file: "bool_table.jed",      page_size: 256, tables: [BOOL_TABLE] },
  { file: "decimal_table.jed",   page_size: 256, tables: [DECIMAL_TABLE] },
  { file: "nopk_table.jed",      page_size: 256,
    tables: [{ name: "r", columns: [col("a", "int16"), col("b", "int64")],
               rows: [[7, 70], [8, 80], [9, 90]] }] },
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
# Integers reuse the order-preserving int bytes; text diverges to a compact u16 byte-length
# + UTF-8 bytes (collation C, verbatim); boolean is a single bool-byte 0x00 false / 0x01
# true (format.md "Value codec").
def encode_value(type, v)
  return "\x01".b if v.nil?

  case type
  when "text"
    bytes = v.b
    "\x00".b + u16(bytes.bytesize) + bytes
  when "boolean"
    "\x00".b + (v ? "\x01".b : "\x00".b)
  when "decimal"
    "\x00".b + encode_decimal(v)
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
    flags = 0
    flags |= 0b01 if c[:pk]
    flags |= 0b10 if c[:not_null]
    out << [flags].pack("C")
    # A decimal column appends its typmod (precision, scale) — only for type_code 6, so
    # non-decimal entries are byte-unchanged. precision 0 = unconstrained numeric.
    out << u16(c[:precision] || 0) << u16(c[:scale] || 0) if c[:type] == "decimal"
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
            encode_int(WIDTH.fetch(table[:columns][pk_idx][:type]), row[pk_idx])
          else
            encode_int(8, i)
          end
    [key, row]
  end
  pairs.sort_by { |key, _| key } # String#<=> is bytewise == memcmp order
end

def record_bytes(table, key, row)
  out = +"".b
  out << u16(key.bytesize) << key
  table[:columns].each_with_index { |c, i| out << encode_value(c[:type], row[i]) }
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

def build_image(tables, page_size)
  ps = page_size
  cap = ps - PAGE_HEADER
  sorted = tables.sort_by { |t| t[:name].downcase }

  records = sorted.map { |t| table_entries(t).map { |key, row| record_bytes(t, key, row) } }

  cat_groups = pack(sorted.map { |t| table_entry_bytes(t, 0).bytesize }, cap)
  next_index = ROOT_PAGE + cat_groups.size
  root_data = Array.new(sorted.size, 0)
  data_groups = Array.new(sorted.size) { [] }
  sorted.each_index do |ti|
    next if records[ti].empty?

    g = pack(records[ti].map(&:bytesize), cap)
    root_data[ti] = next_index
    next_index += g.size
    data_groups[ti] = g
  end
  page_count = next_index

  image = "\x00".b * (page_count * ps)
  write_meta(image, ps, 0, page_size, TXID, ROOT_PAGE, page_count)
  write_meta(image, ps, 1, page_size, TXID, ROOT_PAGE, page_count)

  cat_groups.each_with_index do |group, gi|
    index = ROOT_PAGE + gi
    nxt = gi + 1 < cat_groups.size ? index + 1 : 0
    payload = group.map { |ti| table_entry_bytes(sorted[ti], root_data[ti]) }.join.b
    write_page(image, ps, index, 1, group.size, nxt, payload)
  end

  sorted.each_index do |ti|
    data_groups[ti].each_with_index do |group, gi|
      index = root_data[ti] + gi
      nxt = gi + 1 < data_groups[ti].size ? index + 1 : 0
      payload = group.map { |ri| records[ti][ri] }.join.b
      write_page(image, ps, index, 2, group.size, nxt, payload)
    end
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
    columns << { name: cname, type: type, pk: (f & 0b01) != 0, not_null: (f & 0b10) != 0,
                 precision: precision, scale: scale }
  end
  root, pos = take(buf, pos, 4)
  [{ name: name, columns: columns, root_data_page: root.unpack1("N") }, pos]
end

def decode_record(columns, buf, pos)
  key_len, pos = take(buf, pos, 2)
  _key, pos = take(buf, pos, key_len.unpack1("n"))
  row = []
  columns.each do |c|
    tag, pos = take(buf, pos, 1)
    if tag.getbyte(0).zero? # 0x00 = present, 0x01 = NULL
      case c[:type]
      when "text"
        len, pos = take(buf, pos, 2)
        sb, pos = take(buf, pos, len.unpack1("n"))
        row << sb.dup.force_encoding("UTF-8")
      when "boolean"
        bb, pos = take(buf, pos, 1)
        row << (bb.getbyte(0) == 1)
      when "decimal"
        fb, pos = take(buf, pos, 1)
        scb, pos = take(buf, pos, 2)
        ndb, pos = take(buf, pos, 2)
        coeff = 0
        ndb.unpack1("n").times do
          gb, pos = take(buf, pos, 2)
          coeff = coeff * 10_000 + gb.unpack1("n")
        end
        row << render_decimal((fb.getbyte(0) & 1) != 0, scb.unpack1("n"), coeff)
      else
        vb, pos = take(buf, pos, WIDTH.fetch(c[:type]))
        row << decode_int(WIDTH.fetch(c[:type]), vb)
      end
    else
      row << nil
    end
  end
  [row, pos]
end

def decode_image(image)
  ps = image.byteslice(8, 4).unpack1("N")
  meta = select_meta(image, ps)
  tables = []
  cat = meta[:root_page]
  while cat != 0
    pg = read_page(image, ps, cat)
    raise "expected a catalog page" unless pg[:type] == 1

    pos = 0
    pg[:item_count].times do
      entry, pos = decode_table_entry(pg[:payload], pos)
      rows = []
      dp = entry[:root_data_page]
      while dp != 0
        d = read_page(image, ps, dp)
        raise "expected a data page" unless d[:type] == 2

        dpos = 0
        d[:item_count].times do
          rec, dpos = decode_record(entry[:columns], d[:payload], dpos)
          rows << rec
        end
        dp = d[:next_page]
      end
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
          precision: c[:precision], scale: c[:scale] }
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
