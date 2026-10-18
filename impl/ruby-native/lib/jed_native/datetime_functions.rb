# frozen_string_literal: true

require_relative 'datetime_units'

module JedNative
  module DateTimeFunctions
    def datetime_unit(name, interval: false, truncate: false)
      token = name.tr('A-Z', 'a-z').byteslice(0, DateTimeUnits::TOKEN_MAX_LEN)
      entry = DateTimeUnits::DELTA[token]
      entry ||= DateTimeUnits::DATE[token] unless truncate
      if entry && entry[0] == 'units'
        return entry[1]
      elsif entry && entry[0] == 'reserv' && !truncate
        return 'epoch' if entry[1] == 'epoch'
        raise Error.new('0A000', 'datetime unit is not supported') unless interval
      end
      raise Error.new('22023', "unrecognized datetime unit #{name}")
    end

    def extract(name, value, float: false)
      unit = datetime_unit(name, interval: value.is_a?(IntervalValue))
      trunc = DateFormatting.method(:trunc)
      rem = DateFormatting.method(:remainder)
      if value.is_a?(IntervalValue)
        year, month = trunc.call(value.months, 12), rem.call(value.months, 12)
        seconds = rem.call(value.micros, 60_000_000)
        result = case unit
        when 'microseconds' then seconds
        when 'milliseconds' then Decimal.new(seconds, 3)
        when 'second' then Decimal.new(seconds, 6)
        when 'minute' then rem.call(trunc.call(value.micros, 60_000_000), 60)
        when 'hour' then trunc.call(value.micros, 3_600_000_000)
        when 'day' then value.days
        when 'week' then trunc.call(value.days, 7)
        when 'month' then month
        when 'quarter' then trunc.call(month, 3) + (value.months.negative? ? -1 : 1)
        when 'year' then year
        when 'decade' then trunc.call(year, 10)
        when 'century' then trunc.call(year, 100)
        when 'millennium' then trunc.call(year, 1000)
        when 'epoch'
          # Symbolic years use 365.25 days; a residual month uses 30 days.
          seconds = year * 31_557_600 + month * 2_592_000 + value.days * 86_400
          Decimal.new(seconds * 1_000_000 + value.micros, 6)
        else raise Error.new('0A000', "unit #{name} is not supported for interval")
        end
      else
        is_date = value.is_a?(DateValue)
        micros = is_date ? value.days * TimestampValue::DAY : value.micros
        time_units = %w[microseconds milliseconds second minute hour timezone timezone_hour timezone_minute]
        if is_date && !float && time_units.include?(unit)
          raise Error.new('0A000', "unit #{name} is not supported for date")
        end
        if micros.is_a?(Float)
          if %w[year decade century millennium isoyear epoch julian].include?(unit)
            raise Error.new('22003', 'decimal cannot represent infinity') unless float
            return micros
          end
          return nil
        end
        raise Error.new('0A000', "unit #{name} needs a time zone") if unit.start_with?('timezone')
        days, time = micros.divmod(TimestampValue::DAY)
        year, month, day = DateValue.new(days).parts
        result = case unit
        when 'microseconds' then time % 60_000_000
        when 'milliseconds' then Decimal.new(time % 60_000_000, 3)
        when 'second' then Decimal.new(time % 60_000_000, 6)
        when 'minute' then time.div(60_000_000) % 60
        when 'hour' then time.div(3_600_000_000)
        when 'day' then day
        when 'week' then DateFormatting.iso_week(year, month, day)
        when 'month' then month
        when 'quarter' then (month - 1).div(3) + 1
        when 'year' then year <= 0 ? year - 1 : year
        when 'decade' then year.div(10)
        when 'century' then year > 0 ? (year - 1).div(100) + 1 : -((1 - year).div(100) + 1)
        when 'millennium' then year > 0 ? (year - 1).div(1000) + 1 : -((1 - year).div(1000) + 1)
        when 'isoyear'
          iso = DateFormatting.iso_year(year, month, day)
          iso <= 0 ? iso - 1 : iso
        when 'dow' then (days + 4) % 7
        when 'isodow' then (days + 3) % 7 + 1
        when 'doy' then days - DateValue.from_parts(year, 1, 1).days + 1
        when 'epoch' then is_date && !float ? days * 86_400 : Decimal.new(micros, 6)
        when 'julian'
          julian = days + 2_440_588
          if float
            return julian.to_f + time.to_f / TimestampValue::DAY
          end
          is_date ? Decimal.new(julian) : Decimal.new(julian) + Decimal.new(time) / Decimal.new(TimestampValue::DAY)
        else raise Error.new('0A000', "unit #{name} is not supported")
        end
      end
      float ? result.to_f : Decimal.parse(result)
    end

    def date_trunc(name, value)
      unit = datetime_unit(name, interval: value.is_a?(IntervalValue), truncate: true)
      factors = { 'hour' => 3_600_000_000, 'minute' => 60_000_000, 'second' => 1_000_000, 'milliseconds' => 1000, 'microseconds' => 1 }
      if value.is_a?(IntervalValue)
        months, days, micros = value.months, value.days, value.micros
        if (factor = factors[unit])
          micros = DateFormatting.trunc(micros, factor) * factor
        elsif unit == 'day'
          micros = 0
        elsif (factor = { 'month' => 1, 'quarter' => 3, 'year' => 12, 'decade' => 120, 'century' => 1200, 'millennium' => 12000 }[unit])
          months = DateFormatting.trunc(months, factor) * factor
          days = micros = 0
        else
          raise Error.new('0A000', "unit #{name} is not supported for interval")
        end
        return IntervalValue.new(months, days, micros)
      end
      raise Error.new('0A000', "unit #{name} is not supported for timestamp") unless factors.key?(unit) || %w[day week month quarter year decade century millennium].include?(unit)
      return value if value.micros.is_a?(Float)
      days, time = value.micros.divmod(TimestampValue::DAY)
      year, month, day = DateValue.new(days).parts
      if (factor = factors[unit])
        time = time.div(factor) * factor
      elsif unit == 'week'
        days -= (days + 3) % 7
        return TimestampValue.new(days * TimestampValue::DAY, zoned: value.zoned)
      else
        time = 0
        day = 1 unless unit == 'day'
        month = (month - 1).div(3) * 3 + 1 if unit == 'quarter'
        if %w[year decade century millennium].include?(unit)
          month = 1
          year = year.div(10) * 10 if unit == 'decade'
          if %w[century millennium].include?(unit)
            size = unit == 'century' ? 100 : 1000
            year = year > 0 ? (year - 1).div(size) * size + 1 : -((1 - year).div(size) * size) + 1
          end
        end
      end
      TimestampValue.new(TimeZoneData.finite(DateValue.from_parts(year, month, day).days * TimestampValue::DAY + time), zoned: value.zoned)
    end

    def call(name, args, context: nil, arg_types: nil)
      if name == 'make_timestamp'
        resolve_signature(name, arg_types || args.map { |arg| Types.infer(arg) })
        return nil if args.any?(&:nil?)
        year, month, day, hour, minute, seconds = args
        raise Error.new('22008', 'date/time field out of range') unless year != 0 && hour.between?(0, 24) && minute.between?(0, 59) && seconds.finite? && seconds.between?(0, 60) && (hour != 24 || minute.zero? && seconds.zero?)
        year += 1 if year < 0
        date = DateValue.from_parts(year, month, day)
        micros = (seconds * 1_000_000).round(half: :even)
        return TimestampValue.new(TimeZoneData.finite(date.days * TimestampValue::DAY + (hour * 3600 + minute * 60) * 1_000_000 + micros))
      elsif name == 'age' && context
        return nil if args.any?(&:nil?)
        raise Error.new('22008', 'cannot subtract infinite timestamps') if args.any? { |value| value.micros.is_a?(Float) }
        values = args
        if args.length == 1
          context.charge
          today = context.resolve_timezone.instant_to_local(context.clock_value('now').micros).div(TimestampValue::DAY) * TimestampValue::DAY
          if args.first.zoned
            context.charge
            today = context.local_to_instant(today)
          end
          values = [TimestampValue.new(today, zoned: args.first.zoned), args.first]
        end
        negative = values[0] < values[1]
        values = values.reverse if negative
        values = values.map do |value|
          next value unless value.zoned
          context.charge
          TimestampValue.new(context.resolve_timezone.instant_to_local(value.micros))
        end
        result = age(*values, ordered: true)
        return negative ? -result : result
      end
      super
    end
  end
  Functions.singleton_class.prepend(DateTimeFunctions)

  module DateTimeArguments
    def compile(node, columns)
      if node[:kind] == :call && %w[make_timestamp make_timestamptz].include?(node[:name])
        args = node[:args].each_with_index.map do |arg, index|
          if index == 5 && arg[:kind] == :literal && (arg[:untyped] || arg[:value].nil?)
            { kind: :literal, value: arg[:value]&.to_f, type: 'f64' }
          else arg
          end
        end
        return super(node.merge(args: args), columns)
      end
      super
    end
  end
  Engine.prepend(DateTimeArguments)

  module DateTimeBinding
    def function(node, scope, wanted)
      if %w[make_timestamp make_timestamptz].include?(node[:name])
        args = node[:args].each_with_index.map do |arg, index|
          index == 5 && arg[:kind] == :literal && (arg[:untyped] || arg[:value].nil?) ? { kind: :literal, value: arg[:value]&.to_f, type: 'f64' } : arg
        end
        return super(node.merge(args: args), scope, wanted)
      end
      super
    end
  end
  ParameterBindings.prepend(DateTimeBinding)
end
