#!/usr/bin/env ruby
# frozen_string_literal: true

# Independent reference codec for the page-service wire contract (README.md). It checks the message
# catalog's shape, encodes every [[frame]] and [[owner_record]] fixture from its declarative fields
# and compares the bytes, decodes the bytes back to the same fields, and checks that every
# [[malformed]] frame is rejected for the stated kind of fault. So the fixtures are pinned by a
# fourth implementation, not self-certified by the cores that also read them (CLAUDE.md §8).
#
#   ruby spec/pageservice/verify.rb              # verify
#   ruby spec/pageservice/verify.rb --generate   # rewrite every frame/owner_record `hex` from its fields
#   (or: mise run verify)

require_relative "../../scripts/lib/bundle_setup"
require "toml-rb"
require "zlib"

DIR = __dir__
CATALOG = TomlRB.load_file(File.join(DIR, "messages.toml"))
FIXTURES_PATH = File.join(DIR, "fixtures.toml")
FIXTURES = TomlRB.load_file(FIXTURES_PATH)

MAX_FRAME = CATALOG.fetch("max_frame_length")
TYPES = %w[u8 u16 u32 u64 bool id16 sqlstate string page u32_list page_list indexed_page_list].freeze

class Malformed < StandardError
  attr_reader :kind

  def initialize(kind, message)
    @kind = kind
    super("#{kind}: #{message}")
  end
end

def fail!(message)
  warn "FAIL: #{message}"
  exit 1
end

# --- catalog shape --------------------------------------------------------------------------------

MESSAGES = CATALOG.fetch("message").map do |m|
  fields = m.fetch("fields").map do |f|
    name, type = f.split(":", 2)
    fail!("#{m['name']}: bad field #{f.inspect}") unless name&.match?(/\A[a-z_]+\z/) && TYPES.include?(type)
    [name, type]
  end
  m.merge("fields" => fields)
end
BY_NAME = MESSAGES.to_h { |m| [m.fetch("name"), m] }
BY_CODE = MESSAGES.to_h { |m| [m.fetch("code"), m] }

def check_catalog
  fail!("duplicate message names") unless BY_NAME.size == MESSAGES.size
  fail!("duplicate message codes") unless BY_CODE.size == MESSAGES.size
  MESSAGES.each do |m|
    name, code, direction = m.values_at("name", "code", "direction")
    case direction
    when "request"
      fail!("#{name}: request codes are 0x01-0x3f") unless (0x01..0x3f).cover?(code)
      reply = m.fetch("reply")
      next if %w[none reserved].include?(reply)

      target = BY_NAME[reply] or fail!("#{name}: unknown reply #{reply}")
      fail!("#{name}: reply #{reply} is not a reply") unless target["direction"] == "reply"
    when "reply"
      fail!("#{name}: reply codes are 0x40-0x7d or 0x7f") unless (0x40..0x7d).cover?(code) || code == 0x7f
    when "notice"
      fail!("#{name}: the notice code is 0x7e") unless code == 0x7e
    else
      fail!("#{name}: unknown direction #{direction}")
    end
    fail!("#{name}: phase must be 2..7") unless (2..7).cover?(m.fetch("phase"))
    fail!("#{name}: duplicate field") unless m["fields"].map(&:first).uniq.size == m["fields"].size
  end
end

# --- field text form → value ----------------------------------------------------------------------

def parse_page(text, page_size)
  prefix, fill = text.split("/", 2)
  fail!("page #{text.inspect}: expected <prefix hex>/<fill hex>") unless fill&.match?(/\A\h\h\z/)
  bytes = [prefix].pack("H*")
  fail!("page prefix longer than page_size") if bytes.bytesize > page_size
  bytes + ([fill.to_i(16)].pack("C") * (page_size - bytes.bytesize))
end

def parse_value(type, text, page_size)
  case type
  when "u8", "u16", "u32", "u64" then Integer(text, 10)
  when "bool" then { "true" => true, "false" => false }.fetch(text)
  when "id16"
    fail!("id16 #{text.inspect}") unless text.match?(/\A\h{32}\z/)
    [text].pack("H*")
  when "sqlstate", "string" then text
  when "page" then parse_page(text, page_size)
  when "u32_list" then text.empty? ? [] : text.split(",").map { |v| Integer(v, 10) }
  when "page_list" then text.empty? ? [] : text.split(";").map { |p| parse_page(p, page_size) }
  when "indexed_page_list"
    text.empty? ? [] : text.split(";").map do |item|
      index, page = item.split("=", 2)
      [Integer(index, 10), parse_page(page, page_size)]
    end
  end
end

# --- encoder --------------------------------------------------------------------------------------

LIMITS = { "u8" => 0xff, "u16" => 0xffff, "u32" => 0xffff_ffff, "u64" => 0xffff_ffff_ffff_ffff }.freeze
PACK = { "u8" => "C", "u16" => "n", "u32" => "N", "u64" => "Q>" }.freeze

def encode_value(type, value, page_size)
  case type
  when "u8", "u16", "u32", "u64"
    fail!("#{value} out of range for #{type}") unless (0..LIMITS[type]).cover?(value)
    [value].pack(PACK[type])
  when "bool" then [value ? 1 : 0].pack("C")
  when "id16" then value.b
  when "sqlstate"
    fail!("sqlstate #{value.inspect}") unless value.match?(/\A[0-9A-Z]{5}\z/)
    value.b
  when "string" then [value.bytesize].pack("N") + value.b
  when "page"
    fail!("page is not page_size bytes") unless value.bytesize == page_size && page_size.positive?
    value.b
  when "u32_list" then [value.size].pack("N") + value.map { |v| encode_value("u32", v, 0) }.join
  when "page_list" then [value.size].pack("N") + value.map { |p| encode_value("page", p, page_size) }.join
  when "indexed_page_list"
    [value.size].pack("N") + value.map { |i, p| encode_value("u32", i, 0) + encode_value("page", p, page_size) }.join
  end
end

def encode_frame(message, request_id, values, page_size)
  body = message.fetch("fields").map { |name, type| encode_value(type, values.fetch(name), page_size) }.join
  [5 + body.bytesize, message.fetch("code"), request_id].pack("NCN") + body
end

# --- decoder --------------------------------------------------------------------------------------

class Reader
  def initialize(bytes)
    @bytes = bytes.b
    @pos = 0
  end

  def take(n)
    raise Malformed.new("truncated", "need #{n} bytes at #{@pos}") if n > remaining

    out = @bytes.byteslice(@pos, n)
    @pos += n
    out
  end

  def remaining = @bytes.bytesize - @pos
end

def decode_value(type, reader, page_size)
  case type
  when "u8", "u16", "u32", "u64"
    width = { "u8" => 1, "u16" => 2, "u32" => 4, "u64" => 8 }.fetch(type)
    reader.take(width).unpack1(PACK[type])
  when "bool"
    byte = reader.take(1).unpack1("C")
    raise Malformed.new("bool", "byte #{byte}") unless byte <= 1

    byte == 1
  when "id16" then reader.take(16)
  when "sqlstate"
    code = reader.take(5)
    raise Malformed.new("sqlstate", code.inspect) unless code.match?(/\A[0-9A-Z]{5}\z/)

    code
  when "string"
    length = decode_value("u32", reader, page_size)
    raise Malformed.new("count", "string length #{length}") if length > reader.remaining

    text = reader.take(length).force_encoding(Encoding::UTF_8)
    raise Malformed.new("utf8", text.b.inspect) unless text.valid_encoding?

    text
  when "page"
    raise Malformed.new("page", "no connection page size") unless page_size.positive?

    reader.take(page_size)
  when "u32_list", "page_list", "indexed_page_list"
    count = decode_value("u32", reader, page_size)
    item = { "u32_list" => 4, "page_list" => page_size, "indexed_page_list" => 4 + page_size }.fetch(type)
    raise Malformed.new("count", "#{count} items of #{item} bytes") if count * item > reader.remaining

    Array.new(count) do
      case type
      when "u32_list" then decode_value("u32", reader, page_size)
      when "page_list" then decode_value("page", reader, page_size)
      else [decode_value("u32", reader, page_size), decode_value("page", reader, page_size)]
      end
    end
  end
end

# Decodes one complete frame; returns [message, request_id, values].
def decode_frame(bytes, page_size)
  raise Malformed.new("truncated", "frame shorter than its length field") if bytes.bytesize < 4

  length = bytes.unpack1("N")
  raise Malformed.new("length", "length #{length}") if length < 5 || length > MAX_FRAME

  reader = Reader.new(bytes.byteslice(4..))
  raise Malformed.new("trailing", "frame bytes beyond length") if reader.remaining > length

  frame = Reader.new(reader.take(length))
  code, request_id = frame.take(5).unpack("CN")
  message = BY_CODE[code]
  raise Malformed.new("code", format("0x%02x", code)) if message.nil? || message["reply"] == "reserved"

  values = message.fetch("fields").to_h { |name, type| [name, decode_value(type, frame, page_size)] }
  raise Malformed.new("trailing", "#{frame.remaining} bytes after the last field") unless frame.remaining.zero?

  [message, request_id, values]
end

# --- owner record ---------------------------------------------------------------------------------

OWNER_FIELDS = [%w[flags u8], %w[uid u32], %w[incarnation id16], %w[pid u32], %w[endpoint string]].freeze

def encode_owner(values)
  body = CATALOG.fetch("owner_record_magic").b + [CATALOG.fetch("owner_record_version")].pack("n") +
         OWNER_FIELDS.map { |name, type| encode_value(type, values.fetch(name), 0) }.join
  body + [Zlib.crc32(body)].pack("N")
end

def decode_owner(bytes)
  reader = Reader.new(bytes)
  raise Malformed.new("magic", "owner record magic") unless reader.take(8) == CATALOG.fetch("owner_record_magic")
  raise Malformed.new("version", "owner record version") unless reader.take(2).unpack1("n") == 1

  values = OWNER_FIELDS.to_h { |name, type| [name, decode_value(type, reader, 0)] }
  raise Malformed.new("flags", "owner record reserved flag bits") unless values["flags"] <= 1

  crc = reader.take(4).unpack1("N")
  raise Malformed.new("trailing", "owner record") unless reader.remaining.zero?
  raise Malformed.new("crc", "owner record") unless crc == Zlib.crc32(bytes.byteslice(0, bytes.bytesize - 4))

  values
end

# --- fixtures -------------------------------------------------------------------------------------

def fixture_values(row, fields, page_size)
  fields.to_h { |name, type| [name, parse_value(type, row.fetch("f_#{name}"), page_size)] }
end

def check_field_keys(row, fields)
  extra = row.keys.select { |k| k.start_with?("f_") } - fields.map { |name, _| "f_#{name}" }
  fail!("#{row['name']}: fields not in the message: #{extra.join(', ')}") unless extra.empty?
end

def verify_or_generate(generate)
  check_catalog
  hexes = {}
  FIXTURES.fetch("frame").each do |row|
    message = BY_NAME.fetch(row.fetch("message")) { fail!("#{row['name']}: unknown message") }
    page_size = Integer(row.fetch("page_size"), 10)
    check_field_keys(row, message["fields"])
    values = fixture_values(row, message["fields"], page_size)
    bytes = encode_frame(message, Integer(row.fetch("request_id"), 10), values, page_size)
    hexes[[:frame, row.fetch("name")]] = bytes.unpack1("H*")
    next if generate

    fail!("#{row['name']}: encoded #{bytes.unpack1('H*')}, fixture #{row['hex']}") unless row.fetch("hex") == bytes.unpack1("H*")
    decoded, request_id, decoded_values = decode_frame(bytes, page_size)
    unless decoded.equal?(message) && request_id == Integer(row["request_id"], 10) && decoded_values == values
      fail!("#{row['name']}: decode does not round-trip")
    end
  end
  FIXTURES.fetch("owner_record").each do |row|
    check_field_keys(row, OWNER_FIELDS)
    values = fixture_values(row, OWNER_FIELDS, 0)
    bytes = encode_owner(values)
    hexes[[:owner_record, row.fetch("name")]] = bytes.unpack1("H*")
    next if generate

    fail!("owner record #{row['name']}: encoded #{bytes.unpack1('H*')}") unless row.fetch("hex") == bytes.unpack1("H*")
    fail!("owner record #{row['name']}: decode does not round-trip") unless decode_owner(bytes) == values
  end
  FIXTURES.fetch("malformed").each do |row|
    bytes = [row.fetch("hex")].pack("H*")
    begin
      decode_frame(bytes, Integer(row.fetch("page_size"), 10))
      fail!("malformed #{row['name']}: decoded without error")
    rescue Malformed => e
      fail!("malformed #{row['name']}: rejected as #{e.kind}, expected #{row['kind']}") unless e.kind == row.fetch("kind")
    end
  end
  return unless generate

  rewrite(hexes)
end

# Rewrites each frame/owner_record `hex = "..."` line in place, keeping comments and layout.
def rewrite(hexes)
  section = nil
  name = nil
  lines = File.readlines(FIXTURES_PATH).map do |line|
    case line
    when /\A\[\[(\w+)\]\]/ then section = Regexp.last_match(1).to_sym
    when /\Aname = "([^"]*)"/ then name = Regexp.last_match(1)
    when /\Ahex = "/
      hex = hexes[[section, name]]
      line = %(hex = "#{hex}"\n) if hex
    end
    line
  end
  File.write(FIXTURES_PATH, lines.join)
  puts "rewrote #{hexes.size} fixture hex values"
end

verify_or_generate(ARGV.include?("--generate"))
puts "page-service catalog and fixtures OK (#{MESSAGES.size} messages, " \
     "#{FIXTURES['frame'].size} frames, #{FIXTURES['owner_record'].size} owner records, " \
     "#{FIXTURES['malformed'].size} malformed)" unless ARGV.include?("--generate")
