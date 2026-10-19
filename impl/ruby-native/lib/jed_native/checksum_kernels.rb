# frozen_string_literal: true

module JedNative
  # CRC64 slicing by four folds four input bytes per Ruby loop iteration. The
  # ECMA state stays in two immediate 32-bit integers, avoiding per-byte Bignums.
  # CRC32 uses the standard library; Zlib does not provide CRC64/ECMA-182.
  module ChecksumKernels
    CRC64_SLICES = begin
      slices = [FileFormat::CRC64_TABLE]
      3.times do
        slices << slices.last.map do |value|
          ((value << 8) & 0xffffffffffffffff) ^ slices[0][value >> 56]
        end.freeze
      end
      slices.freeze
    end
    CRC64_HIGH_SLICES = CRC64_SLICES.map { |slice| slice.map { |value| value >> 32 }.freeze }.freeze
    CRC64_LOW_SLICES = CRC64_SLICES.map { |slice| slice.map { |value| value & 0xffffffff }.freeze }.freeze

    def crc64(data, crc = 0)
      high, low = crc >> 32, crc & 0xffffffff
      index, length = 0, data.bytesize
      last = length - 4
      h0, h1, h2, h3 = CRC64_HIGH_SLICES
      l0, l1, l2, l3 = CRC64_LOW_SLICES
      while index <= last
        word = high ^ ((data.getbyte(index) << 24) | (data.getbyte(index + 1) << 16) |
                       (data.getbyte(index + 2) << 8) | data.getbyte(index + 3))
        a, b, c, d = word >> 24, (word >> 16) & 255, (word >> 8) & 255, word & 255
        high = low ^ h3[a] ^ h2[b] ^ h1[c] ^ h0[d]
        low = l3[a] ^ l2[b] ^ l1[c] ^ l0[d]
        index += 4
      end
      while index < length
        slot = (high >> 24) ^ data.getbyte(index)
        high = ((high << 8) & 0xffffffff) ^ (low >> 24) ^ h0[slot]
        low = ((low << 8) & 0xffffffff) ^ l0[slot]
        index += 1
      end
      (high << 32) | low
    end
  end
  FileFormat.prepend(ChecksumKernels)
end
