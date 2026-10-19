# frozen_string_literal: true
require_relative '../lib/jed_native/storage'

module NativeChecksumTests
  module_function

  # Deliberately bitwise: the shared polynomials are the oracle for every short
  # tail/alignment, independently of the production slicing tables.
  def crc32(bytes)
    value = 0xffffffff
    bytes.each_byte do |byte|
      value ^= byte
      8.times { value = (value >> 1) ^ (value.odd? ? 0xedb88320 : 0) }
    end
    value ^ 0xffffffff
  end

  def crc64(bytes, value)
    bytes.each_byte do |byte|
      value ^= byte << 56
      8.times do
        high = value >> 63
        value = ((value << 1) & 0xffffffffffffffff) ^ (high.zero? ? 0 : 0x42f0e1eba9ea3693)
      end
    end
    value
  end

  def run
    codec = JedNative::FileFormat.new
    raise 'shared CRC32 vector' unless codec.crc32('123456789') == 0xcbf43926
    raise 'shared CRC64 vector' unless codec.crc64('123456789') == 0x6c40df5f0b497347
    bytes = (0..255).to_a.reverse.pack('C*') * 2
    260.times do |length|
      input = bytes.byteslice(0, length)
      raise "CRC32 tail #{length}" unless codec.crc32(input) == crc32(input)
      [0, 0x83f0123456789abc].each do |seed|
        expected = crc64(input, seed)
        raise "CRC64 tail #{length}" unless codec.crc64(input, seed) == expected
        split = length / 2
        partial = codec.crc64(input.byteslice(0, split), seed)
        raise "CRC64 continuation #{length}" unless codec.crc64(input.byteslice(split..), partial) == expected
      end
    end
    puts 'ruby-native checksums: shared vectors, byte tails, and seeded continuation passed'
  end
end
NativeChecksumTests.run
