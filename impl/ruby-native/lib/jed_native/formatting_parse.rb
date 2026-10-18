# frozen_string_literal: true

module JedNative
  class DateTemplateReader
    attr_reader :fields
    def initialize(input, template)
      @input, @nodes, @position = input, DateFormatting.template(template), 0
      @fields = Hash.new(0)
      @mode = nil
    end
    def char = @input[@position]
    def space?(value) = value && " \t\n\r\v\f".include?(value)
    def separator?(value) = value && /[\x21-\x2f\x3a-\x40\x5b-\x60\x7b-\x7e]/.match?(value)
    def invalid(message = 'invalid datetime format') = raise(Error.new('22007', message))
    def set(field, value)
      invalid('conflicting datetime fields') if @fields[field] != 0 && @fields[field] != value
      @fields[field] = value
    end
    def read_integer(field, width, index)
      node = @nodes[index]
      initial = @position
      @position += 1 while space?(char)
      following = @nodes[index + 1]
      flexible = node[:modifiers].include?('FM') || node[:modifiers].any? { |mod| %w[TH th].include?(mod) } || !following || (following[:key] ? !following[:key]['is_digit'] : !/\A\d\z/.match?(following[:literal]))
      if flexible
        match = /\A\s*[+-]?\d+/.match(@input[initial..])
        invalid('datetime field must be an integer') unless match
        @position = initial + match[0].length
      else
        part = @input[@position, width]
        invalid('source string too short for datetime field') if part.length < width
        match = /\A\s*[+-]?\d+/.match(part)
        invalid('datetime field must be an integer') unless match && match[0].length == width
        @position += width
      end
      value = match[0].to_i
      raise Error.new('22008', 'datetime field value out of range') unless value.between?(-2_147_483_648, 2_147_483_647)
      set(field, value) if field
      @position - initial
    end
    def word(names)
      match = names.each_with_index.find { |name, _| @input[@position, name.length].tr('A-Z', 'a-z') == name.tr('A-Z', 'a-z') }
      invalid('invalid datetime word') unless match
      @position += match[0].length
      match[1]
    end
    def read
      fixed, extra = false, 0
      @nodes.each_with_index do |node, index|
        break if @position >= @input.length
        key = node[:key]
        if !fixed && (!key || key['id'] != 'FX') && (key || index.zero?)
          while space?(char); @position += 1; extra += 1 end
        end
        if node[:separator] || node[:space]
          if fixed
            @position += 1 if char
          else
            extra -= 1
            if space?(char) || separator?(char)
              @position += 1
              extra += 1
            end
          end
          next
        elsif !key
          !fixed && extra > 0 ? extra -= 1 : @position += 1
          next
        end
        mode = key['date_mode']
        if mode != 'none'
          invalid('invalid combination of date conventions') if @mode && @mode != mode
          @mode = mode
        end
        id = key['id']
        width = key['name'].length
        integer = true
        case id
        when 'FX' then fixed = true; integer = false
        when 'AM','PM','am','pm','A_M','P_M','a_m','p_m'
          set(:pm, word(FormattingTables::WORDS.fetch(id.include?('_') ? 'ampm_long' : 'ampm')) % 2)
          @fields[:clock] = 12
          integer = false
        when 'HH','HH12' then read_integer(:hour, 2, index); @fields[:clock] = 12
        when 'HH24' then read_integer(:hour, 2, index)
        when 'MI' then read_integer(:minute, width, index)
        when 'SS' then read_integer(:second, width, index)
        when 'SSSS' then read_integer(:seconds, width, index)
        when 'MS'
          size = read_integer(:millis, 3, index)
          @fields[:millis] *= 10**(3 - size) if size < 3
        when 'US','FF1','FF2','FF3','FF4','FF5','FF6'
          digits = id == 'US' ? 6 : id[-1].to_i
          @fields[:precision] = digits unless id == 'US'
          size = read_integer(:micros, digits, index)
          @fields[:micros] *= 10**(6 - size) if size < 6
        when 'TZ','tz','TZH','OF'
          raise Error.new('0A000', 'timezone abbreviations in input are not supported') if %w[TZ tz].include?(id) && char && /[A-Za-z]/.match?(char)
          if %w[+ -].include?(char) || char == ' '
            @fields[:tzsign] = char == '-' ? -1 : 1
            @position += 1
          else
            @fields[:tzsign] = extra > 0 && @position > 0 && @input[@position - 1] == '-' ? -1 : 1
          end
          read_integer(:zone_hour, 2, index)
          if id != 'TZH' && char == ':'
            @position += 1
            read_integer(:zone_minute, 2, index)
          end
          integer = false
        when 'TZM'
          @fields[:tzsign] = 1 if @fields[:tzsign].zero?
          read_integer(:zone_minute, 2, index)
          integer = false
        when 'AD','BC','ad','bc','A_D','B_C','a_d','b_c'
          set(:bc, word(FormattingTables::WORDS.fetch(id.include?('_') ? 'adbc_long' : 'adbc')) % 2)
          integer = false
        when 'MONTH','Month','month','MON','Mon','mon'
          set(:month, word(FormattingTables::WORDS.fetch(id.length > 3 ? 'months_full' : 'months_abbrev')) + 1)
          integer = false
        when 'MM' then read_integer(:month, width, index)
        when 'DAY','Day','day','DY','Dy','dy'
          set(:weekday, word(FormattingTables::WORDS.fetch(id.length == 3 ? 'days_full' : 'days_abbrev')))
          @fields[:weekday] += 1
          integer = false
        when 'DDD','IDDD' then read_integer(:year_day, 3, index)
        when 'DD' then read_integer(:day, width, index)
        when 'D','ID'
          read_integer(:weekday, 1, index)
          if id == 'ID'
            @fields[:weekday] += 1
            @fields[:weekday] = 1 if @fields[:weekday] > 7
          end
        when 'WW','IW' then read_integer(:week, width, index)
        when 'Q' then read_integer(nil, width, index)
        when 'CC' then read_integer(:century, width, index)
        when 'Y_YYY'
          match = /\A\s*([+-]?\d+),\s*([+-]?\d{1,3})/.match(@input[@position..])
          invalid('invalid year') unless match
          value = match[1].to_i * 1000 + match[2].to_i
          raise Error.new('22008', 'year out of range') unless value.between?(-2_147_483_648, 2_147_483_647)
          set(:year, value)
          @fields[:year_size] = 4
          @position += match[0].length
        when 'YYYY','IYYY'
          read_integer(:year, width, index)
          @fields[:year_size] = 4
        when 'YYY','YY','Y','IYY','IY','I'
          size = read_integer(:year, width, index)
          if size < 4
            y = @fields[:year]
            @fields[:year] += y < 70 ? 2000 : y < 100 ? 1900 : y < 520 ? 2000 : y < 1000 ? 1000 : 0
          end
          @fields[:year_size] = width
        when 'RM','rm'
          set(:month, 12 - word(FormattingTables::WORDS.fetch('rm_months_lower')))
          integer = false
        when 'W' then read_integer(:month_week, width, index)
        when 'J' then read_integer(:julian, width, index)
        else integer = false
        end
        @position += [2, @input.length - @position].min if integer && node[:modifiers].any? { |mod| %w[TH th].include?(mod) }
        unless fixed
          extra = 0
          while space?(char); @position += 1; extra += 1 end
        end
      end
      assemble
    end
    def assemble
      f = @fields
      hour = DateFormatting.trunc(f[:seconds], 3600)
      minute = DateFormatting.trunc(DateFormatting.remainder(f[:seconds], 3600), 60)
      second = DateFormatting.remainder(f[:seconds], 60)
      hour = f[:hour] unless f[:hour].zero?
      minute = f[:minute] unless f[:minute].zero?
      second = f[:second] unless f[:second].zero?
      if f[:clock] == 12
        invalid('hour is invalid for the 12-hour clock') unless hour.between?(1, 12)
        hour += 12 if f[:pm] != 0 && hour < 12
        hour = 0 if f[:pm] == 0 && hour == 12
      end
      year, month, day = 0, 1, 1
      unless f[:year].zero?
        if f[:century] != 0 && f[:year_size] <= 2
          century = f[:bc] != 0 ? -f[:century] : f[:century]
          year = DateFormatting.remainder(f[:year], 100)
          year = year.zero? ? century * 100 + (century < 0 ? 1 : 0) : century >= 0 ? year + (century - 1) * 100 : (century + 1) * 100 - year + 1
        else
          year = f[:bc] != 0 ? -f[:year] : f[:year]
          year += 1 if year.negative?
        end
      else
        unless f[:century].zero?
          century = f[:bc] != 0 ? -f[:century] : f[:century]
          year = (century >= 0 ? century - 1 : century) * 100 + 1
        end
      end
      unless f[:julian].zero?
        year, month, day = DateValue.new(f[:julian] - 2_440_588).parts
      end
      unless f[:week].zero?
        if @mode == 'isoweek'
          julian = DateFormatting.iso_start(year) + (f[:week] - 1) * 7
          julian += f[:weekday] > 1 ? f[:weekday] - 2 : 6 unless f[:weekday].zero?
          year, month, day = DateValue.new(julian - 2_440_588).parts
        else
          f[:year_day] = (f[:week] - 1) * 7 + 1
        end
      end
      f[:day] = (f[:month_week] - 1) * 7 + 1 unless f[:month_week].zero?
      day = f[:day] unless f[:day].zero?
      month = f[:month] unless f[:month].zero?
      if f[:year_day] != 0 && (month <= 1 || day <= 1)
        invalid('cannot calculate day of year without year') if year.zero? && f[:bc].zero?
        if @mode == 'isoweek'
          year, month, day = DateValue.new(DateFormatting.iso_start(year) + f[:year_day] - 1 - 2_440_588).parts
        else
          date = DateValue.from_parts(year, 1, 1) + f[:year_day] - 1
          y, m, d = date.parts
          raise Error.new('22008', 'day of year out of range') unless year == y
          month = m if month <= 1
          day = d if day <= 1
        end
      end
      micros = f[:millis] * 1000 + f[:micros]
      unless hour.between?(0, 23) && minute.between?(0, 59) && second.between?(0, 59) && micros.between?(0, 999_999)
        raise Error.new('22008', 'date/time field value out of range')
      end
      if f[:year].zero? && f[:century].zero? && f[:julian].zero?
        raise Error.new('22008', 'date field value out of range') unless day.between?(1, 31)
        date = DateValue.from_parts(year, month, 1) + day - 1
      else
        date = DateValue.from_parts(year, month, day)
      end
      if f[:tzsign] != 0 && (!f[:zone_hour].between?(0, 15) || !f[:zone_minute].between?(0, 59))
        raise Error.new('22009', 'timezone displacement out of range')
      end
      zone = f[:tzsign].zero? ? nil : f[:tzsign] * (f[:zone_hour] * 60 + f[:zone_minute]) * 60
      [date, (hour * 3600 + minute * 60 + second) * 1_000_000 + micros, zone, f[:precision]]
    end
  end

  module DateFormatting
    def self.from_text(input, template, date: false, context: nil)
      day, time, zone, precision = DateTemplateReader.new(input, template).read
      if date
        raise Error.new('22008', 'date is out of range') unless day.days.between?(-2_147_483_647, 2_147_483_646)
        return day
      end
      micros = day.days * TimestampValue::DAY + time
      if zone
        micros -= zone * 1_000_000
      else
        context&.charge
        micros = context.local_to_instant(micros) if context&.respond_to?(:local_to_instant)
      end
      if precision > 0 && precision < 6
        epoch = 946_684_800_000_000
        factor = 10**(6 - precision)
        relative = micros - epoch
        rounded = (relative.abs + factor / 2) / factor * factor
        micros = epoch + (relative.negative? ? -rounded : rounded)
      end
      raise Error.new('22008', 'timestamp is out of range') unless micros.between?(-(2**63) + 1, 2**63 - 2)
      TimestampValue.new(micros, zoned: true)
    end
  end
end
