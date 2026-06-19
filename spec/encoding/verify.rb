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

  puts "OK: #{checked} vectors verified (round-trip + byte-exact + order)"
end

main
