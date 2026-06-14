#!/usr/bin/env ruby
# frozen_string_literal: true

# Verify spec/encoding/prng.toml against the entropy-seam contract in spec/design/entropy.md.
# INDEPENDENT reference implementation (CLAUDE.md §5/§8): recomputes the splitmix64 stream and the
# v4/v7 UUID bytes from scratch — the Ruby "third voice" beyond the Rust/Go/TS cores, so a bug all
# three cores share is still caught. Test-time only; run via `rake verify`.
#
#   bundle exec ruby spec/encoding/prng_verify.rb
#
# Exit 0 = every fixture conforms; nonzero = mismatch (prints the offending case).

require "bundler/setup"
require "toml-rb"

MASK64 = 0xFFFFFFFFFFFFFFFF
GAMMA  = 0x9E3779B97F4A7C15
MIX1   = 0xBF58476D1CE4E5B9
MIX2   = 0x94D049BB133111EB
GREGORIAN_OFFSET_100NS = 122_192_928_000_000_000

def fail!(msg)
  warn "FAIL: #{msg}"
  exit 1
end

# splitmix64 (entropy.md §2): state starts = seed; each step adds the gamma then mixes.
class StmtRng
  def initialize(seed)
    @state = seed & MASK64
    @counter = 0
  end

  def next_u64
    @state = (@state + GAMMA) & MASK64
    x = @state
    x = ((x ^ (x >> 30)) * MIX1) & MASK64
    x = ((x ^ (x >> 27)) * MIX2) & MASK64
    x ^ (x >> 31)
  end

  def next_counter
    c = @counter & 0x0FFF
    @counter += 1
    c
  end
end

def u64_be(v)
  Array.new(8) { |i| (v >> (8 * (7 - i))) & 0xFF }
end

def render_uuid(bytes)
  h = bytes.map { |b| format("%02x", b) }.join
  "#{h[0, 8]}-#{h[8, 4]}-#{h[12, 4]}-#{h[16, 4]}-#{h[20, 12]}"
end

# uuidv4 (entropy.md §3): 16 random bytes (two draws), version 4 + RFC variant.
def build_v4(rng)
  b = u64_be(rng.next_u64) + u64_be(rng.next_u64)
  b[6] = (b[6] & 0x0F) | 0x40
  b[8] = (b[8] & 0x3F) | 0x80
  b
end

# uuidv7 (entropy.md §3): 48-bit ms + a 12-bit monotonic counter in rand_a + one random draw in
# rand_b; version 7 + RFC variant.
def build_v7(rng, clock_micros)
  unix_ms = clock_micros / 1000 # fixtures use a positive clock; floor == trunc here
  ms_bytes = Array.new(6) { |i| (unix_ms >> (8 * (5 - i))) & 0xFF }
  counter = rng.next_counter & 0x0FFF
  rand_b = u64_be(rng.next_u64) # bytes 8..15 (one draw)
  b = ms_bytes + [0, 0] + rand_b # ms(6) + rand_a(2) + rand_b(8) = 16
  b[6] = 0x70 | ((counter >> 8) & 0x0F)
  b[7] = counter & 0xFF
  b[8] = (b[8] & 0x3F) | 0x80
  b
end

def main
  data = TomlRB.load_file(File.join(__dir__, "prng.toml"))
  fail!("prng.toml: schema_version must be 1") unless data["schema_version"] == 1
  n = 0

  (data["stream"] || []).each do |s|
    rng = StmtRng.new(s["seed"])
    s["outputs"].each_with_index do |hex, i|
      got = format("%016x", rng.next_u64)
      fail!("stream seed=#{s['seed']} draw #{i}: got #{got}, want #{hex}") unless got == hex
      n += 1
    end
  end

  (data["uuidv4"] || []).each do |v|
    got = render_uuid(build_v4(StmtRng.new(v["seed"])))
    fail!("uuidv4 seed=#{v['seed']}: got #{got}, want #{v['uuid']}") unless got == v["uuid"]
    n += 1
  end

  (data["uuidv7"] || []).each do |v|
    rng = StmtRng.new(v["seed"])
    v["uuids"].each_with_index do |want, i|
      got = render_uuid(build_v7(rng, v["clock_micros"]))
      fail!("uuidv7 seed=#{v['seed']} #[#{i}]: got #{got}, want #{want}") unless got == want
      n += 1
    end
  end

  puts "OK: #{n} PRNG/UUID vectors verified (splitmix64 + v4/v7 byte layout)"
end

main
