# frozen_string_literal: true

require_relative '../lib/jed_native'

module RubyPendingPagesTest
  def self.run
    codec = JedNative::FileFormat.new
    full = JedNative::PageBuffer.new(8192, 515)
    compact = JedNative::PageBuffer.new(8192, 515, compact: true)
    [full, compact].each do |image|
      512.times do |index|
        # The fixed-fanout GiST format leaves most page bytes zero. Include
        # embedded/trailing zeros so compact storage cannot trim logical data.
        payload = [index, 0, index ^ 0xffff].pack('N3') + "payload\0".b
        codec.write_page(image, 8192, index + 2, 9, 1, 0, payload)
      end
      codec.write_meta(image, 8192, 0, 8192, 17, 2, 515, 512)
      image[8192 + 100, 7] = "a\0b\0c\0d".b
      image[8192 + 102, 2] = "\0\0".b
    end
    raise 'compacted pages changed image bytes' unless compact.to_s == full.to_s
    raise 'page enumeration omitted padding' unless compact.pages.to_h == full.pages
    raise 'pending pages retained padding' unless compact.pages.retained_bytes < 32_000
    [0, 1, 17, 32, 8191, 8192, 8193, 65535].each do |offset|
      raise 'partial read changed bytes' unless compact.byteslice(offset, 132) == full.byteslice(offset, 132)
    end
    copy = JedNative::PendingPages.new(8192).merge!(compact.pages)
    raise 'page adoption changed bytes' unless copy.to_h == full.pages
    raise 'page adoption retained padding' unless copy.retained_bytes < 25_000
    puts 'ruby-native pending pages: canonical bytes, checksums, partial writes, and bounded zero padding passed'
  end
end

RubyPendingPagesTest.run
