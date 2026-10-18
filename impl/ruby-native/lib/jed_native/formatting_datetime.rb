# frozen_string_literal: true
require_relative 'formatting_tables'

module JedNative
  module DateFormatting
    module_function
    def template(text)
      nodes, i = [], 0
      while i < text.length
        modifiers = []
        prefix = FormattingTables::SUFFIXES.find { |entry| entry['type'] == 'prefix' && text[i, entry['name'].length] == entry['name'] }
        if prefix
          modifiers << prefix['id']
          i += prefix['name'].length
        end
        break if i == text.length
        key = FormattingTables::DCH.find { |entry| text[i, entry['name'].length] == entry['name'] }
        if key
          i += key['name'].length
          suffix = FormattingTables::SUFFIXES.find { |entry| entry['type'] == 'postfix' && text[i, entry['name'].length] == entry['name'] }
          if suffix
            modifiers << suffix['id']
            i += suffix['name'].length
          end
          nodes << { key: key, modifiers: modifiers }
        elsif text[i] == '"'
          i += 1
          while i < text.length && text[i] != '"'
            i += 1 if text[i] == '\\' && i + 1 < text.length
            nodes << { literal: text[i] }
            i += 1
          end
          i += 1 if text[i] == '"'
        else
          i += 1 if text[i, 2] == '\\"'
          char = text[i]
          nodes << { literal: char, separator: /[\x20-\x2f\x3a-\x40\x5b-\x60\x7b-\x7e]/.match?(char), space: " \t\r\n\v\f".include?(char) }
          i += 1
        end
      end
      nodes
    end
    def trunc(a, b) = a.negative? ? -(-a / b) : a / b
    def remainder(a, b) = a - trunc(a, b) * b
    def int32(value) = ((value + 2**31) & 0xffffffff) - 2**31
    def julian(year, month, day)
      month, year = month > 2 ? [month + 1, year + 4800] : [month + 13, year + 4799]
      century = trunc(year, 100)
      int32(int32(year * 365) - 32167 + trunc(year, 4) - century + trunc(century, 4) + trunc(int32(7834 * month), 256) + day)
    end
    def weekday(julian) = int32(julian + 1) % 7
    def iso_start(year)
      fourth = julian(year, 1, 4)
      int32(fourth - weekday(fourth - 1))
    end
    def iso_year(year, month, day)
      date = julian(year, month, day)
      return year - 1 if date < iso_start(year)
      date >= iso_start(year + 1) ? year + 1 : year
    end
    def iso_week(year, month, day) = trunc(julian(year, month, day) - iso_start(iso_year(year, month, day)), 7) + 1
    def padded(value, width)
      text = value.to_s
      value.negative? ? '-' + text[1..].rjust([width - 1, 0].max, '0') : text.rjust(width, '0')
    end
    def ordinal(text, upper)
      n = text.to_i.abs
      suffix = n % 100 >= 11 && n % 100 <= 13 ? 'th' : { 1=>'st', 2=>'nd', 3=>'rd' }.fetch(n % 10, 'th')
      upper ? suffix.upcase : suffix
    end
    def decompose(value, context)
      if value.is_a?(IntervalValue)
        year = trunc(value.months, 12)
        month = remainder(value.months, 12)
        seconds = trunc(value.micros, 1_000_000)
        [year, month, value.days, trunc(seconds, 3600), trunc(remainder(seconds, 3600), 60), remainder(seconds, 60), remainder(value.micros, 1_000_000), 0, value.months * 30 + value.days, 0, nil]
      else
        micros, offset, zone = value.micros, 0, value.zoned ? 'UTC' : nil
        if value.zoned
          context&.charge
          if context&.respond_to?(:timezone_at)
            offset, zone = context.timezone_at(micros)
            micros += offset * 1_000_000
          end
        end
        days, rest = micros.divmod(TimestampValue::DAY)
        seconds, fraction = rest.divmod(1_000_000)
        year, month, day = DateValue.new(days).parts
        [year, month, day, seconds / 3600, seconds / 60 % 60, seconds % 60, fraction, (days + 4) % 7, days - DateValue.from_parts(year, 1, 1).days + 1, offset, zone]
      end
    end
    def to_char(value, text, context: nil)
      return nil if text.empty? || value.is_a?(TimestampValue) && value.micros.is_a?(Float)
      interval = value.is_a?(IntervalValue)
      year, month, day, hour, minute, second, micro, week_day, year_day, offset, zone = decompose(value, context)
      nodes = template(text)
      nodes.map do |node|
        next node[:literal] unless node[:key]
        key = node[:key]['id']
        fill = node[:modifiers].include?('FM')
        translate = node[:modifiers].include?('TM')
        width = ->(n, value = 0) { fill ? 0 : n + (value.negative? ? 1 : 0) }
        number = nil
        result = case key
        when 'HH', 'HH12'
          h = remainder(hour, 12); h = 12 if h.zero?
          number = padded(h, width.call(2, hour))
        when 'HH24' then number = padded(hour, width.call(2, hour))
        when 'MI' then number = padded(minute, width.call(2, minute))
        when 'SS' then number = padded(second, width.call(2, second))
        when 'FF1', 'FF2', 'FF3', 'FF4', 'FF5', 'FF6', 'MS', 'US'
          digits = key == 'MS' ? 3 : key == 'US' ? 6 : key[-1].to_i
          number = padded(trunc(micro, 10**(6 - digits)), digits)
        when 'SSSS' then number = (hour * 3600 + minute * 60 + second).to_s
        when 'AM','PM','am','pm','A_M','P_M','a_m','p_m'
          value = remainder(hour, 24) >= 12 ? 'PM' : 'AM'
          value = value.chars.join('.') + '.' if key.include?('_')
          key == key.downcase ? value.downcase : value
        when 'TZ','tz','TZH','TZM','OF'
          raise Error.new('22007', 'invalid format specification for an interval value') if interval
          case key
          when 'TZ' then zone || ''
          when 'tz' then (zone || '').downcase
          when 'TZH' then (offset.negative? ? '-' : '+') + padded(offset.abs / 3600, 2)
          when 'TZM' then padded(offset.abs % 3600 / 60, 2)
          else
            (offset.negative? ? '-' : '+') + padded(offset.abs / 3600, fill ? 0 : 2) + (offset % 3600 == 0 ? '' : ':' + padded(offset.abs % 3600 / 60, 2))
          end
        when 'AD','BC','ad','bc','A_D','B_C','a_d','b_c'
          raise Error.new('22007', 'invalid format specification for an interval value') if interval
          value = year <= 0 ? 'BC' : 'AD'
          value = value.chars.join('.') + '.' if key.include?('_')
          key == key.downcase ? value.downcase : value
        when 'MONTH','Month','month','MON','Mon','mon','DAY','Day','day','DY','Dy','dy'
          raise Error.new('22007', 'invalid format specification for an interval value') if interval
          words, index = if key.downcase.start_with?('mon')
            [key.length > 3 ? 'months_full' : 'months_abbrev', month - 1]
          else
            [key.downcase == 'day' ? 'days_full' : 'days_abbrev', week_day]
          end
          value = FormattingTables::WORDS.fetch(words)[index]
          value = value.upcase if key == key.upcase
          value = value.downcase if key == key.downcase
          %w[months_full days_full].include?(words) && !fill && !translate ? value.ljust(9) : value
        when 'MM' then number = padded(month, width.call(2, month))
        when 'DDD' then number = padded(year_day, width.call(3))
        when 'IDDD' then number = padded(julian(year, month, day) - iso_start(iso_year(year, month, day)) + 1, width.call(3))
        when 'DD' then number = padded(day, width.call(2))
        when 'D','ID'
          raise Error.new('22007', 'invalid format specification for an interval value') if interval
          number = (key == 'D' ? week_day + 1 : week_day.zero? ? 7 : week_day).to_s
        when 'WW' then number = padded(trunc(year_day - 1, 7) + 1, width.call(2))
        when 'IW' then number = padded(iso_week(year, month, day), width.call(2))
        when 'Q' then month.zero? ? '' : (number = (trunc(month - 1, 3) + 1).to_s)
        when 'CC'
          century = interval ? trunc(year, 100) : year.positive? ? trunc(year - 1, 100) + 1 : trunc(year, 100) - 1
          number = padded(century, century.between?(-99, 99) ? width.call(2, century) : 0)
        when 'Y_YYY'
          y = !interval && year <= 0 ? 1 - year : year
          k = trunc(y, 1000)
          number = k.to_s + ',' + padded(y - k * 1000, 3)
        when 'YYYY','YYY','YY','Y','IYYY','IYY','IY','I'
          y = key.start_with?('I') ? iso_year(year, month, day) : year
          y = 1 - y if !interval && y <= 0
          digits = key == 'I' ? 1 : key.start_with?('I') ? key.length : key.length
          y = remainder(y, 10**digits) if digits < 4
          number = padded(y, digits == 1 ? 1 : width.call(digits, y))
        when 'RM','rm'
          if month.zero? && year.zero? then ''
          else
            index = month.zero? ? year >= 0 ? 0 : 11 : month.negative? ? -(month + 1) : 12 - month
            value = FormattingTables::WORDS.fetch(key == 'RM' ? 'rm_months_upper' : 'rm_months_lower')[index]
            fill ? value : value.ljust(4)
          end
        when 'W' then number = (trunc(day - 1, 7) + 1).to_s
        when 'J' then number = julian(year, month, day).to_s
        else ''
        end
        if number && (node[:modifiers].include?('TH') || node[:modifiers].include?('th'))
          result = number + ordinal(number, node[:modifiers].include?('TH'))
        end
        result
      end.join
    end
  end

  module DateFormattingExpressions
    def compile(node, columns)
      if node[:kind] == :call && node[:name] == 'to_char' && node[:args].length == 2
        value = compile(node[:args][0], columns)
        raise Error.new('42725', 'to_char is ambiguous') if value.kind == :literal && value.type == 'text'
        if %w[timestamp timestamptz interval].include?(value.type)
          return Expression.new(kind: :datetime_format, type: 'text', args: [value, compile(node[:args][1], columns)])
        end
      end
      super
    end
    def evaluate(expression, row, group = nil)
      return super unless expression.kind == :datetime_format
      charge
      value, template = expression.args.map { |arg| evaluate(arg, row, group) }
      return nil if value.nil? || template.nil?
      result = DateFormatting.to_char(value, template, context: self)
      charge(result.bytesize) if result && !(respond_to?(:constant_expression?) && constant_expression?(expression))
      result
    end
  end
  Engine.prepend(DateFormattingExpressions)
end
