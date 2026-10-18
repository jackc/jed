# frozen_string_literal: true

require_relative 'float_format'
require_relative 'interval_parse'

module JedNative
  # Exact decimal: the coefficient uses Ruby's built-in integer representation.
  # Scale belongs to the value, so equal numbers can retain distinct displays.
  class Decimal < Numeric
    include Comparable
    attr_reader :coefficient, :scale
    MAX_SCALE = 16_383
    MAX_DIGITS = 131_072

    def initialize(coefficient, scale = 0, check: true)
      @coefficient, @scale = coefficient, scale
      if scale.negative?
        @coefficient *= 10**-scale
        @scale = 0
      end
      if check && (@scale > MAX_SCALE || @coefficient.abs.to_s.length - @scale > MAX_DIGITS)
        raise Error.new('22003', 'numeric value out of range')
      end
      freeze
    end

    def self.parse(value)
      return value if value.is_a?(self)
      return new(value) if value.is_a?(Integer)
      text = value.to_s.strip
      return new(Types.parse_integer_text(text)) if /\A[+-]?0[xXoObB]/.match?(text)
      text = text.gsub(/(?<=\d)_(?=\d)/, '')
      match = /\A\s*([+-]?)(?:(\d+)(?:\.(\d*))?|\.(\d+))(?:[eE]([+-]?\d+))?\s*\z/.match(text)
      raise Error.new('22P02', 'invalid input syntax for decimal') unless match
      fraction = match[3] || match[4] || ''
      digits = (match[2] || '0') + fraction
      exponent = (match[5] || '0').to_i
      raise Error.new('22003', 'numeric value out of range') if exponent.abs > MAX_DIGITS + MAX_SCALE
      coefficient = digits.to_i
      coefficient = -coefficient if match[1] == '-'
      new(coefficient, fraction.length - exponent)
    end

    def to_s
      sign = @coefficient.negative? ? '-' : ''
      digits = @coefficient.abs.to_s
      return sign + digits if @scale.zero?
      digits = digits.rjust(@scale + 1, '0')
      "#{sign}#{digits[0...-@scale]}.#{digits[-@scale..]}"
    end
    def inspect = "Decimal(#{to_s})"
    def to_r = Rational(@coefficient, 10**@scale)
    def to_f = to_s.to_f
    def to_i = @coefficient.negative? ? -(-@coefficient / 10**@scale) : @coefficient / 10**@scale
    def zero? = @coefficient.zero?
    def negative? = @coefficient.negative?
    def positive? = @coefficient.positive?
    def abs = negative? ? self.class.new(-@coefficient, @scale) : self
    def -@ = self.class.new(-@coefficient, @scale)
    def +@ = self
    def coerce(other) = [self.class.parse(other), self]
    def <=>(other)
      return nil unless other.is_a?(Decimal) || other.is_a?(Integer)
      other = self.class.parse(other)
      s = [@scale, other.scale].max
      (@coefficient * 10**(s - @scale)) <=> (other.coefficient * 10**(s - other.scale))
    end
    def +(other)
      other = self.class.parse(other)
      s = [@scale, other.scale].max
      self.class.new(@coefficient * 10**(s - @scale) + other.coefficient * 10**(s - other.scale), s)
    end
    def -(other) = self + -self.class.parse(other)
    def *(other)
      other = self.class.parse(other)
      value = self.class.new(@coefficient * other.coefficient, @scale + other.scale, check: false)
      value.round([value.scale, MAX_SCALE].min)
    end
    def division_scale(other)
      a_weight, a_first = leading_group
      b_weight, b_first = other.leading_group
      weight = a_weight - b_weight - (a_first <= b_first ? 1 : 0)
      [[16 - 4 * weight, @scale, other.scale, 0].max, 1000].min
    end
    def leading_group
      return [0, 0] if zero?
      weight = (@coefficient.abs.to_s.length - 1 - @scale).div(4)
      shift = 4 * weight + @scale
      first = shift.negative? ? @coefficient.abs * 10**-shift : @coefficient.abs / 10**shift
      [weight, first]
    end
    def /(other)
      other = self.class.parse(other)
      divide(other, division_scale(other))
    end
    def divide(other, scale)
      other = self.class.parse(other)
      raise Error.new('22012', 'division by zero') if other.zero?
      shift = scale + other.scale - @scale
      numerator, denominator = @coefficient.abs, other.coefficient.abs
      if shift >= 0 then numerator *= 10**shift else denominator *= 10**-shift end
      q, r = numerator.divmod(denominator)
      q += 1 if r * 2 >= denominator
      q = -q if negative? != other.negative?
      self.class.new(q, scale)
    end
    def %(other)
      other = self.class.parse(other)
      raise Error.new('22012', 'division by zero') if other.zero?
      s = [@scale, other.scale].max
      a = @coefficient * 10**(s - @scale)
      b = other.coefficient * 10**(s - other.scale)
      self.class.new(a.remainder(b), s)
    end
    def round(scale = 0)
      scale = [[scale, MAX_SCALE].min, -MAX_DIGITS].max
      shift = @scale - scale
      return self.class.new(@coefficient * 10**-shift, scale) if shift <= 0
      divisor = 10**shift
      q, r = @coefficient.abs.divmod(divisor)
      q += 1 if 2 * r >= divisor
      self.class.new(negative? ? -q : q, scale)
    end
    def trunc(scale = 0)
      scale = [[scale, MAX_SCALE].min, -MAX_DIGITS].max
      shift = @scale - scale
      return self.class.new(@coefficient * 10**-shift, scale) if shift <= 0
      q = @coefficient.abs / 10**shift
      self.class.new(negative? ? -q : q, scale)
    end
    def floor = self.class.new(@coefficient.div(10**@scale))
    def ceil = self.class.new(-(-@coefficient).div(10**@scale))
    def trim_scale
      n, s = @coefficient, @scale
      while s.positive? && (n % 10).zero?
        n /= 10
        s -= 1
      end
      self.class.new(n, s)
    end
    def eql?(other) = other.is_a?(Decimal) && self == other
    def hash = to_r.hash
  end

  class Bytea < String; end
  class UUID < String; end

  # Proleptic Gregorian dates, with days counted from the Unix civil epoch.
  # The algorithms use only integer arithmetic, including dates before 1 AD.
  class DateValue
    include Comparable
    attr_reader :days
    def initialize(days)
      raise Error.new('22008', 'date out of range') unless days.is_a?(Float) || days.between?(-2_147_483_647, 2_147_483_646)
      @days = days
      freeze
    end
    def self.from_parts(year, month, day)
      raise Error.new('22008', 'date field value out of range') unless (1..12).cover?(month)
      leap = (year % 4).zero? && (!(year % 100).zero? || (year % 400).zero?)
      count = [31, leap ? 29 : 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][month - 1]
      raise Error.new('22008', 'date field value out of range') unless (1..count).cover?(day)
      year -= 1 if month <= 2
      era = year.div(400)
      yoe = year - era * 400
      doy = (153 * (month + (month > 2 ? -3 : 9)) + 2).div(5) + day - 1
      new(era * 146_097 + yoe * 365 + yoe.div(4) - yoe.div(100) + doy - 719_468)
    end
    def self.parse(text)
      return text if text.is_a?(self)
      return new(0) if text.to_s.strip.downcase == 'epoch'
      return new(Float::INFINITY) if text == 'infinity'
      return new(-Float::INFINITY) if text == '-infinity'
      source = text.to_s.strip
      return new(Float::INFINITY) if %w[infinity +infinity].include?(source.downcase)
      return new(-Float::INFINITY) if source.downcase == '-infinity'
      m = /\A(?:(BC|AD)\s+)?(\d+)-(\d{1,2})-(\d{1,2})(?:[ T](\d{1,2}):(\d{2})(?::(\d{2})(\.\d+)?)?)?\s*(Z|[+-]\d{1,2}(?::\d{2}(?::\d{2})?)?)?(?:\s+(BC|AD))?\z/i.match(source)
      raise Error.new('22007', 'invalid input syntax for date') unless m
      year = m[2].to_i
      raise Error.new('22008', 'year zero is not supported') if year.zero?
      year = 1 - year if (m[1] || m[10])&.upcase == 'BC'
      hour, minute, second = m[5].to_i, m[6].to_i, m[7].to_i
      fraction = m[8] && Decimal.parse(m[8])
      if hour > 24 || minute > 59 || second > 59 || hour == 24 && (minute != 0 || second != 0 || fraction && !fraction.zero?)
        raise Error.new('22008', 'date time field out of range')
      end
      if m[9] && m[9].upcase != 'Z'
        hours, minutes, seconds = m[9][1..].split(':').map(&:to_i)
        raise Error.new('22008', 'date offset out of range') if hours > 15 || (minutes || 0) > 59 || (seconds || 0) > 59
      end
      from_parts(year, m[3].to_i, m[4].to_i)
    end
    def parts
      z = @days + 719_468
      era = z.div(146_097)
      doe = z - era * 146_097
      yoe = (doe - doe.div(1460) + doe.div(36_524) - doe.div(146_096)).div(365)
      year = yoe + era * 400
      doy = doe - (365 * yoe + yoe.div(4) - yoe.div(100))
      mp = (5 * doy + 2).div(153)
      day = doy - (153 * mp + 2).div(5) + 1
      month = mp + (mp < 10 ? 3 : -9)
      year += 1 if month <= 2
      [year, month, day]
    end
    def to_s
      return @days.positive? ? 'infinity' : '-infinity' if @days.is_a?(Float)
      y, m, d = parts
      format('%04d-%02d-%02d%s', y <= 0 ? 1 - y : y, m, d, y <= 0 ? ' BC' : '')
    end
    def <=>(other) = other.is_a?(DateValue) ? @days <=> other.days : nil
    def +(days) = self.class.new(@days + days)
    def -(other) = other.is_a?(DateValue) ? @days - other.days : self.class.new(@days - other)
    def eql?(other) = other.is_a?(DateValue) && @days == other.days
    def hash = @days.hash
  end

  class TimestampValue
    include Comparable
    DAY = 86_400_000_000
    attr_reader :micros, :zoned
    def initialize(micros, zoned: false) = (@micros = micros; @zoned = zoned; freeze)
    def self.parse(text, zoned: false)
      return new(text.micros, zoned: zoned) if text.is_a?(self)
      return new(text.days * DAY, zoned: zoned) if text.is_a?(DateValue)
      special = text.to_s.strip.downcase
      return new(special == '-infinity' ? -Float::INFINITY : Float::INFINITY, zoned: zoned) if %w[infinity +infinity -infinity].include?(special)
      m = /\A\s*(\d+-\d+-\d+)(?:[T ](\d{1,2}):(\d{2})(?::(\d{2})(\.\d+)?)?)?\s*(Z|[+-]\d{1,2}(?::?\d{2})?|UTC)?\s*(BC|AD)?\s*\z/i.match(text.to_s)
      raise Error.new('22007', 'invalid input syntax for timestamp') unless m
      date = DateValue.parse(m[1] + (m[7] ? ' ' + m[7] : ''))
      hour, minute, second = m[2].to_i, m[3].to_i, m[4].to_i
      raise Error.new('22008', 'timestamp field value out of range') if hour > 24 || minute > 59 || second > 59 || hour == 24 && (minute != 0 || second != 0 || m[5] && !Decimal.parse(m[5]).zero?)
      fraction = m[5] ? Decimal.parse(m[5]).round(6).coefficient : 0
      value = date.days * DAY + (hour * 3600 + minute * 60 + second) * 1_000_000 + fraction
      if m[6] && (offset = /\A([+-])(\d{1,2})(?::?(\d{2}))?\z/.match(m[6]))
        raise Error.new('22008', 'timestamp offset out of range') if offset[2].to_i > 15 || offset[3].to_i > 59
        seconds = (offset[2].to_i * 60 + offset[3].to_i) * 60
        value -= seconds * 1_000_000 * (offset[1] == '-' ? -1 : 1) if zoned
      end
      raise Error.new('22008', 'timestamp out of range') unless value.between?(-(1 << 63) + 1, (1 << 63) - 2)
      new(value, zoned: zoned)
    end
    def to_s
      return @micros.positive? ? 'infinity' : '-infinity' if @micros.is_a?(Float)
      days, time = @micros.divmod(DAY)
      seconds, micro = time.divmod(1_000_000)
      hour, rest = seconds.divmod(3600)
      minute, second = rest.divmod(60)
      fraction = micro.zero? ? '' : '.' + micro.to_s.rjust(6, '0').sub(/0+\z/, '')
      date = DateValue.new(days).to_s
      era = date.end_with?(' BC') ? ' BC' : ''
      date = date.delete_suffix(' BC')
      "#{date} #{format('%02d:%02d:%02d', hour, minute, second)}#{fraction}#{@zoned ? '+00' : ''}#{era}"
    end
    def <=>(other) = other.is_a?(TimestampValue) ? @micros <=> other.micros : nil
    def eql?(other) = other.is_a?(TimestampValue) && @micros == other.micros
    def hash = @micros.hash
  end

  class IntervalValue
    include Comparable
    attr_reader :months, :days, :micros
    def initialize(months = 0, days = 0, micros = 0)
      IntervalParser.validate(months, days, micros)
      @months, @days, @micros = months, days, micros
      freeze
    end
    def self.parse(text) = IntervalParser.parse(text)
    def span = (@months * 30 + @days) * TimestampValue::DAY + @micros
    def <=>(other) = other.is_a?(IntervalValue) ? span <=> other.span : nil
    def +(other) = self.class.new(@months + other.months, @days + other.days, @micros + other.micros)
    def -@ = self.class.new(-@months, -@days, -@micros)
    def -(other) = self + -other
    def *(factor)
      factor = Decimal.parse(factor).to_r
      months = @months * factor
      days = @days * factor + (months - months.to_i) * 30
      micros = @micros * factor + (days - days.to_i) * TimestampValue::DAY
      rounded = (micros.abs + Rational(1, 2)).floor * (micros.negative? ? -1 : 1)
      self.class.new(months.to_i, days.to_i, rounded)
    end
    def to_s
      pieces = []
      years = @months.negative? ? -(-@months / 12) : @months / 12
      months = @months - years * 12
      pieces << "#{years} #{years == 1 ? 'year' : 'years'}" unless years.zero?
      pieces << "#{months.positive? && years.negative? ? '+' : ''}#{months} #{months == 1 ? 'mon' : 'mons'}" unless months.zero?
      pieces << "#{@days.positive? && @months.negative? ? '+' : ''}#{@days} #{@days == 1 ? 'day' : 'days'}" unless @days.zero?
      if !@micros.zero? || pieces.empty?
        seconds, fraction = @micros.abs.divmod(1_000_000)
        hours, rest = seconds.divmod(3600)
        minute, second = rest.divmod(60)
        frac = fraction.zero? ? '' : '.' + fraction.to_s.rjust(6, '0').sub(/0+\z/, '')
        pieces << "#{@micros.negative? ? '-' : (@days.negative? || @days.zero? && @months.negative?) ? '+' : ''}#{format('%02d:%02d:%02d', hours, minute, second)}#{frac}"
      end
      pieces.join(' ')
    end
    def eql?(other) = other.is_a?(IntervalValue) && span == other.span
    def hash = span.hash
  end

  class ArrayValue < Array
    attr_accessor :element_type, :dimensions, :lower_bounds
    def initialize(values = [], element_type: nil, lower_bound: 1, dimensions: nil, lower_bounds: nil)
      if dimensions.nil? && values.any? { |v| v.is_a?(ArrayValue) }
        raise Error.new('42804', 'cannot mix array and scalar elements') unless values.all? { |v| v.is_a?(ArrayValue) }
        shape = values.first.dimensions
        raise Error.new('2202E', 'multidimensional arrays must have matching dimensions') unless values.all? { |v| v.dimensions == shape && v.lower_bounds == values.first.lower_bounds }
        dimensions = [values.length] + shape
        lower_bounds ||= [lower_bound] + values.first.lower_bounds
        values = values.flat_map(&:to_a)
        element_type = element_type&.sub(/(?:\[\])+\z/, '')
      end
      super(values)
      @element_type = element_type
      @dimensions = dimensions || (empty? ? [] : [length])
      @lower_bounds = lower_bounds || [lower_bound] * @dimensions.length
      raise Error.new('54000', 'number of array dimensions exceeds six') if @dimensions.length > 6
    end
    def lower_bound = @lower_bounds.first || 1
    def lower_bound=(value)
      @lower_bounds[0] = value
    end
    def to_s
      prefix = @lower_bounds.any? { |v| v != 1 } ? @dimensions.each_with_index.map { |d, i| "[#{@lower_bounds[i]}:#{@lower_bounds[i] + d - 1}]" }.join + '=' : ''
      offset = 0
      render = lambda do |depth|
        count = @dimensions[depth] || 0
        values = Array.new(count) do
          if depth + 1 < @dimensions.length
            render.call(depth + 1)
          else
            value = self[offset]
            offset += 1
            text = value == true ? 't' : value == false ? 'f' : Types.format(value, @element_type)
            value.nil? ? 'NULL' : Types.container_quote(text, /[{},"\\\s]|\ANULL\z/i)
          end
        end
        '{' + values.join(',') + '}'
      end
      prefix + render.call(0)
    end
  end
  
  class CompositeValue < Array
    attr_accessor :type_name, :field_names
    def initialize(values = [], type_name: 'record', field_names: nil)
      super(values)
      @type_name, @field_names = type_name, field_names
    end
    def to_s = '(' + map { |v| v.nil? ? '' : Types.record_quote(v == true ? 't' : v == false ? 'f' : Types.format(v)) }.join(',') + ')'
  end

  class RangeValue
    include Comparable
    attr_reader :lower, :upper, :lower_inc, :upper_inc, :element_type, :empty
    def initialize(lower, upper, bounds = '[)', element_type: 'i32', empty: false)
      raise Error.new('22000', 'invalid range bound flags') unless %w[[] [) (] ()].include?(bounds)
      @element_type = element_type
      @lower = lower.nil? ? nil : Types.cast(lower, element_type)
      @upper = upper.nil? ? nil : Types.cast(upper, element_type)
      @lower_inc = !@lower.nil? && bounds[0] == '['
      @upper_inc = !@upper.nil? && bounds[1] == ']'
      @empty = empty
      if !@lower.nil? && !@upper.nil?
        comparison = Types.compare(@lower, @upper)
        raise Error.new('22000', 'range lower bound must be less than or equal to upper bound') if comparison.positive?
        @empty ||= comparison.zero? && !(@lower_inc && @upper_inc)
      end
      if %w[i32 i64 date].include?(element_type) && !@empty
        @lower += 1 if @lower && !@lower_inc
        @upper += 1 if @upper && @upper_inc
        Types.check_integer(@lower, element_type) if @lower && Types::INTEGER_BITS.key?(element_type)
        Types.check_integer(@upper, element_type) if @upper && Types::INTEGER_BITS.key?(element_type)
        @lower_inc = !@lower.nil?
        @upper_inc = false
        @empty = !@lower.nil? && !@upper.nil? && Types.compare(@lower, @upper) >= 0
      end
      if @empty then @lower = @upper = nil; @lower_inc = @upper_inc = false end
      freeze
    end
    def self.parse(text, element_type: 'i32')
      return text if text.is_a?(self) && text.element_type == element_type
      return new(nil, nil, element_type: element_type, empty: true) if text == 'empty'
      raise Error.new('22P02', 'malformed range literal') unless text.is_a?(String) && ['[', '('].include?(text[0]) && [']', ')'].include?(text[-1])
      parts = Types.split_container(text[1...-1])
      raise Error.new('22P02', 'malformed range literal') unless parts.length == 2
      new(parts[0], parts[1], text[0] + text[-1], element_type: element_type)
    end
    def to_s
      return 'empty' if @empty
      lo = @lower.nil? ? '' : Types.container_quote(Types.format(@lower), /[,()\[\]"\\\s]/)
      hi = @upper.nil? ? '' : Types.container_quote(Types.format(@upper), /[,()\[\]"\\\s]/)
      "#{@lower_inc ? '[' : '('}#{lo},#{hi}#{@upper_inc ? ']' : ')'}"
    end
    def <=>(other)
      return nil unless other.is_a?(RangeValue)
      return @empty ? (other.empty ? 0 : -1) : 1 if @empty || other.empty
      cmp = if @lower.nil? then other.lower.nil? ? 0 : -1 elsif other.lower.nil? then 1 else Types.compare(@lower, other.lower) end
      return cmp unless cmp.zero?
      return @lower_inc ? -1 : 1 if @lower_inc != other.lower_inc
      cmp = if @upper.nil? then other.upper.nil? ? 0 : 1 elsif other.upper.nil? then -1 else Types.compare(@upper, other.upper) end
      return cmp unless cmp.zero?
      @upper_inc == other.upper_inc ? 0 : (@upper_inc ? 1 : -1)
    end
    def contains?(value)
      return true if value.is_a?(RangeValue) && value.empty
      return false if @empty
      return value.empty || (contains_bound?(value.lower, value.lower_inc, true) && contains_bound?(value.upper, value.upper_inc, false)) if value.is_a?(RangeValue)
      (@lower.nil? || Types.compare(value, @lower).positive? || (@lower_inc && Types.compare(value, @lower).zero?)) &&
        (@upper.nil? || Types.compare(value, @upper).negative? || (@upper_inc && Types.compare(value, @upper).zero?))
    end
    def contains_bound?(value, inclusive, lower)
      own, own_inc = lower ? [@lower, @lower_inc] : [@upper, @upper_inc]
      return true if own.nil?
      return false if value.nil?
      cmp = Types.compare(value, own) * (lower ? 1 : -1)
      cmp.positive? || (cmp.zero? && (!inclusive || own_inc))
    end
    def strictly_left?(other)
      return false if @empty || other.empty || @upper.nil? || other.lower.nil?
      cmp = Types.compare(@upper, other.lower)
      cmp.negative? || (cmp.zero? && !(@upper_inc && other.lower_inc))
    end
    def overlaps?(other) = !@empty && !other.empty && !strictly_left?(other) && !other.strictly_left?(self)
    def adjacent?(other)
      return false if @empty || other.empty
      (!@upper.nil? && !other.lower.nil? && Types.compare(@upper, other.lower).zero? && @upper_inc != other.lower_inc) ||
        (!@lower.nil? && !other.upper.nil? && Types.compare(@lower, other.upper).zero? && @lower_inc != other.upper_inc)
    end
    def eql?(other) = other.is_a?(RangeValue) && self == other
    def hash = [@lower, @upper, @lower_inc, @upper_inc, @empty].hash
  end

  module Types
    ALIASES = {
      'smallint' => 'i16', 'int2' => 'i16', 'integer' => 'i32', 'int' => 'i32', 'int4' => 'i32',
      'bigint' => 'i64', 'int8' => 'i64', 'bool' => 'boolean', 'varchar' => 'text', 'string' => 'text',
      'character varying' => 'text', 'numeric' => 'decimal', 'dec' => 'decimal', 'real' => 'f32',
      'float4' => 'f32', 'float8' => 'f64', 'float' => 'f64', 'double precision' => 'f64',
      'timestamp without time zone' => 'timestamp', 'timestamp with time zone' => 'timestamptz',
      'int4range' => 'i32range', 'int8range' => 'i64range'
    }.freeze
    INTEGER_BITS = {'i16' => 16, 'i32' => 32, 'i64' => 64}.freeze
    RANGES = {'i32range' => 'i32', 'i64range' => 'i64', 'numrange' => 'decimal', 'tsrange' => 'timestamp', 'tstzrange' => 'timestamptz', 'daterange' => 'date'}.freeze
    module_function
    def normalize(type)
      text = type.to_s.strip.downcase.gsub(/\s+/, ' ')
      return 'unknown' if text == 'null'
      return normalize(text.sub(/(?:\[\d*\])+\z/, '')) + '[]' if /\[\d*\]\z/.match?(text)
      match = /\A([^()]+)(\(.*\))?\z/.match(text)
      return text unless match
      base = ALIASES.fetch(match[1].strip, match[1].strip)
      base + (match[2] || '').delete(' ')
    end
    def base(type) = normalize(type).sub(/\(.*\)\z/, '')
    def infer(value)
      case value
      when nil then 'unknown'
      when true, false then 'boolean'
      when Integer then 'i64'
      when Float then 'f64'
      when Decimal then 'decimal'
      when Bytea then 'bytea'
      when UUID then 'uuid'
      when String then 'text'
      when DateValue then 'date'
      when TimestampValue then value.zoned ? 'timestamptz' : 'timestamp'
      when IntervalValue then 'interval'
      when CompositeValue then value.type_name
      when ArrayValue then (value.element_type || infer(value.find { |v| !v.nil? })) + '[]'
      when Array then infer(value.find { |v| !v.nil? }) + '[]'
      when RangeValue then RANGES.key(value.element_type)
      else value.respond_to?(:sql_type) ? value.sql_type : 'unknown'
      end
    end
    def cast(value, type, source_type: nil)
      return nil if value.nil?
      type = normalize(type)
      if type.end_with?('[]')
        elem = type[0...-2]
  if value.is_a?(String)
    return parse_array(value, elem)
  end
  raise Error.new('42804', 'cannot cast value to array') unless value.is_a?(Array)
  if value.any? { |v| v.is_a?(ArrayValue) }
    value = ArrayValue.new(value, element_type: elem)
  end
  return ArrayValue.new(value.map { |v| cast(v, elem) }, element_type: elem,
                        dimensions: value.respond_to?(:dimensions) ? value.dimensions : nil,
                        lower_bounds: value.respond_to?(:lower_bounds) ? value.lower_bounds : nil)
end
      return RangeValue.parse(value, element_type: RANGES[type]) if RANGES.key?(type)
      case base(type)
      when 'i16', 'i32', 'i64'
        if value.is_a?(Decimal) then number = value.round.coefficient
        elsif value.is_a?(Float)
          raise Error.new('22003', 'integer out of range') unless value.finite?
          number = value.round(half: :up)
        elsif value == true then number = 1
        elsif value == false then number = 0
        elsif value.is_a?(Integer) then number = value
        else
          prefix = /\A[+-]?[0-9](?:[0-9_]*[0-9])?/.match(value.to_s.strip)
          if prefix && prefix[0].delete('_').to_i.abs > (1 << (INTEGER_BITS[type] - 1))
            raise Error.new('22003', 'integer out of range')
          end
          number = parse_integer_text(value.to_s)
        end
        check_integer(number, type)
      when 'decimal'
        number = Decimal.parse(value)
        if (mod = /\((\d+)(?:,(\d+))?\)/.match(type))
          precision, scale = mod[1].to_i, mod[2].to_i
          raise Error.new('22023', 'invalid numeric precision or scale') unless (1..1000).cover?(precision) && scale <= precision
          number = number.round(scale)
          raise Error.new('22003', 'numeric field overflow') if number.coefficient.abs >= 10**precision
        end
        number
      when 'f32', 'f64'
        number = if value.is_a?(Numeric) then value.to_f else
          text = value.to_s.strip
          case text.downcase
          when 'nan' then Float::NAN
          when 'infinity', '+infinity', 'inf', '+inf' then Float::INFINITY
          when '-infinity', '-inf' then -Float::INFINITY
          else Float(text, exception: false) || (raise Error.new('22P02', 'invalid input syntax for float'))
          end
        end
        if number.infinite? && !value.is_a?(Float) && !/\A[+-]?(?:inf|infinity)\z/i.match?(value.to_s.strip)
          raise Error.new('22003', 'float out of range')
        end
        narrowed = if type == 'f32' && number.finite? && number.abs > 3.4028234663852886e38 && number.abs < (2**128 - 2**103)
          number.negative? ? -3.4028234663852886e38 : 3.4028234663852886e38
        else
          type == 'f32' ? [number].pack('f').unpack1('f') : number
        end
        raise Error.new('22003', 'float out of range') if narrowed.infinite? && number.finite?
        narrowed
      when 'boolean'
        return value if value == true || value == false
        return !value.zero? if value.is_a?(Integer)
        text = value.to_s.strip.downcase
        return true if %w[true t tr tru yes y ye on 1].include?(text)
        return false if %w[false f fa fal fals no n off of 0].include?(text)
        raise Error.new('22P02', 'invalid input syntax for boolean')
      when 'text'
        format(value, source_type)
      when 'bytea'
        return value if value.is_a?(Bytea)
        text = value.to_s
        if text.start_with?('\\x')
          body = text[2..]
          raise Error.new('22P02', 'invalid hexadecimal bytea') unless /\A(?:[ \t\n\r]*[0-9a-fA-F]{2})*[ \t\n\r]*\z/.match?(body)
          hex = body.delete(" \t\n\r")
          raise Error.new('22P02', 'invalid hexadecimal bytea') unless hex.length.even? && /\A[0-9a-f]*\z/i.match?(hex)
          Bytea.new([hex].pack('H*'))
        else
          bytes = +''.b
          offset = 0
          while offset < text.bytesize
            byte = text.getbyte(offset)
            offset += 1
            if byte == 92
              if text.getbyte(offset) == 92
                bytes << 92
                offset += 1
              elsif (octal = text.byteslice(offset, 3)) && /\A[0-3][0-7]{2}\z/.match?(octal)
                bytes << octal.to_i(8)
                offset += 3
              else
                raise Error.new('22P02', 'invalid bytea escape')
              end
            else
              bytes << byte
            end
          end
          Bytea.new(bytes)
        end
      when 'uuid'
        return value if value.is_a?(UUID)
        text = value.to_s
        if text.start_with?('{')
          raise Error.new('22P02', 'invalid input syntax for uuid') unless text.end_with?('}')
          text = text[1...-1]
        end
        raise Error.new('22P02', 'invalid input syntax for uuid') unless /\A[0-9a-f]{4}(?:[0-9a-f]{4}|-[0-9a-f]{4})*\z/i.match?(text)
        hex = text.delete('-')
        raise Error.new('22P02', 'invalid input syntax for uuid') unless /\A[0-9a-f]{32}\z/i.match?(hex)
        UUID.new([hex[0, 8], hex[8, 4], hex[12, 4], hex[16, 4], hex[20, 12]].join('-').downcase)
      when 'date'
        value.is_a?(TimestampValue) ? DateValue.new(value.micros.div(TimestampValue::DAY)) : DateValue.parse(value)
      when 'timestamp', 'timestamptz' then TimestampValue.parse(value, zoned: type == 'timestamptz')
      when 'interval'
        raise Error.new('42804', 'cannot cast this value to interval') unless value.is_a?(String) || value.is_a?(IntervalValue)
        IntervalValue.parse(value)
      when 'json', 'jsonb' then JsonValue.parse(value, binary: type == 'jsonb')
      when 'jsonpath' then JsonPathValue.parse(value)
      when 'record' then value.is_a?(CompositeValue) ? value : CompositeValue.new(value)
      else
        return value if value.is_a?(CompositeValue) && value.type_name == type
        raise Error.new('42704', "type #{type} does not exist in experimental Ruby core")
      end
    end
    def check_integer(value, type = 'i64')
      bits = INTEGER_BITS.fetch(base(type), 64)
      raise Error.new('22003', "#{type} out of range") if value < -(1 << (bits - 1)) || value >= (1 << (bits - 1))
      value
    end
    def parse_integer_text(value)
      text = value.strip
      sign = text.start_with?('-') ? -1 : 1
      text = text[1..] if text.start_with?('+', '-')
      radix = { 'x' => [16, /\A_?[0-9a-fA-F](?:_?[0-9a-fA-F])*\z/], 'o' => [8, /\A_?[0-7](?:_?[0-7])*\z/], 'b' => [2, /\A_?[01](?:_?[01])*\z/] }
      if text.start_with?('0') && (rule = radix[text[1]&.downcase])
        base, pattern = rule
        text = text[2..]
      else
        base, pattern = 10, /\A[0-9](?:_?[0-9])*\z/
      end
      raise Error.new('22P02', 'invalid input syntax for integer') unless pattern.match?(text)
      sign * Integer(text.delete('_'), base)
    end
    def promote(left, right)
      left, right = normalize(left), normalize(right)
      return right if left == 'unknown'
      return left if right == 'unknown'
      return left if left == right
      if INTEGER_BITS.key?(left) && INTEGER_BITS.key?(right)
        return INTEGER_BITS[left] >= INTEGER_BITS[right] ? left : right
      end
      return 'decimal' if (left == 'decimal' && INTEGER_BITS.key?(right)) || (right == 'decimal' && INTEGER_BITS.key?(left))
      return 'f64' if %w[f32 f64].include?(left) && %w[f32 f64].include?(right)
      raise Error.new('42804', "types #{left} and #{right} cannot be matched")
    end
    def compare(a, b)
      return 0 if a.nil? && b.nil?
      return 1 if a.nil?
      return -1 if b.nil?
      if a.is_a?(Float) || b.is_a?(Float)
        return a.is_a?(Float) && a.nan? ? (b.is_a?(Float) && b.nan? ? 0 : 1) : -1 if (a.is_a?(Float) && a.nan?) || (b.is_a?(Float) && b.nan?)
      end
      return (a ? 1 : 0) <=> (b ? 1 : 0) if (a == true || a == false) && (b == true || b == false)
      if a.is_a?(Array) && b.is_a?(Array)
        [a.length, b.length].min.times do |i|
          cmp = compare(a[i], b[i])
          return cmp unless cmp.zero?
        end
        cmp = a.length <=> b.length
        return cmp unless cmp.zero?
        if a.is_a?(ArrayValue) && b.is_a?(ArrayValue)
          cmp = a.dimensions.length <=> b.dimensions.length
          return cmp unless cmp.zero?
          cmp = a.dimensions <=> b.dimensions
          return cmp unless cmp.zero?
          return a.lower_bounds <=> b.lower_bounds
        end
        return 0
      end
      comparison = a <=> b
      raise Error.new('42804', "cannot compare #{infer(a)} and #{infer(b)}") if comparison.nil?
      comparison
    end
    def unary(op, value, type: nil)
      return nil if value.nil?
      case op.to_s.downcase
      when 'not' then return !value
      when '-', 'neg' then result = -value
      when '+' then return value
      when '~' then result = ~value
      else raise Error.new('0A000', "unsupported unary operator #{op}")
      end
      result.is_a?(Integer) ? check_integer(result, type || 'i64') : result
    end
    def binary(op, a, b, type: nil)
      op = op.to_s.downcase
      return false if op == 'and' && (a == false || b == false)
      return true if op == 'or' && (a == true || b == true)
      return compare(a, b) != 0 if ['is distinct from', 'is_distinct_from'].include?(op)
      return compare(a, b) == 0 if ['is not distinct from', 'is_not_distinct_from'].include?(op)
      return nil if a.nil? || b.nil?
      case op
      when 'and' then return a && b
      when 'or' then return a || b
      when '=', 'eq' then return compare(a, b).zero?
      when '<>', '!=', 'ne' then return !compare(a, b).zero?
      when '<', 'lt' then return compare(a, b).negative?
      when '>', 'gt' then return compare(a, b).positive?
      when '<=', 'le' then return compare(a, b) <= 0
      when '>=', 'ge' then return compare(a, b) >= 0
      when '||'
        return ArrayValue.new(a + b, element_type: a.element_type) if a.is_a?(ArrayValue) && b.is_a?(ArrayValue)
        return ArrayValue.new(a.to_a + [b], element_type: a.element_type) if a.is_a?(ArrayValue)
        return ArrayValue.new([a] + b.to_a, element_type: b.element_type) if b.is_a?(ArrayValue)
        return a.concat_json(b) if a.respond_to?(:concat_json)
        return a.to_s + b.to_s
      when '@>' then return a.respond_to?(:contains?) ? a.contains?(b) : b.all? { |v| !v.nil? && a.include?(v) }
      when '<@' then return binary('@>', b, a)
      when '&&' then return a.is_a?(RangeValue) ? a.overlaps?(b) : a.any? { |v| !v.nil? && b.include?(v) }
      when '-|-' then return a.adjacent?(b)
      when '<<' then return a.strictly_left?(b) if a.is_a?(RangeValue)
      when '>>' then return b.strictly_left?(a) if a.is_a?(RangeValue)
      when '&<', '&>'
        return false if a.empty || b.empty
        if op == '&<'
          return true if b.upper.nil?
          return false if a.upper.nil?
          comparison = compare(a.upper, b.upper)
          return comparison.negative? || comparison.zero? && (!a.upper_inc || b.upper_inc)
        else
          return true if b.lower.nil?
          return false if a.lower.nil?
          comparison = compare(a.lower, b.lower)
          return comparison.positive? || comparison.zero? && (!a.lower_inc || b.lower_inc)
        end
      when 'like', 'ilike', 'not like', 'not ilike'
        result = Functions.like(a, b, insensitive: op.include?('ilike'))
        return op.start_with?('not ') ? !result : result
      when '~', '~*', '!~', '!~*'
        result = Functions.regex(b, op.end_with?('*') ? 'i' : '').match?(a)
        return op.start_with?('!') ? !result : result
      when '->', '->>', '#>', '#>>', '?', '?|', '?&' then return a.operator(op, b)
      end
      if a.is_a?(TimestampValue) || a.is_a?(DateValue) || a.is_a?(IntervalValue) || b.is_a?(IntervalValue) || b.is_a?(DateValue) || b.is_a?(TimestampValue)
        return datetime_binary(op, a, b)
      end
      if a.is_a?(RangeValue) && %w[+ - *].include?(op)
        return Functions.range_set(op, a, b)
      end
      if a.is_a?(Decimal) || b.is_a?(Decimal)
        a, b = Decimal.parse(a), Decimal.parse(b)
      end
      case op
      when '+', 'add' then result = a + b
      when '-', 'sub' then result = a - b
      when '*', 'mul' then result = a * b
      when '/', 'div'
        raise Error.new('22012', 'division by zero') if b.zero? && !(a.is_a?(Float) && a.nan?)
        result = a.is_a?(Integer) && b.is_a?(Integer) ? (a.abs / b.abs) * (a.negative? != b.negative? ? -1 : 1) : a / b
      when '%', 'mod'
        raise Error.new('22012', 'division by zero') if b.zero?
        result = a.is_a?(Integer) ? a.remainder(b) : a % b
      when '^' then result = a**b
      when '&' then result = a & b
      when '|' then result = a | b
      when '#' then result = a ^ b
      when '<<' then result = a << b
      when '>>' then result = a >> b
      else raise Error.new('0A000', "unsupported binary operator #{op}")
      end
  if result.is_a?(Float)
    result = [result].pack('f').unpack1('f') if type == 'f32'
    raise Error.new('22003', 'float out of range') if result.infinite? && a.respond_to?(:finite?) && b.respond_to?(:finite?) && a.finite? && b.finite?
  end
  result.is_a?(Integer) ? check_integer(result, type || 'i64') : result
end
    def datetime_binary(op, a, b)
      return a + b if a.is_a?(DateValue) && b.is_a?(Integer) && op == '+'
      return a - b if a.is_a?(DateValue) && (b.is_a?(Integer) || b.is_a?(DateValue)) && op == '-'
      return IntervalValue.new(0, 0, a.micros - b.micros) if a.is_a?(TimestampValue) && b.is_a?(TimestampValue) && op == '-'
      if b.is_a?(IntervalValue) && (a.is_a?(TimestampValue) || a.is_a?(DateValue))
        b = -b if op == '-'
        micros = a.is_a?(DateValue) ? a.days * TimestampValue::DAY : a.micros
        day, time = micros.divmod(TimestampValue::DAY)
        y, m, d = DateValue.new(day).parts
        month = y * 12 + m - 1 + b.months
        y, m = month.divmod(12)
        m += 1
        begin
          date = DateValue.from_parts(y, m, d)
        rescue Error
          d -= 1
          retry
        end
        return TimestampValue.new((date.days + b.days) * TimestampValue::DAY + time + b.micros, zoned: a.is_a?(TimestampValue) && a.zoned)
      end
      return a + b if a.is_a?(IntervalValue) && b.is_a?(IntervalValue) && op == '+'
      return a - b if a.is_a?(IntervalValue) && b.is_a?(IntervalValue) && op == '-'
      raise Error.new('42883', "unsupported date/time operator #{op}")
    end
    def format(value, type = nil)
      return 'NULL' if value.nil?
      return cast(value, type).to_s if type && %w[date timestamp timestamptz interval].include?(normalize(type)) && value.is_a?(String)
      case value
      when true then 'true'
      when false then 'false'
      when Bytea then '\\x' + value.unpack1('H*')
      when Float then FloatFormat.render(value, type == 'f32' ? 32 : 64)
      when ArrayValue, CompositeValue then value.to_s
      when Array then ArrayValue.new(value).to_s
      else value.to_s
      end
    end
    def container_quote(text, pattern)
      return text unless text.empty? || pattern.match?(text)
      '"' + text.gsub(/["\\]/) { |char| '\\' + char } + '"'
    end
    def record_quote(text)
      return text unless text.empty? || /[(),"\\\s]/.match?(text)
      '"' + text.gsub(/["\\]/) { |char| char + char } + '"'
    end
    def split_container(text, null_word: false)
      values, current, quoted, was_quoted, escaped = [], +'', false, false, false
      position = 0
      while position < text.length
        char = text[position]
        position += 1
        if escaped then current << char; escaped = false
        elsif char == '\\' then escaped = true
        elsif char == '"'
          if quoted && text[position] == '"'
            current << '"'
            position += 1
          else
            quoted = !quoted
          end
          was_quoted = true
        elsif char == ',' && !quoted
          value = was_quoted ? current : current.strip
          values << ((!was_quoted && (value.empty? || (null_word && value.upcase == 'NULL'))) ? nil : value)
          current, was_quoted = +'', false
        else current << char
        end
      end
      raise Error.new('22P02', 'unterminated quoted container value') if quoted || escaped
      value = was_quoted ? current : current.strip
      values << ((!was_quoted && (value.empty? || (null_word && value.upcase == 'NULL'))) ? nil : value)
      values
    end
  end
end

module JedNative
  module Types
    PG_EPOCH_DAYS = 10_957
    module_function
    def storage_value(type, value, composites = {})
      return nil if value.nil?
      type = normalize(type)
      if type.end_with?('[]')
        elem = type[0...-2]
        elements = value.map { |v| storage_value(elem, v, composites) }
        return elements unless value.respond_to?(:dimensions) && (value.dimensions.length > 1 || value.lower_bound != 1)
        return {dims: value.dimensions, lbounds: value.lower_bounds, elements: elements}
      end
      if RANGES.key?(type)
        return :empty if value.empty
        return {lower: storage_value(value.element_type, value.lower, composites), upper: storage_value(value.element_type, value.upper, composites), lower_inc: value.lower_inc, upper_inc: value.upper_inc}
      end
      if (fields = composites[type])
        return fields.each_with_index.map { |field, index| storage_value(field[:type], value[index], composites) }
      end
      case base(type)
      when 'decimal' then value.to_s
      when 'date'
        value.days.infinite? ? (value.days.positive? ? (1 << 31) - 1 : -(1 << 31)) : value.days - PG_EPOCH_DAYS
      when 'timestamp', 'timestamptz'
        value.micros.infinite? ? (value.micros.positive? ? (1 << 63) - 1 : -(1 << 63)) : value.micros - PG_EPOCH_DAYS * TimestampValue::DAY
      when 'interval' then [value.months, value.days, value.micros]
      when 'json', 'jsonpath' then value.to_s
      when 'jsonb' then json_storage_node(value.value)
      else value.is_a?(CompositeValue) ? value.to_a : value
      end
    end
    def from_storage_value(type, value, composites = {})
      return nil if value.nil?
      type = normalize(type)
      if type.end_with?('[]')
        elem = type[0...-2]
        elements = value.is_a?(Hash) ? value.fetch(:elements) : value
        lower = value.is_a?(Hash) ? value.fetch(:lbounds, [1])[0] : 1
        return ArrayValue.new(elements.map { |v| from_storage_value(elem, v, composites) }, element_type: elem, lower_bound: lower, dimensions: value.is_a?(Hash) ? value[:dims] : nil, lower_bounds: value.is_a?(Hash) ? value[:lbounds] : nil)
      end
      if RANGES.key?(type)
        return RangeValue.new(nil, nil, element_type: RANGES[type], empty: true) if value == :empty
        return RangeValue.new(from_storage_value(RANGES[type], value[:lower], composites), from_storage_value(RANGES[type], value[:upper], composites), (value[:lower_inc] ? '[' : '(') + (value[:upper_inc] ? ']' : ')'), element_type: RANGES[type])
      end
      if (fields = composites[type])
        return CompositeValue.new(fields.each_with_index.map { |field, index| from_storage_value(field[:type], value[index], composites) }, type_name: type, field_names: fields.map { |field| field[:name] })
      end
      case base(type)
      when 'decimal' then Decimal.parse(value)
      when 'date'
        days = value == -(1 << 31) ? -Float::INFINITY : value == (1 << 31) - 1 ? Float::INFINITY : value + PG_EPOCH_DAYS
        DateValue.new(days)
      when 'timestamp', 'timestamptz'
        micros = value == -(1 << 63) ? -Float::INFINITY : value == (1 << 63) - 1 ? Float::INFINITY : value + PG_EPOCH_DAYS * TimestampValue::DAY
        TimestampValue.new(micros, zoned: type == 'timestamptz')
      when 'interval' then IntervalValue.new(*value)
      when 'bytea' then Bytea.new(value)
      when 'uuid' then UUID.new(value)
      when 'json' then JsonValue.parse(value)
      when 'jsonb' then JsonValue.new(json_from_storage_node(value))
      when 'jsonpath' then JsonPathValue.parse(value)
      else value
      end
    end
    def json_storage_node(value)
      case value
      when nil then [:null]
      when true, false then [:bool, value]
      when Integer, Decimal then [:num, value.to_s]
      when String then [:str, value]
      when Array then [:arr, value.map { |v| json_storage_node(v) }]
      when Hash then [:obj, value.keys.sort_by(&:b).map { |key| [key, json_storage_node(value[key])] }]
      end
    end
    def json_from_storage_node(node)
      case node[0]
      when :null then nil
      when :bool, :str then node[1]
      when :num then Decimal.parse(node[1])
      when :arr then node[1].map { |v| json_from_storage_node(v) }
      when :obj then node[1].to_h { |k, v| [k, json_from_storage_node(v)] }
      end
    end
  end
end

module JedNative
  module Types
    module_function
    def parse_array(source, element_type)
      text = source.strip
      dimensions, lower_bounds = [], []
      while (match = /\A\[(-?\d+):(-?\d+)\]/.match(text))
        lower, upper = match[1].to_i, match[2].to_i
        raise Error.new('2202E', 'upper bound cannot be less than lower bound') if upper < lower
        dimensions << upper - lower + 1
        lower_bounds << lower
        text = text[match[0].length..]
      end
      unless dimensions.empty?
        raise Error.new('22P02', 'malformed array literal') unless text.start_with?('=')
        text = text[1..]
      end
      raise Error.new('22P02', 'malformed array literal') unless text.start_with?('{') && text.end_with?('}')
      index, values = 0, []
      parse_level = nil
      parse_level = lambda do |depth|
        raise Error.new('22P02', 'number of array dimensions exceeds six') if depth > 6
        raise Error.new('22P02', 'malformed array literal') unless text[index] == '{'
        index += 1
        count, subshape, nested = 0, nil, nil
        loop do
          index += 1 while index < text.length && /\s/.match?(text[index])
          if text[index] == '}'
            index += 1
            break
          end
          is_nested = text[index] == '{'
          raise Error.new('22P02', 'malformed array dimensions') unless nested.nil? || nested == is_nested
          nested = is_nested
          if is_nested
            shape = parse_level.call(depth + 1)
            raise Error.new('22P02', 'multidimensional arrays must have matching dimensions') if subshape && shape != subshape
            subshape = shape
          else
            quoted, escaped, was_quoted, value = false, false, false, +''
            while index < text.length
              char = text[index]
              break if !quoted && !escaped && [',', '}'].include?(char)
              index += 1
              if escaped then value << char; escaped = false
              elsif char == '\\' then escaped = true
              elsif char == '"' then quoted = !quoted; was_quoted = true
              else value << char
              end
            end
            raise Error.new('22P02', 'malformed array literal') if quoted || escaped
            value = value.strip unless was_quoted
            raise Error.new('22P02', 'malformed array literal') if value.empty? && !was_quoted
            values << (!was_quoted && value.upcase == 'NULL' ? nil : block_given? ? yield(value) : cast(value, element_type))
          end
          count += 1
          index += 1 while index < text.length && /\s/.match?(text[index])
          if text[index] == ','
            index += 1
            raise Error.new('22P02', 'malformed array literal') if text[index] == '}'
          elsif text[index] == '}'
            index += 1
            break
          else raise Error.new('22P02', 'malformed array literal')
          end
        end
        [count] + (subshape || [])
      end
      shape = parse_level.call(1)
      raise Error.new('22P02', 'malformed array literal') unless index == text.length
      raise Error.new('22P02', 'array dimensions do not match contents') if !dimensions.empty? && dimensions != shape
      dimensions = shape if dimensions.empty?
      dimensions = [] if values.empty?
      lower_bounds = [1] * dimensions.length if lower_bounds.empty?
      ArrayValue.new(values, element_type: element_type, dimensions: dimensions, lower_bounds: lower_bounds)
    end
  end
end
