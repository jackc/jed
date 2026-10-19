# frozen_string_literal: true

module JedNative
  # Slicing by four folds four input bytes per Ruby loop iteration. The ECMA
  # state stays in two immediate 32-bit integers, avoiding per-byte Bignums.
  # Tables derive from the same polynomials as the byte-at-a-time codec.
  module ChecksumKernels
    CRC32_SLICES = begin
      slices = [FileFormat::CRC32_TABLE]
      3.times do
        slices << slices.last.map { |value| (value >> 8) ^ slices[0][value & 255] }.freeze
      end
      slices.freeze
    end
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

    def crc32(data)
      crc, index, length = 0xffffffff, 0, data.bytesize
      last = length - 4
      t0, t1, t2, t3 = CRC32_SLICES
      while index <= last
        crc ^= data.getbyte(index) | (data.getbyte(index + 1) << 8) |
               (data.getbyte(index + 2) << 16) | (data.getbyte(index + 3) << 24)
        crc = t3[crc & 255] ^ t2[(crc >> 8) & 255] ^ t1[(crc >> 16) & 255] ^ t0[crc >> 24]
        index += 4
      end
      while index < length
        crc = (crc >> 8) ^ t0[(crc ^ data.getbyte(index)) & 255]
        index += 1
      end
      crc ^ 0xffffffff
    end

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
