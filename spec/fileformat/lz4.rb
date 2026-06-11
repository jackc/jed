# frozen_string_literal: true

# Reference implementation of the pinned LZ4-block codec (lz4.md). Pure Ruby, no gem
# dependency; test-time only (CLAUDE.md S5). The encoder's free parameters are FIXED by
# lz4.md S2 — greedy match search, step 1, a 4096-entry single-candidate hash table, no
# backward extension — so the compressed bytes are identical across Rust/Go/TS/Ruby
# (pinned by lz4_vectors.toml and the compressed_table.jed golden).
module LZ4
  MIN_MATCH = 4
  MAX_OFFSET = 65_535
  MFLIMIT = 12       # no match may start after len - 12
  LAST_LITERALS = 5  # no match may extend past len - 5
  HASH_LOG = 12
  HASH_MUL = 2_654_435_761

  def self.hash32(v) = ((v * HASH_MUL) & 0xFFFFFFFF) >> (32 - HASH_LOG)

  def self.le32(src, pos)
    src.getbyte(pos) | (src.getbyte(pos + 1) << 8) |
      (src.getbyte(pos + 2) << 16) | (src.getbyte(pos + 3) << 24)
  end

  # length-extension bytes for a nibble that hit 15: 255* then the remainder (lz4.md S1).
  def self.emit_length(out, n)
    while n >= 255
      out << 255.chr
      n -= 255
    end
    out << n.chr
  end

  def self.emit_sequence(out, literals, offset, mlen)
    lit = literals.bytesize
    ml = mlen - MIN_MATCH
    out << (((lit < 15 ? lit : 15) << 4) | (ml < 15 ? ml : 15)).chr
    emit_length(out, lit - 15) if lit >= 15
    out << literals
    out << (offset & 0xFF).chr << ((offset >> 8) & 0xFF).chr # u16 LITTLE-endian (lz4.md S1)
    emit_length(out, ml - 15) if ml >= 15
  end

  def self.emit_last_literals(out, literals)
    lit = literals.bytesize
    out << ((lit < 15 ? lit : 15) << 4).chr
    emit_length(out, lit - 15) if lit >= 15
    out << literals
  end

  # The pinned encoder (lz4.md S2). Deterministic: one input -> one output, in every core.
  def self.compress(src)
    src = src.b
    out = +"".b
    table = Array.new(1 << HASH_LOG, -1)
    anchor = 0
    pos = 0
    limit = src.bytesize - MFLIMIT # last legal match start (may be negative)
    while pos <= limit
      h = hash32(le32(src, pos))
      cand = table[h]
      table[h] = pos # store AFTER reading the candidate
      if cand >= 0 && pos - cand <= MAX_OFFSET && le32(src, cand) == le32(src, pos)
        mlen = MIN_MATCH
        maxend = src.bytesize - LAST_LITERALS
        mlen += 1 while pos + mlen < maxend && src.getbyte(cand + mlen) == src.getbyte(pos + mlen)
        emit_sequence(out, src.byteslice(anchor, pos - anchor), pos - cand, mlen)
        pos += mlen # positions inside the match are NOT hashed
        anchor = pos
      else
        pos += 1 # step is always 1 (no acceleration)
      end
    end
    emit_last_literals(out, src.byteslice(anchor, src.bytesize - anchor))
    out
  end

  # The decoder (lz4.md S3): total and safe — every read bounds-checked, output capped at
  # raw_len, malformed input raises (the cores map this to data_corrupted).
  def self.decompress(comp, raw_len)
    comp = comp.b
    out = +"".b
    i = 0
    n = comp.bytesize
    loop do
      raise "truncated block" if i >= n

      token = comp.getbyte(i)
      i += 1
      lit = token >> 4
      if lit == 15
        loop do
          raise "truncated block" if i >= n

          b = comp.getbyte(i)
          i += 1
          lit += b
          break if b != 255
        end
      end
      raise "truncated block" if i + lit > n
      raise "decompressed length overflow" if out.bytesize + lit > raw_len

      out << comp.byteslice(i, lit)
      i += lit
      break if i == n # a literals-only tail ends the block

      raise "truncated block" if i + 2 > n

      offset = comp.getbyte(i) | (comp.getbyte(i + 1) << 8)
      i += 2
      raise "invalid match offset" if offset.zero? || offset > out.bytesize

      ml = token & 0x0F
      if ml == 15
        loop do
          raise "truncated block" if i >= n

          b = comp.getbyte(i)
          i += 1
          ml += b
          break if b != 255
        end
      end
      ml += MIN_MATCH
      raise "decompressed length overflow" if out.bytesize + ml > raw_len

      from = out.bytesize - offset
      ml.times { |k| out << out.getbyte(from + k).chr } # byte-by-byte; overlap replicates
    end
    raise "decompressed length mismatch" unless out.bytesize == raw_len

    out
  end
end
