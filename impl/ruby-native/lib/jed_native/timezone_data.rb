# frozen_string_literal: true

require_relative 'file_format'

module JedNative
  # A host supplies the complete, immutable JTZ image. SQL can only look up the
  # names already loaded; this reader never consults the operating system.
  class TimeZoneData
    class Reader
      attr_reader :position
      def initialize(bytes) = (@bytes = bytes.b; @position = 0)
      def take(length)
        raise Error.new('XX001', 'truncated timezone data') if length.negative? || @position + length > @bytes.bytesize
        value = @bytes.byteslice(@position, length)
        @position += length
        value
      end
      def u16 = take(2).unpack1('n')
      def u32 = take(4).unpack1('N')
      def string
        value = take(u16).force_encoding(Encoding::UTF_8)
        raise Error.new('XX001', 'invalid timezone name encoding') unless value.valid_encoding?
        value.freeze
      end
    end

    attr_reader :zones, :bytes, :version, :description
    def initialize(bytes)
      @bytes = bytes.b.dup.freeze
      crc = FileFormat.allocate
      raise Error.new('XX001', 'invalid timezone bundle checksum') if @bytes.bytesize < 12 || crc.crc32(@bytes.byteslice(0, @bytes.bytesize - 4)) != @bytes.byteslice(-4, 4).unpack1('N')
      reader = Reader.new(@bytes)
      raise Error.new('XX001', 'invalid timezone bundle header') unless reader.take(6) == "JTZ\0\0\0" && reader.u16 == 1
      @version, @description = reader.string, reader.string
      count, links_count = reader.u16, reader.u16
      sections = Array.new(count) { [reader.string, reader.u32, reader.u32, reader.u32, reader.u32] }
      links = Array.new(links_count) { [reader.string, reader.string] }
      names = sections.map(&:first)
      aliases = links.map(&:first)
      unless names == names.sort && names.uniq.length == names.length && aliases == aliases.sort && aliases.uniq.length == aliases.length && (names & aliases).empty?
        raise Error.new('XX001', 'invalid timezone manifest ordering')
      end
      @zones = {}
      body_end = reader.position
      sections.each do |name, checksum, raw_length, length, position|
        raise Error.new('XX001', 'invalid timezone section bounds') if name.empty? || position != body_end || position + length > @bytes.bytesize - 4
        raw = LZ4.decompress(@bytes.byteslice(position, length), raw_length)
        raise Error.new('XX001', 'timezone section checksum mismatch') unless crc.crc32(raw) == checksum
        @zones[name] = Zone.new(raw, name: name, version: @version, checksum: checksum)
        body_end = position + length
      end
      raise Error.new('XX001', 'trailing timezone bundle data') unless body_end == @bytes.bytesize - 4
      links.each do |name, target|
        raise Error.new('XX001', 'invalid timezone alias target') unless @zones.key?(target)
        @zones[name] = @zones[target]
      end
      @zones.freeze
      freeze
    rescue Error
      raise
    rescue StandardError => error
      raise Error.new('XX001', "invalid timezone bundle: #{error.message}")
    end

    def self.finite(micros)
      raise Error.new('22008', 'timestamp out of range') unless micros.between?(-(2**63) + 1, 2**63 - 2)
      micros
    end

    def self.fixed(name)
      return Fixed.new(0) if name == 'UTC'
      match = /\A([+-])(\d{1,3})(?::(\d{1,2})(?::(\d{1,2}))?)?\z/.match(name)
      return nil unless match
      hours, minutes, seconds = match[2].to_i, match[3].to_i, match[4].to_i
      return nil if hours > 167 || minutes > 59 || seconds > 59
      Fixed.new((hours * 3600 + minutes * 60 + seconds) * (match[1] == '+' ? -1 : 1))
    end

    module Conversion
      def instant_to_local(micros) = TimeZoneData.finite(micros + offset_at(micros.div(1_000_000))[0] * 1_000_000)
      def local_to_instant(micros)
        seconds = micros.div(1_000_000)
        low, high = seconds - 86_400, seconds + 86_400
        before, after = offset_at(low)[0], offset_at(high)[0]
        offset = before
        unless before == after
          while low < high
            middle = (low + high).div(2)
            offset_at(middle)[0] == before ? low = middle + 1 : high = middle
          end
          first, second = seconds - before, seconds - after
          offset = if (first < low) == (second < low)
            first < low ? before : after
          else
            first > second ? before : after
          end
        end
        TimeZoneData.finite(micros - offset * 1_000_000)
      end
    end

    class Fixed
      include Conversion
      def initialize(offset)
        h, rest = offset.abs.divmod(3600)
        m, s = rest.divmod(60)
        abbreviation = offset.zero? ? 'UTC' : format('%s%02d:%02d', offset.negative? ? '-' : '+', h, m) + (s.zero? ? '' : format(':%02d', s))
        @type = [offset, abbreviation.freeze, false].freeze
        freeze
      end
      def offset_at(_seconds) = @type
    end

    class Zone
      include Conversion
      attr_reader :name, :version, :checksum
      def initialize(bytes, name:, version:, checksum:)
        @name, @version, @checksum = name, version, checksum
        reader = Reader.new(bytes)
        version_byte, counts = header(reader)
        size = 4
        if version_byte != 0
          reader.take(block_length(counts, 4))
          repeated, counts = header(reader)
          raise Error.new('XX001', 'mismatched TZif headers') unless repeated == version_byte
          size = 8
        end
        ut_count, std_count, leap_count, time_count, type_count, char_count = counts
        raise Error.new('XX001', 'invalid TZif type count') unless type_count.between?(1, 256)
        @transitions = reader.take(time_count * size).unpack(size == 8 ? 'q>*' : 'l>*').freeze
        raise Error.new('XX001', 'unordered TZif transitions') unless @transitions.each_cons(2).all? { |a, b| a < b }
        @indices = reader.take(time_count).bytes.freeze
        raw_types = Array.new(type_count) { reader.take(6).unpack('l>CC') }
        designations = reader.take(char_count)
        @types = raw_types.map do |offset, dst, index|
          finish = designations.index("\0", index)
          raise Error.new('XX001', 'invalid TZif local type') unless dst <= 1 && finish && @indices.all? { |i| i < type_count }
          [offset, designations.byteslice(index, finish - index).force_encoding(Encoding::UTF_8).freeze, dst == 1].freeze
        end.freeze
        @initial = @types.find { |type| !type[2] } || @types.first
        reader.take(leap_count * (size + 4) + std_count + ut_count)
        if version_byte != 0
          footer = reader.take(bytes.bytesize - reader.position)
          raise Error.new('XX001', 'invalid TZif footer') unless footer.start_with?("\n") && footer.end_with?("\n")
          @footer = footer.bytesize > 2 ? PosixRule.new(footer.byteslice(1, footer.bytesize - 2)) : nil
        end
        freeze
      end
      def header(reader)
        raise Error.new('XX001', 'invalid TZif magic') unless reader.take(4) == 'TZif'
        version = reader.take(1).getbyte(0)
        raise Error.new('XX001', 'unsupported TZif version') unless [0, 50, 51].include?(version)
        reader.take(15)
        [version, reader.take(24).unpack('N6')]
      end
      def block_length(counts, size)
        ut, std, leap, times, types, chars = counts
        times * (size + 1) + types * 6 + chars + leap * (size + 4) + ut + std
      end
      def offset_at(seconds)
        return @footer ? @footer.offset_at(seconds) : @initial if @transitions.empty?
        return @initial if seconds < @transitions.first
        return @footer.offset_at(seconds) if @footer && seconds >= @transitions.last
        index = @transitions.bsearch_index { |time| time > seconds }
        @types[@indices[(index || @transitions.length) - 1]]
      end
    end

    class PosixRule
      def initialize(text)
        @text, @position = text, 0
        standard = name
        offset = -offset()
        @standard = [offset, standard, false].freeze
        if @position < @text.length
          daylight = name
          dst_offset = char == ',' || !char ? offset + 3600 : -offset()
          @daylight = [dst_offset, daylight, true].freeze
          expect(',')
          @start = rule
          expect(',')
          @finish = rule
        end
        raise Error.new('XX001', 'trailing POSIX timezone data') if @position != @text.length
        freeze
      end
      def char = @text[@position]
      def expect(value)
        raise Error.new('XX001', 'invalid POSIX timezone rule') unless char == value
        @position += 1
      end
      def number
        first = @position
        @position += 1 while char && char >= '0' && char <= '9'
        raise Error.new('XX001', 'missing POSIX timezone number') if first == @position
        @text[first...@position].to_i
      end
      def name
        first = @position
        if char == '<'
          @position += 1
          first = @position
          @position += 1 while char && char != '>'
          result = @text[first...@position]
          expect('>')
        else
          @position += 1 while char && /[A-Za-z]/.match?(char)
          result = @text[first...@position]
        end
        raise Error.new('XX001', 'invalid POSIX timezone name') if result.length < 3
        result.freeze
      end
      def offset
        sign = char == '-' ? -1 : 1
        @position += 1 if char == '-' || char == '+'
        hour = number
        minute = second = 0
        if char == ':'
          @position += 1
          minute = number
          if char == ':'
            @position += 1
            second = number
          end
        end
        raise Error.new('XX001', 'invalid POSIX timezone offset') if hour > 167 || minute > 59 || second > 59
        sign * (hour * 3600 + minute * 60 + second)
      end
      def rule
        if char == 'M'
          @position += 1
          month = number
          expect('.')
          week = number
          expect('.')
          weekday = number
          raise Error.new('XX001', 'invalid POSIX transition date') unless month.between?(1, 12) && week.between?(1, 5) && weekday.between?(0, 6)
          date = [:month, month, week, weekday]
        else
          julian = char == 'J'
          @position += 1 if julian
          day = number
          raise Error.new('XX001', 'invalid POSIX transition day') unless day.between?(julian ? 1 : 0, 365)
          date = [julian ? :julian : :ordinal, day]
        end
        time = 7200
        if char == '/'
          @position += 1
          time = offset
        end
        [date.freeze, time].freeze
      end
      def transition(year, rule, offset)
        date, time = rule
        if date[0] == :month
          _, month, week, weekday = date
          first = DateValue.from_parts(year, month, 1).days
          day = 1 + (weekday - (first + 4) % 7) % 7 + (week - 1) * 7
          following = month == 12 ? DateValue.from_parts(year + 1, 1, 1).days : DateValue.from_parts(year, month + 1, 1).days
          day -= 7 if first + day - 1 >= following
          days = first + day - 1
        else
          days = DateValue.from_parts(year, 1, 1).days + date[1]
          if date[0] == :julian
            days -= 1
            leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
            days += 1 if leap && date[1] >= 60
          end
        end
        days * 86_400 + time - offset
      end
      def offset_at(seconds)
        return @standard unless @daylight
        year = DateValue.new(seconds.div(86_400)).parts[0]
        first = transition(year, @start, @standard[0])
        last = transition(year, @finish, @daylight[0])
        daylight = first < last ? seconds >= first && seconds < last : seconds >= first || seconds < last
        daylight ? @daylight : @standard
      end
    end
  end
end
