# frozen_string_literal: true
require_relative '../lib/jed_native/storage'

module NativeChecksumTests
  module_function

  # Deliberately bitwise: the shared polynomials are the oracle for every short
  # tail/alignment, independently of the production slicing tables.
  def crc32(bytes, value = 0)
    value ^= 0xffffffff
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
      [0, 0x83f01234, 0xffffffff].each do |seed|
        expected = crc32(input, seed)
        raise "CRC32 tail #{length}" unless codec.crc32(input, seed) == expected
        split = length / 2
        partial = codec.crc32(input.byteslice(0, split), seed)
        raise "CRC32 continuation #{length}" unless codec.crc32(input.byteslice(split..), partial) == expected
      end
      [0, 0x83f0123456789abc].each do |seed|
        expected = crc64(input, seed)
        raise "CRC64 tail #{length}" unless codec.crc64(input, seed) == expected
        split = length / 2
        partial = codec.crc64(input.byteslice(0, split), seed)
        raise "CRC64 continuation #{length}" unless codec.crc64(input.byteslice(split..), partial) == expected
      end
    end
    [256, 8192, 65536].each do |size|
      page = (bytes * (size / bytes.bytesize + 1)).byteslice(0, size)
      expected_page = crc32(page.byteslice(0, 12) + page.byteslice(16..))
      expected_meta = crc32(page.byteslice(0, 32) + page.byteslice(36..))
      raise "CRC32 page spans #{size}" unless codec.page_crc(page) == expected_page
      raise "CRC32 meta spans #{size}" unless codec.meta_crc(page) == expected_meta
      changed_page = page.dup
      changed_page[12, 4] = "\xff\x00\x80\x7f".b
      raise 'page checksum included its own field' unless codec.page_crc(changed_page) == expected_page
      changed_meta = page.dup
      changed_meta[32, 4] = "\xff\x00\x80\x7f".b
      raise 'meta checksum included its own field' unless codec.meta_crc(changed_meta) == expected_meta
    end
    puts 'ruby-native checksums: shared vectors, byte tails, seeded continuation, and page spans passed'
  end
end
NativeChecksumTests.run
