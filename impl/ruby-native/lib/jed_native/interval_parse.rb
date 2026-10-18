# frozen_string_literal: true

module JedNative
  module IntervalParser
    UNITS = {
      'millennium' => :millennium, 'millennia' => :millennium, 'mil' => :millennium,
      'century' => :century, 'centuries' => :century, 'cent' => :century,
      'decade' => :decade, 'decades' => :decade, 'dec' => :decade,
      'year' => :year, 'years' => :year, 'y' => :year, 'yr' => :year, 'yrs' => :year,
      'month' => :month, 'months' => :month, 'mon' => :month, 'mons' => :month,
      'week' => :week, 'weeks' => :week, 'w' => :week,
      'day' => :day, 'days' => :day, 'd' => :day,
      'hour' => :hour, 'hours' => :hour, 'h' => :hour, 'hr' => :hour, 'hrs' => :hour,
      'minute' => :minute, 'minutes' => :minute, 'm' => :minute, 'min' => :minute, 'mins' => :minute, 'mm' => :minute,
      'second' => :second, 'seconds' => :second, 's' => :second, 'sec' => :second, 'secs' => :second,
      'millisecond' => :millisecond, 'milliseconds' => :millisecond, 'ms' => :millisecond, 'msecond' => :millisecond, 'mseconds' => :millisecond,
      'microsecond' => :microsecond, 'microseconds' => :microsecond, 'us' => :microsecond, 'usecond' => :microsecond, 'useconds' => :microsecond
    }.freeze
    NUMBER = '[+-]?(?:\d+(?:\.\d*)?|\.\d+)'
    module_function
    def parse(input)
      return input if input.is_a?(IntervalValue)
      text = input.to_s.strip
      raise Error.new('22007', 'invalid input syntax for interval') if text.empty?
      if text.start_with?('P')
        raise Error.new('22007', 'invalid ISO interval') unless text == input.to_s
        return iso(text)
      end
      text = text.delete_prefix('@').strip.sub(/\s+at\z/, '').gsub(/([+-])\s+(?=\d|\.)/, '\\1')
      ago = text.sub!(/\s+ago\z/i, '')
      tokens = []
      until text.empty?
        text = text.sub(/\A[\s,]+/, '')
        break if text.empty?
        if (m = /\A([+-]?)(\d+)-(\d+)(?=\s|\z)/.match(text))
          raise Error.new('22008', 'interval field value out of range') if m[3].to_i > 11
          sign = m[1] == '-' ? -1 : 1
          tokens << [:year_month, Decimal.new(sign * (m[2].to_i * 12 + m[3].to_i))]
        elsif (m = /\A([+-]?)(\d+):(\d+(?:\.\d*)?)(?::(\d+(?:\.\d*)?))?/.match(text))
          sign = m[1] == '-' ? -1 : 1
          second_mode = m[4].nil? && m[3].include?('.')
          h, minute, second = if second_mode then [0, m[2].to_i, Decimal.parse(m[3])] else [m[2].to_i, m[3].to_i, Decimal.parse(m[4] || '0')] end
          raise Error.new('22008', 'interval field value out of range') if minute > 59 || second > 60
          tokens << [:time, Decimal.new(sign) * (Decimal.new(h * 3600 + minute * 60) + second)]
          text = text.sub(/(?<=\d)\s+hours?\b/, '')
        elsif (m = /\A(#{NUMBER})(?:\s*([a-zA-Z]+))?/.match(text))
          unit = m[2] && UNITS[m[2].downcase]
          raise Error.new('22007', 'invalid interval unit') if m[2] && unit.nil?
          tokens << [unit, Decimal.parse(m[1])]
        else raise Error.new('22007', 'invalid input syntax for interval')
        end
        text = text[m[0].length..]
      end
      raise Error.new('22007', 'invalid input syntax for interval') if tokens.empty?
      tokens.each_with_index do |token, index|
        next if token[0]
        following = tokens[index + 1]&.first
        token[0] = if [:time, :hour].include?(following) then :day elsif following then following else :second end
      end
      accumulate(tokens, ago ? -1 : 1)
    end
    def accumulate(tokens, sign = 1)
      months = days = micros = 0
      seen = {}
      tokens.reverse_each do |unit, value|
        fields = case unit
                 when :time then [:hour, :minute, :second, :millisecond, :microsecond]
                 when :year_month then [:year, :month]
                 when :second then value == value.trunc ? [:second] : [:second, :millisecond, :microsecond]
                 else [unit]
                 end
        case unit
        when :millennium, :century, :decade, :year
          multiplier = {millennium: 12_000, century: 1200, decade: 120, year: 12}.fetch(unit)
          months += (value * multiplier).round.coefficient
        when :year_month then months += value.to_i
        when :month
          whole = value.to_i
          months += whole
          fraction = (value - whole) * 30
          days += fraction.to_i
          micros += ((fraction - fraction.to_i) * TimestampValue::DAY).round.coefficient
        when :day, :week
          value *= 7 if unit == :week
          days += value.to_i
          micros += ((value - value.to_i) * TimestampValue::DAY).round.coefficient
        else
          multiplier = {hour: 3_600_000_000, minute: 60_000_000, second: 1_000_000, time: 1_000_000, millisecond: 1000, microsecond: 1}[unit]
          raise Error.new('22007', 'invalid input syntax for interval') unless multiplier
          micros = 0 if unit == :time
          micros += (value * multiplier).round.coefficient
        end
        validate(months, days, micros)
        raise Error.new('22007', 'duplicate interval field') if fields.any? { |f| seen[f] }
        fields.each { |f| seen[f] = true }
      end
      IntervalValue.new(months * sign, days * sign, micros * sign)
    end
    def validate(months, days, micros)
      raise Error.new('22008', 'interval field value out of range') unless months.between?(-2_147_483_648, 2_147_483_647) && days.between?(-2_147_483_648, 2_147_483_647) && micros.between?(-9_223_372_036_854_775_808, 9_223_372_036_854_775_807)
    end
    def iso(text)
      body = text[1..]
      return parse(body[1..]) if /\AT\d+:/.match?(body)
      if (m = /\A(\d{4})(?:-(\d{2}))?(?:-(\d{2}))?(?:T(\d{2})(?::(\d{2}))?(?::(\d{2}(?:\.\d+)?))?)?\z/.match(body))
        return accumulate([[:year, Decimal.new(m[1].to_i)], [:month, Decimal.new(m[2].to_i)], [:day, Decimal.new(m[3].to_i)], [:hour, Decimal.new(m[4].to_i)], [:minute, Decimal.new(m[5].to_i)], [:second, Decimal.parse(m[6] || '0')]])
      elsif (m = /\A(\d{4})(\d{2})(\d{2})(?:T(\d{2})(\d{2})(\d{2}(?:\.\d+)?))?\z/.match(body))
        return accumulate([[:year, Decimal.new(m[1].to_i)], [:month, Decimal.new(m[2].to_i)], [:day, Decimal.new(m[3].to_i)], [:hour, Decimal.new(m[4].to_i)], [:minute, Decimal.new(m[5].to_i)], [:second, Decimal.parse(m[6] || '0')]])
      end
      tokens, time = [], false
      until body.empty?
        if body.start_with?('T')
          raise Error.new('22007', 'invalid ISO interval') if time
          time = true
          body = body[1..]
          next
        end
        m = /\A(#{NUMBER}(?:[eE][+-]?\d+)?)([YMWDHS])/.match(body)
        raise Error.new('22007', 'invalid ISO interval') unless m
        unit = time ? {'H' => :hour, 'M' => :minute, 'S' => :second}[m[2]] : {'Y' => :year, 'M' => :month, 'W' => :week, 'D' => :day}[m[2]]
        raise Error.new('22007', 'invalid ISO interval') unless unit
        tokens << [unit, Decimal.parse(m[1])]
        body = body[m[0].length..]
      end
      raise Error.new('22007', 'invalid ISO interval') if tokens.empty? && !time
      accumulate(tokens)
    end
  end
end
