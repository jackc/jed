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

MAGIC = "ABDB".b
VERSION = 1
PAGE_HEADER = 12
ROOT_PAGE = 2
TXID = 1

WIDTH = { "int16" => 2, "int32" => 4, "int64" => 8 }.freeze
TYPECODE = { "int16" => 1, "int32" => 2, "int64" => 3 }.freeze
CODETYPE = TYPECODE.invert.freeze

# --- declarative fixtures (mirror what the cores build via SQL) --------------

def col(name, type, pk: false)
  { name: name, type: type, pk: pk, not_null: pk } # PRIMARY KEY => NOT NULL
end

PK_TABLE = {
  name: "t",
  columns: [col("id", "int32", pk: true), col("v", "int16")],
  # 20 rows so the data spans >1 page at page_size 256; id 3 has a NULL value.
  rows: (1..20).map { |i| [i, i == 3 ? nil : i * 10] }
}.freeze

FIXTURES = [
  { file: "empty_db.adb",        page_size: 256, tables: [] },
  { file: "one_table_empty.adb", page_size: 256,
    tables: [{ name: "t", columns: [col("id", "int32", pk: true), col("v", "int16")], rows: [] }] },
  { file: "pk_table.adb",        page_size: 256, tables: [PK_TABLE] },
  { file: "nopk_table.adb",      page_size: 256,
    tables: [{ name: "r", columns: [col("a", "int16"), col("b", "int64")],
               rows: [[7, 70], [8, 80], [9, 90]] }] },
  # Torn-write fallback: same image as pk_table, with one meta slot's CRC smashed.
  { file: "torn_meta_slot0.adb", page_size: 256, tables: [PK_TABLE], corrupt_slot: 0 },
  { file: "torn_meta_slot1.adb", page_size: 256, tables: [PK_TABLE], corrupt_slot: 1 }
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

# value codec: presence tag + (when present) the integer bytes.
def encode_value(width, v)
  return "\x00".b if v.nil?

  "\x01".b + encode_int(width, v)
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
  table[:columns].each_with_index { |c, i| out << encode_value(WIDTH.fetch(c[:type]), row[i]) }
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
    columns << { name: cname, type: CODETYPE.fetch(tc.getbyte(0)),
                 pk: (f & 0b01) != 0, not_null: (f & 0b10) != 0 }
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
    if tag.getbyte(0).zero?
      row << nil
    else
      vb, pos = take(buf, pos, WIDTH.fetch(c[:type]))
      row << decode_int(WIDTH.fetch(c[:type]), vb)
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
      columns: t[:columns].map { |c| { name: c[:name], type: c[:type], pk: c[:pk], not_null: c[:not_null] } },
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
