#!/usr/bin/env ruby
# frozen_string_literal: true

# Verify spec/encoding/integers.toml against the encoding rules in
# spec/design/encoding.md. Independent reference encoder — recomputes the bytes
# from scratch and checks the three invariants (round-trip, byte-exactness, order)
# rather than trusting the file. Test-time only (CLAUDE.md §5).
#
#   bundle exec ruby spec/encoding/verify.rb   (or: mise run verify)
#
# Exit 0 = all vectors conform; nonzero = mismatch (prints the offending case).

require "bundler/setup"
require "toml-rb"
require "json"

WIDTH = { "i16" => 2, "i32" => 4, "i64" => 8, "boolean" => 1, "uuid" => 16 }.freeze

# uuid-raw16: the 16 raw bytes of the canonical 8-4-4-4-12 form (no sign-flip, no
# escape/terminator — encoding.md §2.7). encode = strip hyphens, pack the 32 hex digits.
def uuid_to_bytes(s)
  [s.delete("-")].pack("H*")
end

def uuid_from_bytes(bytes)
  h = bytes.unpack1("H*")
  "#{h[0, 8]}-#{h[8, 4]}-#{h[12, 4]}-#{h[16, 4]}-#{h[20, 12]}"
end

# bare key encoding. For integers: int-be-signflip (add bias 2^(bits-1), unsigned BE).
# For uuid (a String value): the raw 16 bytes verbatim, no sign-flip. For boolean (a true/false
# value): the single bool-byte 0x00 false / 0x01 true (§2.9), no sign-flip.
def enc_bare(value, width)
  return uuid_to_bytes(value) if value.is_a?(String)
  return [value ? 1 : 0].pack("C") if value == true || value == false

  bias = 1 << (width * 8 - 1)
  u = value + bias
  raise "value #{value} out of range for width #{width}" unless u.between?(0, (1 << (width * 8)) - 1)

  # to fixed-width big-endian bytes
  Array.new(width) { |i| (u >> (8 * (width - 1 - i))) & 0xFF }.pack("C*")
end

def dec_bare(bytes, width)
  return uuid_from_bytes(bytes) if width == 16 # uuid is the only 16-byte key
  return bytes.bytes.first == 1 if width == 1 # boolean is the only 1-byte key

  bytes.bytes.reduce(0) { |acc, b| (acc << 8) | b } - (1 << (width * 8 - 1))
end

# nullable slot: 0x00 = present + value bytes, 0x01 = NULL. Present (0x00) sorts
# before NULL (0x01), so NULLs sort last ascending (the PostgreSQL model).
def enc_nullable(c, width)
  return [0x01].pack("C") if c["null"]

  [0x00].pack("C") + enc_bare(c["value"], width)
end

def invert(bytes)
  bytes.bytes.map { |b| b ^ 0xFF }.pack("C*")
end

# text-terminated-escape / bytea-terminated-escape (encoding.md §2.4/§2.6): escape every 0x00 to
# 0x00 0xFF, terminate with 0x00 0x01. `content` is the value's raw bytes (UTF-8 for text, raw for
# bytea). Variable-width and self-delimiting; the bare PRIMARY-KEY body (no presence tag).
def enc_terminated(content)
  out = +"".b
  content.each_byte do |b|
    out << b
    out << 0xFF if b.zero?
  end
  out << 0x00 << 0x01
  out
end

# The raw content bytes of a terminated-escape case: a text `value` (UTF-8 string) or a bytea
# `hex` string. NULL cases carry neither (handled by the slot encoder).
def terminated_content(c)
  return c["value"].b if c.key?("value")

  [c["hex"]].pack("H*").b
end

def enc_terminated_nullable(c)
  return [0x01].pack("C") if c["null"]

  [0x00].pack("C") + enc_terminated(terminated_content(c))
end

def terminated_label(c)
  return "NULL" if c["null"]

  c.key?("value") ? c["value"].inspect : "\\x#{c['hex']}"
end

# Parse a decimal string "[-]int[.frac]" into (neg, significant-digits, scale) — the stored
# (sign, coefficient-digits, scale) the cores carry. Significant digits strip LEADING zeros
# (so "0.05" → digits "5", scale 2, precision 1); an all-zero coefficient gives "" (== zero).
def parse_decimal(s)
  neg = s.start_with?("-")
  body = neg ? s[1..] : s
  int_part, frac_part = body.split(".", 2)
  frac_part ||= ""
  digits = (int_part + frac_part).sub(/\A0+/, "")
  [neg, digits, frac_part.length]
end

# decimal-order-preserving (encoding.md §2.5): normalize to (sign, base-100 mantissa pairs, E)
# with value = 0.<pairs> × 100^E, then emit class byte (03 neg / 04 zero / 05 pos), the 4-byte
# int-be-signflip exponent E, the mantissa pairs (pair+1 ∈ [01,64]), and a 00 terminator; negatives
# complement E+mantissa+terminator. Independent of display scale — 1.5 and 1.50 coincide.
def enc_decimal(s)
  neg, digits, scale = parse_decimal(s)
  return [0x04].pack("C") if digits.empty? # zero is the single class byte

  decpt = digits.length - scale            # value = 0.<digits> × 10^decpt
  digits = digits.sub(/0+\z/, "")          # drop trailing zero digits (decpt unchanged)
  e = (decpt + 1) / 2                       # Ruby integer / floors = ⌊(decpt+1)/2⌋
  grouped = decpt.odd? ? "0#{digits}" : digits.dup
  grouped << "0" if grouped.length.odd?     # pad right to an even number of base-10 digits
  body = +"".b
  body << enc_bare(e, 4)                     # 4-byte order-preserving exponent
  grouped.chars.each_slice(2) { |a, b| body << ((a.to_i * 10 + b.to_i) + 1) }
  body << 0x00
  neg ? [0x03].pack("C") + invert(body) : [0x05].pack("C") + body
end

def enc_decimal_nullable(c)
  return [0x01].pack("C") if c["null"]

  [0x00].pack("C") + enc_decimal(c["value"])
end

def decimal_label(c) = c["null"] ? "NULL" : c["value"]

# The canonical 128-bit microsecond span of an interval (spec/design/interval.md §2): 1 month =
# 30 days, 1 day = 24 h. The comparison/dedup key the encoding sorts by.
def interval_span(months, days, micros) = (months * 30 + days) * 86_400_000_000 + micros

# interval-span-i128 (encoding.md §2.10): the 16-byte order-preserving encoding of the canonical
# 128-bit span — int-be-signflip at i128 width (bias 2^127, big-endian). enc_bare already does the
# fixed-width int-be-signflip; width 16 = i128 (the bias keeps the value in [0, 2^128)). Span-equal
# intervals ('1 mon' / '30 days') coincide — the "equal but not identical" wrinkle.
def enc_interval(c)
  enc_bare(interval_span(c["months"], c["days"], c["micros"]), 16)
end

def enc_interval_nullable(c)
  return [0x01].pack("C") if c["null"]

  [0x00].pack("C") + enc_interval(c)
end

def interval_label(c)
  return "NULL" if c["null"]

  c["label"] || "{#{c['months']} #{c['days']} #{c['micros']}}"
end

# One range bound's element key given the element type name (the six range subtypes — ranges.md §2):
# int-be-signflip for the integers (i32 = 4 bytes, i64 = 8) and `date` (the i32 day codec, 4 bytes) and
# the timestamps (the i64 instant codec, 8 bytes); decimal-order-preserving for `decimal` (§2.5).
def enc_range_elem(elem, value)
  case elem
  when "i32", "date" then enc_bare(value, 4)
  when "i64", "timestamp", "timestamptz" then enc_bare(value, 8)
  when "decimal" then enc_decimal(value)
  else raise "unknown range element type #{elem}"
  end
end

# Append one bound of a non-empty range (encoding.md §2.11): an infinite bound is a single marker
# (−∞ = 0x00 on the lower side, +∞ = 0x02 on the upper); a finite bound is 0x01 ‖ the element key ‖ an
# inclusivity byte (0x00 when inclusive == is_lower, else 0x01 — PG range_cmp_bounds).
def enc_range_bound(out, elem, c, side, is_lower)
  unless c.key?(side)
    out << (is_lower ? 0x00 : 0x02)
    return
  end
  out << 0x01
  out << enc_range_elem(elem, c[side])
  inc = c["#{side}_inc"]
  out << ((inc == is_lower) ? 0x00 : 0x01)
end

# range-bounds (encoding.md §2.11): the first container key. empty = 0x00 (the whole key); non-empty =
# 0x01 ‖ lower bound ‖ upper bound, each framed by enc_range_bound. memcmp over the bytes reproduces
# range_total_cmp (ranges.md §6).
def enc_range(elem, c)
  return [0x00].pack("C") if c["empty"]

  out = +"".b
  out << 0x01
  enc_range_bound(out, elem, c, "lower", true)
  enc_range_bound(out, elem, c, "upper", false)
  out
end

def enc_range_nullable(elem, c)
  return [0x01].pack("C") if c["null"]

  [0x00].pack("C") + enc_range(elem, c)
end

def range_label(c)
  return "NULL" if c["null"]

  c["label"] || "{range}"
end

# Parse a float fixture's string `value` the way a core's float input does: the special words
# Infinity/-Infinity/NaN (case as written), else a decimal / e-notation literal.
def parse_float_str(s)
  case s
  when "Infinity", "+Infinity", "inf" then Float::INFINITY
  when "-Infinity", "-inf" then -Float::INFINITY
  when "NaN" then Float::NAN
  else Float(s)
  end
end

# float-order-preserving (encoding.md §2.8): canonicalize (-0 → +0, every NaN → one quiet pattern),
# take the IEEE bits as a big-endian unsigned integer, then if the sign bit is set flip ALL bits
# else flip just the sign bit. Fixed-width: f64 = 8 bytes, f32 = 4. `pack("g")` rounds the f64
# fixture value to binary32 before reading its bits (the fixtures use f32-representable values).
def enc_float(type, s)
  f = parse_float_str(s)
  if type == "f64"
    bits = f.nan? ? 0x7FF8000000000000 : (f == 0.0 ? 0 : [f].pack("G").unpack1("Q>"))
    bits ^= (bits >> 63) == 1 ? 0xFFFFFFFFFFFFFFFF : 0x8000000000000000
    [bits].pack("Q>")
  else # f32
    bits = f.nan? ? 0x7FC00000 : (f == 0.0 ? 0 : [f].pack("g").unpack1("N"))
    bits ^= (bits >> 31) == 1 ? 0xFFFFFFFF : 0x80000000
    [bits].pack("N")
  end
end

def enc_float_nullable(type, c)
  return [0x01].pack("C") if c["null"]

  [0x00].pack("C") + enc_float(type, c["value"])
end

def float_label(c) = c["null"] ? "NULL" : c["value"]

# Verify spec/encoding/float.toml: the float-order-preserving KEY encoding (§2.8) — the same three
# invariants as the decimal/interval paths (byte-exact + strict order, minus round-trip: a key is
# never decoded back to a value). The bare body is the fixed-width transformed bits; nullable
# prepends the §2.2 tag; descending inverts the whole component (and the file lists descending cases
# already in ascending-byte order, so the order walk holds).
def check_float_file(filename)
  data = TomlRB.load_file(File.join(__dir__, filename))
  checked = 0
  [["bare", ->(type, c) { enc_float(type, c["value"]) }],
   ["nullable", ->(type, c) { enc_float_nullable(type, c) }],
   ["descending", ->(type, c) { invert(enc_float_nullable(type, c)) }]].each do |kind, enc|
    (data[kind] || []).each do |group|
      type = group["type"]
      rows = []
      group["cases"].each do |c|
        want = c["bytes"]
        got = hex(enc.call(type, c))
        fail!("#{kind} #{type} #{float_label(c)}: encode=#{got} want=#{want}") unless got == want
        rows << [float_label(c), [want].pack("H*").b]
        checked += 1
      end
      check_order("#{kind} #{type}", rows)
    end
  end
  checked
end

# Verify spec/encoding/range.toml: the range-bounds KEY encoding (§2.11) — the same three invariants
# as the decimal/interval paths (byte-exact + strict order, minus round-trip: a key is never decoded
# back to a value). Each group names its element type (`elem`); the bare body is the container key,
# nullable prepends the §2.2 tag, descending inverts the whole component.
def check_range_file(filename)
  data = TomlRB.load_file(File.join(__dir__, filename))
  checked = 0
  [["bare", ->(elem, c) { enc_range(elem, c) }],
   ["nullable", ->(elem, c) { enc_range_nullable(elem, c) }],
   ["descending", ->(elem, c) { invert(enc_range_nullable(elem, c)) }]].each do |kind, enc|
    (data[kind] || []).each do |group|
      elem = group["elem"]
      rows = []
      group["cases"].each do |c|
        want = c["bytes"]
        got = hex(enc.call(elem, c))
        fail!("#{kind} #{group['type']} #{range_label(c)}: encode=#{got} want=#{want}") unless got == want
        rows << [range_label(c), [want].pack("H*").b]
        checked += 1
      end
      check_order("#{kind} #{group['type']}", rows)
    end
  end
  checked
end

# One composite-key FIELD slot (encoding.md §2.15): `0x01` for a NULL field, else `0x00` present ‖
# the field's own order-preserving key. `v` is an inline table naming the field's encoding — a scalar
# (`text`/`bytea`/`i16`/`i32`/`i64`), a NESTED composite (`{ composite = [...] }`, recurses), or
# `{ null = true }`. A composite field carries no COLLATE, so text keys use the `C` byte order.
def enc_composite_field(v)
  return [0x01].pack("C") if v["null"]

  body =
    if v.key?("text") then enc_terminated(v["text"].b)
    elsif v.key?("bytea") then enc_terminated([v["bytea"]].pack("H*"))
    elsif v.key?("i16") then enc_bare(v["i16"], 2)
    elsif v.key?("i32") then enc_bare(v["i32"], 4)
    elsif v.key?("i64") then enc_bare(v["i64"], 8)
    elsif v.key?("composite") then enc_composite(v["composite"])
    else raise "unknown composite field value #{v.inspect}"
    end
  [0x00].pack("C") + body
end

# The bare `composite-field-slots` key (encoding.md §2.15): the concatenation of each field's §2.2
# slot, in declaration order. Fixed arity ⇒ no terminator. `fields` is the per-field value list.
def enc_composite(fields) = fields.map { |f| enc_composite_field(f) }.join.b

# The composite key in a NULLABLE slot (a secondary-index column / a whole-value-NULL member):
# `0x01` for a whole-value NULL, else `0x00` present ‖ the bare composite key.
def enc_composite_nullable(c)
  return [0x01].pack("C") if c["null"]

  [0x00].pack("C") + enc_composite(c["fields"])
end

# Verify spec/encoding/composite.toml (encoding.md §2.15): the recursive composite container key.
# The same invariants as the range/array container paths (byte-exact + strict order, minus round-trip
# — a key is never decoded back to a value). `bare` is the PK body, `nullable` prepends the §2.2 tag,
# `descending` inverts the whole component.
def check_composite_file(filename)
  data = TomlRB.load_file(File.join(__dir__, filename))
  checked = 0
  [["bare", ->(c) { enc_composite(c["fields"]) }],
   ["nullable", ->(c) { enc_composite_nullable(c) }],
   ["descending", ->(c) { invert(enc_composite_nullable(c)) }]].each do |kind, enc|
    (data[kind] || []).each do |group|
      rows = []
      group["cases"].each do |c|
        want = c["bytes"]
        got = hex(enc.call(c))
        fail!("#{kind} #{group['type']} #{c['label']}: encode=#{got} want=#{want}") unless got == want
        rows << [c["label"], [want].pack("H*").b]
        checked += 1
      end
      check_order("#{kind} #{group['type']}", rows)
    end
  end
  checked
end

# Verify spec/encoding/decimal.toml: the same three invariants as the terminated-escape path
# (byte-exact + strict order, minus round-trip — a key is never decoded back to a value).
def check_decimal_file(filename)
  data = TomlRB.load_file(File.join(__dir__, filename))
  checked = 0
  [["bare", ->(c) { enc_decimal(c["value"]) }],
   ["nullable", ->(c) { enc_decimal_nullable(c) }],
   ["descending", ->(c) { invert(enc_decimal_nullable(c)) }]].each do |kind, enc|
    (data[kind] || []).each do |group|
      rows = []
      group["cases"].each do |c|
        want = c["bytes"]
        got = hex(enc.call(c))
        fail!("#{kind} #{group['type']} #{decimal_label(c)}: encode=#{got} want=#{want}") unless got == want
        rows << [decimal_label(c), [want].pack("H*").b]
        checked += 1
      end
      check_order("#{kind} #{group['type']}", rows)
    end
  end
  checked
end

# Verify spec/encoding/interval.toml: the interval-span-i128 KEY encoding (§2.10) — the same three
# invariants as the decimal path (byte-exact + strict order, minus round-trip: a key is never
# decoded back to a value). The bare body is the 16-byte span; nullable prepends the §2.2 tag;
# descending inverts the whole component.
def check_interval_file(filename)
  data = TomlRB.load_file(File.join(__dir__, filename))
  checked = 0
  [["bare", ->(c) { enc_interval(c) }],
   ["nullable", ->(c) { enc_interval_nullable(c) }],
   ["descending", ->(c) { invert(enc_interval_nullable(c)) }]].each do |kind, enc|
    (data[kind] || []).each do |group|
      rows = []
      group["cases"].each do |c|
        want = c["bytes"]
        got = hex(enc.call(c))
        fail!("#{kind} #{group['type']} #{interval_label(c)}: encode=#{got} want=#{want}") unless got == want
        rows << [interval_label(c), [want].pack("H*").b]
        checked += 1
      end
      check_order("#{kind} #{group['type']}", rows)
    end
  end
  checked
end

# Verify one variable-width terminated-escape fixture file (text.toml / bytea.toml): the same
# three invariants as the fixed-width path, minus round-trip (a key is never decoded back to a
# value). Returns the number of vectors checked.
def check_terminated_file(filename)
  data = TomlRB.load_file(File.join(__dir__, filename))
  checked = 0

  (data["bare"] || []).each do |group|
    rows = []
    group["cases"].each do |c|
      want = c["bytes"]
      got = hex(enc_terminated(terminated_content(c)))
      fail!("bare #{group['type']} #{terminated_label(c)}: encode=#{got} want=#{want}") unless got == want
      rows << [terminated_label(c), [want].pack("H*").b]
      checked += 1
    end
    check_order("bare #{group['type']}", rows)
  end

  (data["nullable"] || []).each do |group|
    rows = []
    group["cases"].each do |c|
      want = c["bytes"]
      got = hex(enc_terminated_nullable(c))
      fail!("nullable #{group['type']} #{terminated_label(c)}: encode=#{got} want=#{want}") unless got == want
      rows << [terminated_label(c), [want].pack("H*").b]
      checked += 1
    end
    check_order("nullable #{group['type']}", rows)
  end

  (data["descending"] || []).each do |group|
    rows = []
    group["cases"].each do |c|
      want = c["bytes"]
      got = hex(invert(enc_terminated_nullable(c)))
      fail!("descending #{group['type']} #{terminated_label(c)}: encode=#{got} want=#{want}") unless got == want
      rows << [terminated_label(c), [want].pack("H*").b]
      checked += 1
    end
    check_order("descending #{group['type']}", rows)
  end

  checked
end

def hex(bytes) = bytes.unpack1("H*")

def fail!(msg)
  warn "FAIL: #{msg}"
  exit 1
end

# rows: array of [human_label, bytes] in listed order; must be strictly increasing.
def check_order(label, rows)
  rows.each_cons(2) do |(ph, pb), (h, b)|
    next if pb < b

    fail!("#{label}: order not strictly increasing at #{ph.inspect} -> #{h.inspect} " \
          "(#{hex(pb)} !< #{hex(b)})")
  end
end

# --- GIN jsonb_ops terms (spec/design/gin.md §2.1/§4.1) -----------------------------------------
# term = flag ‖ body: 0x01 key / 0x02 null / 0x03 boolean / 0x04 number / 0x05 string value, with
# 0x10 set when a key/number/string body source exceeded 128 bytes and was cut to its first 128. A
# key/string body is text-terminated-escape over the (cut) UTF-8; a number body the same over the
# (cut) decimal-order-preserving key; boolean 0x00/0x01; null empty.
GIN_JSONB_MAX = 128

def gin_jsonb_framed(flag, content)
  content = content.b
  if content.bytesize > GIN_JSONB_MAX
    flag |= 0x10
    content = content.byteslice(0, GIN_JSONB_MAX)
  end
  [flag].pack("C") + enc_terminated(content)
end

def gin_jsonb_key_term(key) = gin_jsonb_framed(0x01, key)

# A scalar's term; `as_key` marks a string that is an array element (or the top-level scalar).
def gin_jsonb_scalar_term(v, as_key)
  case v
  when nil then "\x02".b
  when true then "\x03\x01".b
  when false then "\x03\x00".b
  when String then gin_jsonb_framed(as_key ? 0x01 : 0x05, v)
  when GinJsonbNumber then gin_jsonb_framed(0x04, enc_decimal(v.plain))
  else raise "bad jsonb scalar #{v.inspect}"
  end
end

def gin_jsonb_walk(node, out)
  case node
  when Hash
    node.each do |k, v|
      out << gin_jsonb_key_term(k)
      v.is_a?(Hash) || v.is_a?(Array) ? gin_jsonb_walk(v, out) : out << gin_jsonb_scalar_term(v, false)
    end
  when Array
    node.each do |e|
      e.is_a?(Hash) || e.is_a?(Array) ? gin_jsonb_walk(e, out) : out << gin_jsonb_scalar_term(e, e.is_a?(String))
    end
  end
end

# A JSON number kept EXACT (jsonb numbers are decimals): `plain` is its "[-]int[.frac]" spelling.
GinJsonbNumber = Struct.new(:plain)

# A minimal exact JSON reader for the vectors (Ruby's JSON would round numbers through Float): objects
# keep their LAST duplicate key (jsonb's rule), strings decode JSON escapes via JSON.parse.
class GinJsonbParser
  def initialize(text)
    @s = text
    @i = 0
  end

  def parse
    v = value
    ws
    raise "trailing JSON text" unless @i == @s.length

    v
  end

  private

  def ws = (@i += 1 while @i < @s.length && " \t\n\r".include?(@s[@i]))

  def value
    ws
    case @s[@i]
    when "{" then object
    when "[" then array
    when '"' then string
    when "t" then lit("true", true)
    when "f" then lit("false", false)
    when "n" then lit("null", nil)
    else number
    end
  end

  def lit(word, v)
    raise "bad JSON literal" unless @s[@i, word.length] == word

    @i += word.length
    v
  end

  def object
    @i += 1
    h = {}
    ws
    if @s[@i] == "}"
      @i += 1
      return h
    end
    loop do
      ws
      k = string
      ws
      raise "expected :" unless @s[@i] == ":"

      @i += 1
      h.delete(k)
      h[k] = value
      ws
      c = @s[@i]
      @i += 1
      return h if c == "}"
      raise "expected , or }" unless c == ","
    end
  end

  def array
    @i += 1
    a = []
    ws
    if @s[@i] == "]"
      @i += 1
      return a
    end
    loop do
      a << value
      ws
      c = @s[@i]
      @i += 1
      return a if c == "]"
      raise "expected , or ]" unless c == ","
    end
  end

  def string
    m = /\G"(?:[^"\\]|\\.)*"/.match(@s, @i) or raise "bad JSON string"
    @i += m[0].length
    JSON.parse(m[0])
  end

  # Normalize "[-]int[.frac][e[+-]exp]" to a plain "[-]int[.frac]" decimal string, exactly.
  def number
    m = /\G(-?)(\d+)(?:\.(\d+))?(?:[eE]([+-]?\d+))?/.match(@s, @i) or raise "bad JSON number"
    @i += m[0].length
    sign, int, frac, exp = m[1], m[2], m[3] || "", m[4].to_i
    digits = int + frac
    point = int.length + exp # position of the decimal point within `digits`
    digits = ("0" * -point) + digits if point.negative?
    point = [point, 0].max
    digits += "0" * (point - digits.length) if point > digits.length
    whole = digits[0, point]
    whole = "0" if whole.empty?
    fr = digits[point..]
    GinJsonbNumber.new(fr.empty? ? "#{sign}#{whole}" : "#{sign}#{whole}.#{fr}")
  end
end

# The index term SET of a document (JSON text), deduplicated, in ascending byte order.
def gin_jsonb_index_terms(doc_text)
  node = GinJsonbParser.new(doc_text).parse
  out = []
  if node.is_a?(Hash) || node.is_a?(Array)
    gin_jsonb_walk(node, out)
  else
    out << gin_jsonb_scalar_term(node, node.is_a?(String)) # a raw scalar is a one-element array
  end
  out.uniq.sort
end

# Verify spec/encoding/gin_jsonb_terms.toml: each document's term set and each query key's term are
# byte-exact against this independent reference.
def check_gin_jsonb_file(filename)
  data = TomlRB.load_file(File.join(__dir__, filename))
  checked = 0
  (data["index"] || []).each do |c|
    got = gin_jsonb_index_terms(c["doc"]).map { |t| hex(t) }
    fail!("gin jsonb index #{c['doc'][0, 60].inspect}: terms=#{got} want=#{c['terms']}") unless got == c["terms"]
    checked += 1
  end
  (data["key"] || []).each do |c|
    got = hex(gin_jsonb_key_term(c["key"]))
    fail!("gin jsonb key #{c['key'][0, 60].inspect}: term=#{got} want=#{c['term']}") unless got == c["term"]
    checked += 1
  end
  checked
end

# --- ordered index slots (encoding.md §2.16) ----------------------------------------------------
# One value's bare order-preserving key body, from a tagged inline table: an integer width
# (`i16`/`i32`/`i64`, `timestamp` = the i64 rule, `date` = the i32 rule), `boolean`, `uuid`, `f32`/`f64`
# (string), `decimal` (string), `text`, `bytea` (hex), `interval` ([months, days, micros]), `range`
# ({ elem, lower?, upper?, lower_inc?, upper_inc?, empty? }), `array` ({ elem, elements = [...] } —
# 1-D, lower bound 1; a NULL element is { null = true }), `composite` (the field list, §2.15 rules),
# or `collated` (a given UCA sort-key body in hex, pinned in spec/collation/vectors/sortkey.toml).
ORDERED_INT_WIDTH = { "i16" => 2, "i32" => 4, "i64" => 8, "timestamp" => 8, "date" => 4 }.freeze

def enc_ordered_body(v)
  k, x = v.first
  case k
  when *ORDERED_INT_WIDTH.keys then enc_bare(x, ORDERED_INT_WIDTH[k])
  when "boolean" then enc_bare(x, 1)
  when "uuid" then uuid_to_bytes(x)
  when "f32", "f64" then enc_float(k, x)
  when "decimal" then enc_decimal(x)
  when "text" then enc_terminated(x.b)
  when "bytea" then enc_terminated([x].pack("H*"))
  when "interval" then enc_interval({ "months" => x[0], "days" => x[1], "micros" => x[2] })
  when "range" then enc_range(x["elem"], x)
  when "array" then enc_ordered_array(x)
  when "composite" then enc_composite(x)
  when "collated" then [x].pack("H*")
  else raise "unknown ordered value #{v.inspect}"
  end
end

# array-elements-terminated (§2.14) for a 1-D array with lower bound 1 (the empty array has ndim 0).
def enc_ordered_array(x)
  out = +"".b
  x["elements"].each do |e|
    out << (e.is_a?(Hash) && e["null"] ? "\x02".b : "\x01".b + enc_ordered_body({ x["elem"] => e }))
  end
  out << "\x00".b
  out << (x["elements"].empty? ? "\x00".b : "\x01".b + [x["elements"].size].pack("N") + enc_bare(1, 4))
end

# The four ordered slots: [present tag, NULL tag, invert body?] per order (encoding.md §2.16).
ORDERED_SLOTS = {
  "asc_nulls_last" => [0x00, 0x01, false], "asc_nulls_first" => [0x01, 0x00, false],
  "desc_nulls_first" => [0xFF, 0xFE, true], "desc_nulls_last" => [0xFE, 0xFF, true]
}.freeze

def enc_ordered_slot(order, c)
  present, null, inv = ORDERED_SLOTS.fetch(order)
  return [null].pack("C") if c["null"]

  body = enc_ordered_body(c["value"])
  [present].pack("C") + (inv ? invert(body) : body)
end

# Verify spec/encoding/ordered.toml: every case's bytes in every order are byte-exact against this
# reference; within a group the cases are listed in ASCENDING logical order (the NULL case last), and
# each order's encodings must sort strictly in that order's logical sequence (ASC / DESC values, NULLs
# placed first or last) — and be PREFIX-FREE, so a slot composes with any following bytes.
def check_ordered_file(filename)
  data = TomlRB.load_file(File.join(__dir__, filename))
  checked = 0
  (data["group"] || []).each do |group|
    values = group["cases"].reject { |c| c["null"] }
    nulls = group["cases"].select { |c| c["null"] }
    fail!("ordered #{group['type']}: exactly one trailing NULL case") unless nulls.size == 1 && group["cases"].last["null"]
    ORDERED_SLOTS.each do |order, (_, _, desc)|
      seq = desc ? values.reverse : values
      seq = order.end_with?("nulls_first") ? nulls + seq : seq + nulls
      rows = seq.map do |c|
        got = hex(enc_ordered_slot(order, c))
        want = c[order]
        fail!("ordered #{group['type']} #{c['label']} #{order}: encode=#{got} want=#{want}") unless got == want
        checked += 1
        [c["label"] || "NULL", [want].pack("H*").b]
      end
      check_order("ordered #{group['type']} #{order}", rows)
      rows.combination(2) do |(la, a), (lb, b)|
        fail!("ordered #{group['type']} #{order}: #{la} / #{lb} not prefix-free") if a.start_with?(b) || b.start_with?(a)
      end
    end
  end
  checked
end


# --- GIN posting runs (gin.md §4.5, format_version 47) -------------------------------------------
# An independent reference of the run codec and of run maintenance over one term's posting list.
# A term's runs are kept as an ascending list of key lists (ascending by last key = run-key order).
GIN_RUN_MAX_BYTES = 64

def gin_run_varint(v)
  out = "".b
  loop do
    byte = v & 0x7F
    v >>= 7
    if v.zero?
      out << byte.chr
      return out
    end
    out << (byte | 0x80).chr
  end
end

def gin_run_shared(a, b)
  n = 0
  n += 1 while n < a.bytesize && n < b.bytesize && a.getbyte(n) == b.getbyte(n)
  n
end

def gin_run_encode(term, keys)
  last = keys.last
  body = "".b
  prev = last
  keys[0...-1].each do |k|
    shared = gin_run_shared(prev, k)
    body << gin_run_varint(shared) << gin_run_varint(k.bytesize - shared) << k.byteslice(shared..)
    prev = k
  end
  (term + last + body + [body.bytesize].pack("n")).b
end

def gin_run_decode(term_len, run)
  body_len = run.byteslice(-2, 2).unpack1("n")
  last = run.byteslice(term_len, run.bytesize - 2 - body_len - term_len)
  body = run.byteslice(run.bytesize - 2 - body_len, body_len)
  keys = []
  at = 0
  read = lambda do
    v = 0
    shift = 0
    loop do
      byte = body.getbyte(at)
      at += 1
      v |= (byte & 0x7F) << shift
      shift += 7
      return v if (byte & 0x80).zero?
    end
  end
  while at < body.bytesize
    shared = read.call
    suffix = read.call
    prev = keys.last || last
    keys << (prev.byteslice(0, shared) + body.byteslice(at, suffix)).b
    at += suffix
  end
  keys << last
end

def gin_run_pack(term, keys)
  runs = []
  start = 0
  while start < keys.size
    stop = start + 1
    stop += 1 while stop < keys.size && gin_run_encode(term, keys[start..stop]).bytesize <= GIN_RUN_MAX_BYTES
    runs << keys[start...stop]
    start = stop
  end
  runs
end

def gin_run_split(term, keys)
  return [keys] if keys.size == 1 || gin_run_encode(term, keys).bytesize <= GIN_RUN_MAX_BYTES

  mid = (keys.size + 1) / 2
  gin_run_split(term, keys[0...mid]) + gin_run_split(term, keys[mid..])
end

# Add `key` (gin.md §4.5): the first run whose last key is >= key takes it, split in halves when it
# outgrows the limit; a key above every run joins the last run when it stays within the limit, else
# starts a new run.
def gin_run_insert(term, runs, key)
  i = runs.index { |r| r.last >= key }
  if i
    return runs if runs[i].include?(key)

    keys = (runs[i] + [key]).sort
    return runs[0...i] + gin_run_split(term, keys) + runs[(i + 1)..]
  end
  if !runs.empty? && gin_run_encode(term, runs.last + [key]).bytesize <= GIN_RUN_MAX_BYTES
    return runs[0...-1] + [runs.last + [key]]
  end
  runs + [[key]]
end

def gin_run_remove(term, runs, key)
  i = runs.index { |r| r.last >= key }
  return runs unless i && runs[i].include?(key)

  keys = runs[i] - [key]
  runs[0...i] + (keys.empty? ? [] : gin_run_split(term, keys)) + runs[(i + 1)..]
end

# Verify spec/encoding/gin_posting_runs.toml: each `[[run]]`'s encoding and decoding, each
# `[[pack]]`'s greedy packing, and each `[[history]]`'s maintenance result.
def check_gin_runs_file(filename)
  data = TomlRB.load_file(File.join(__dir__, filename))
  unhex = ->(h) { [h].pack("H*").b }
  checked = 0
  (data["run"] || []).each do |c|
    term = unhex.call(c["term"])
    keys = c["keys"].map(&unhex)
    got = hex(gin_run_encode(term, keys))
    fail!("gin run #{c['keys']}: #{got} want #{c['run']}") unless got == c["run"]
    fail!("gin run #{c['run']}: decode mismatch") unless gin_run_decode(term.bytesize, unhex.call(c["run"])) == keys
    checked += 1
  end
  (data["pack"] || []).each do |c|
    term = unhex.call(c["term"])
    got = gin_run_pack(term, c["keys"].map(&unhex)).map { |r| hex(gin_run_encode(term, r)) }
    fail!("gin pack #{c['keys']}: #{got} want #{c['runs']}") unless got == c["runs"]
    checked += 1
  end
  (data["history"] || []).each do |c|
    term = unhex.call(c["term"])
    runs = []
    c["ops"].each do |op|
      key = unhex.call(op[1..])
      runs = op.start_with?("+") ? gin_run_insert(term, runs, key) : gin_run_remove(term, runs, key)
    end
    got = runs.map { |r| hex(gin_run_encode(term, r)) }
    fail!("gin history #{c['name']}: #{got} want #{c['runs']}") unless got == c["runs"]
    checked += 1
  end
  checked
end

def label_of(c) = c["null"] ? "NULL" : c["value"]

def main
  path = File.join(__dir__, "integers.toml")
  data = TomlRB.load_file(path)
  checked = 0

  (data["bare"] || []).each do |group|
    t = group["type"]
    w = WIDTH.fetch(t)
    rows = []
    group["cases"].each do |c|
      v = c["value"]
      want = c["bytes"]
      got = hex(enc_bare(v, w))
      fail!("bare #{t} value=#{v}: encode=#{got} want=#{want}") unless got == want
      fail!("bare #{t} value=#{v}: round-trip mismatch") unless dec_bare([want].pack("H*").b, w) == v
      rows << [v, [want].pack("H*").b]
      checked += 1
    end
    check_order("bare #{t}", rows)
  end

  (data["nullable"] || []).each do |group|
    t = group["type"]
    w = WIDTH.fetch(t)
    rows = []
    group["cases"].each do |c|
      want = c["bytes"]
      got = hex(enc_nullable(c, w))
      fail!("nullable #{t} #{label_of(c)}: encode=#{got} want=#{want}") unless got == want
      rows << [label_of(c), [want].pack("H*").b]
      checked += 1
    end
    check_order("nullable #{t}", rows)
  end

  (data["descending"] || []).each do |group|
    t = group["type"]
    w = WIDTH.fetch(t)
    rows = []
    group["cases"].each do |c|
      want = c["bytes"]
      got = hex(invert(enc_nullable(c, w))) # descending = invert(ascending)
      fail!("descending #{t} #{label_of(c)}: encode=#{got} want=#{want}") unless got == want
      rows << [label_of(c), [want].pack("H*").b]
      checked += 1
    end
    check_order("descending #{t}", rows)
  end

  # Variable-width terminated-escape vectors (text §2.4, bytea §2.6) live in their own files —
  # their values are not fixed-WIDTH, so they take the dedicated terminated-escape path.
  checked += check_terminated_file("text.toml")
  checked += check_terminated_file("bytea.toml")

  # Decimal: the variable-width decimal-order-preserving rule (§2.5) — its own file, its own
  # value→bytes derivation (mantissa pairs + i32 exponent), not the WIDTH/terminated paths.
  checked += check_decimal_file("decimal.toml")

  # Interval: the interval-span-i128 rule (§2.10) — the 16-byte canonical span, int-be-signflip at
  # i128 width. Its own file: the value is a (months, days, micros) triple, not a fixed-WIDTH scalar.
  checked += check_interval_file("interval.toml")

  # Range: the range-bounds container rule (§2.11) — the first container key, recursing into the
  # element key with empty/±∞/inclusivity framing. Its own file: a bound-shape per case, not a scalar.
  checked += check_range_file("range.toml")

  # Float: the float-order-preserving rule (§2.8) — fixed-width (f64 = 8, f32 = 4), the canonicalized
  # IEEE bits with the sign-bit / all-bits flip. Its own file: the value is a float string (incl
  # Infinity/-Infinity/NaN), not a fixed-WIDTH integer scalar.
  checked += check_float_file("float.toml")

  # Composite: the composite-field-slots container rule (§2.15) — the third container key, recursing
  # into each field's key via the §2.2 nullable slot (fixed arity, no terminator). Its own file: a
  # heterogeneous field-value list per case, not a scalar.
  checked += check_composite_file("composite.toml")

  # GIN jsonb_ops terms (gin.md §4.1): not a key encoding of a value, but the inverted index's term
  # bytes, built from the text/decimal key rules above — a flag byte + a cut, terminated body.
  checked += check_gin_jsonb_file("gin_jsonb_terms.toml")

  # Ordered index slots (§2.16): every keyable type's body in all four ASC/DESC x NULLS orders.
  checked += check_ordered_file("ordered.toml")

  # GIN posting runs (gin.md §4.5): the run codec, the build's greedy packing, and run maintenance.
  checked += check_gin_runs_file("gin_posting_runs.toml")

  puts "OK: #{checked} vectors verified (round-trip + byte-exact + order)"
end

main if $PROGRAM_NAME == __FILE__
