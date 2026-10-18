# frozen_string_literal: true

require_relative 'timezone_data'

module JedNative
  class UnicodeData
    Reader = TimeZoneData::Reader
    attr_reader :bytes, :collations, :properties, :unicode_version, :cldr_version, :description
    def initialize(bytes)
      @bytes = bytes.b.dup.freeze
      crc = FileFormat.allocate
      raise Error.new('XX001', 'invalid Unicode bundle checksum') if @bytes.bytesize < 12 || crc.crc32(@bytes.byteslice(0, @bytes.bytesize - 4)) != @bytes.byteslice(-4, 4).unpack1('N')
      reader = Reader.new(@bytes)
      raise Error.new('XX001', 'invalid Unicode bundle header') unless reader.take(6) == "JUCD\0\0" && reader.u16 == 1
      @unicode_version, @cldr_version, @description = reader.string, reader.string, reader.string
      sections = Array.new(reader.u16) { [reader.take(1).getbyte(0), reader.string, reader.u32, reader.u32, reader.u32, reader.u32] }
      root = nil
      deltas = []
      body_end = reader.position
      sections.each do |kind, name, checksum, raw_length, length, position|
        raise Error.new('XX001', 'invalid Unicode section bounds') unless position == body_end && position + length <= @bytes.bytesize - 4
        raw = LZ4.decompress(@bytes.byteslice(position, length), raw_length)
        raise Error.new('XX001', 'Unicode section checksum mismatch') unless crc.crc32(raw) == checksum
        case kind
        when 0
          raise Error.new('XX001', 'duplicate Unicode properties') if @properties
          @properties = Properties.new(raw)
        when 1
          raise Error.new('XX001', 'duplicate Unicode root') if root
          root = [name, Collation.read_table(raw)]
        when 2 then deltas << [name, Collation.read_table(raw)]
        else raise Error.new('XX001', 'unknown Unicode section kind')
        end
        body_end = position + length
      end
      raise Error.new('XX001', 'trailing Unicode bundle data') unless body_end == @bytes.bytesize - 4
      raise Error.new('XX001', 'Unicode tailoring requires a root') if !root && !deltas.empty?
      @collations = {}
      if root
        ([root] + deltas.map { |name, maps| [name, [root[1][0].merge(maps[0]), root[1][1].merge(maps[1])]] }).each do |name, maps|
          raise Error.new('XX001', 'duplicate Unicode collation') if @collations.key?(name)
          @collations[name] = Collation.new(name, *maps, unicode_version: @unicode_version, cldr_version: @cldr_version, description: @description)
        end
      end
      @collations.freeze
      freeze
    rescue Error
      raise
    rescue StandardError => error
      raise Error.new('XX001', "invalid Unicode data: #{error.message}")
    end

    class Collation
      attr_reader :name, :unicode_version, :cldr_version, :description, :checksum, :singles, :contractions
      def self.read_table(bytes)
        reader = Reader.new(bytes)
        raise Error.new('XX001', 'invalid collation layout') unless reader.take(1).getbyte(0) == 1
        single_count, contraction_count = reader.u32, reader.u32
        singles, contractions = {}, {}
        last = -1
        single_count.times do
          codepoint = reader.u32
          raise Error.new('XX001', 'invalid collation code point order') unless codepoint > last && codepoint <= 0x10ffff
          last = codepoint
          singles[codepoint] = read_elements(reader)
        end
        last = nil
        contraction_count.times do
          length = reader.take(1).getbyte(0)
          raise Error.new('XX001', 'invalid collation contraction') if length < 2
          sequence = reader.take(length * 4).unpack('N*').freeze
          raise Error.new('XX001', 'invalid contraction order') if last && (last <=> sequence) >= 0
          last = sequence
          contractions[sequence] = read_elements(reader)
        end
        raise Error.new('XX001', 'trailing collation data') unless reader.position == bytes.bytesize
        [singles, contractions]
      end
      def self.read_elements(reader)
        count = reader.take(1).getbyte(0)
        raise Error.new('XX001', 'empty collation element') if count.zero?
        Array.new(count) do
          element = reader.take(7).unpack('Cnnn')
          raise Error.new('XX001', 'unknown collation element flags') if element[0] > 1
          element.freeze
        end.freeze
      end
      def initialize(name, singles, contractions, unicode_version:, cldr_version:, description: '')
        @name, @unicode_version, @cldr_version, @description = name, unicode_version, cldr_version, description
        @singles, @contractions = singles.sort.to_h.freeze, contractions.sort.to_h.freeze
        @contraction_lengths = @contractions.keys.map(&:length).uniq.sort.reverse.freeze
        @checksum = FileFormat.allocate.crc32(table_bytes)
        freeze
      end
      def table_bytes
        output = [1, @singles.length, @contractions.length].pack('CNN')
        @singles.each do |codepoint, elements|
          output << [codepoint, elements.length].pack('NC')
          elements.each { |element| output << element.pack('Cnnn') }
        end
        @contractions.each do |sequence, elements|
          output << [sequence.length].pack('C') << sequence.pack('N*') << [elements.length].pack('C')
          elements.each { |element| output << element.pack('Cnnn') }
        end
        output
      end
      def sort_key(text)
        points, elements, index = text.codepoints, [], 0
        while index < points.length
          matched = @contraction_lengths.find do |length|
            next false if index + length > points.length
            candidate = @contractions[points[index, length]]
            if candidate
              elements.concat(candidate)
              index += length
              true
            end
          end
          next if matched
          entry = @singles[points[index]]
          raise Error.new('0A000', 'collation code point has no mapping') unless entry
          elements.concat(entry)
          index += 1
        end
        output = +''.b
        (1..3).each do |level|
          elements.each { |element| output << [element[level]].pack('n') unless element[level].zero? }
          output << "\0\0"
        end
        output << text.b.gsub("\0", "\0\xff".b) << "\0\1"
        output
      end
    end

    class Properties
      def initialize(bytes)
        reader = Reader.new(bytes)
        raise Error.new('XX001', 'invalid Unicode property layout') unless reader.take(1).getbyte(0) == 1
        @simple, @special = {}, {}
        last = -1
        reader.u32.times do
          cp, *mapping = reader.take(16).unpack('N4')
          raise Error.new('XX001', 'invalid Unicode property order') unless cp > last && ([cp] + mapping).all? { |point| point <= 0x10ffff && !point.between?(0xd800, 0xdfff) }
          last = cp
          @simple[cp] = mapping.freeze
        end
        last = -1
        reader.u32.times do
          cp = reader.u32
          raise Error.new('XX001', 'invalid Unicode special casing order') unless cp > last
          last = cp
          @special[cp] = Array.new(3) { reader.take(reader.take(1).getbyte(0) * 4).unpack('N*').pack('U*').freeze }.freeze
        end
        raise Error.new('XX001', 'trailing Unicode property data') unless reader.position == bytes.bytesize
        @simple.freeze
        @special.freeze
        freeze
      end
      def fold(text, direction, simple: false)
        index = direction == :upper ? 0 : direction == :lower ? 1 : 2
        output = +''
        text.each_codepoint do |point|
          special = !simple && @special[point]
          output << (special ? special[index] : (@simple[point]&.[](index) || point).chr(Encoding::UTF_8))
        end
        output
      end
    end
  end
end
