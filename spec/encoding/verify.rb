#!/usr/bin/env ruby
# frozen_string_literal: true

# Verify spec/encoding/integers.toml against the encoding rules in
# spec/design/encoding.md. Independent reference encoder — recomputes the bytes
# from scratch and checks the three invariants (round-trip, byte-exactness, order)
# rather than trusting the file. Test-time only (CLAUDE.md §5).
#
#   bundle exec ruby spec/encoding/verify.rb   (or: rake verify)
#
# Exit 0 = all vectors conform; nonzero = mismatch (prints the offending case).

require "bundler/setup"
require "toml-rb"

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

  puts "OK: #{checked} vectors verified (round-trip + byte-exact + order)"
end

main
