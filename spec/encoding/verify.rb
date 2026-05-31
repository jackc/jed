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

WIDTH = { "int16" => 2, "int32" => 4, "int64" => 8 }.freeze

# bare int-be-signflip: add bias 2^(bits-1), emit as unsigned big-endian.
def enc_bare(value, width)
  bias = 1 << (width * 8 - 1)
  u = value + bias
  raise "value #{value} out of range for width #{width}" unless u.between?(0, (1 << (width * 8)) - 1)

  # to fixed-width big-endian bytes
  Array.new(width) { |i| (u >> (8 * (width - 1 - i))) & 0xFF }.pack("C*")
end

def dec_bare(bytes, width)
  bytes.bytes.reduce(0) { |acc, b| (acc << 8) | b } - (1 << (width * 8 - 1))
end

# nullable slot: 0x00 = NULL, 0x01 = present + value bytes.
def enc_nullable(c, width)
  return [0x00].pack("C") if c["null"]

  [0x01].pack("C") + enc_bare(c["value"], width)
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
  # Read as UTF-8 explicitly rather than TomlRB.load_file: under a non-UTF-8 locale
  # (e.g. the container's POSIX/US-ASCII default) the file would be tagged US-ASCII
  # and clash with toml-rb's UTF-8 grammar. Spec files are UTF-8 regardless of locale.
  data = TomlRB.parse(File.read(path, encoding: "UTF-8"))
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
