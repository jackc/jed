# frozen_string_literal: true

module JedNative
  module DateTimeArithmeticTypes
    def datetime_binary(op, left, right)
      if op == '+' && (left.is_a?(Integer) && right.is_a?(DateValue) || left.is_a?(IntervalValue) && (right.is_a?(DateValue) || right.is_a?(TimestampValue)))
        return datetime_binary(op, right, left)
      end
      if left.is_a?(IntervalValue) && %w[* /].include?(op)
        factor = Decimal.parse(right).to_r
        raise Error.new('22012', 'division by zero') if op == '/' && factor.zero?
        factor = 1 / factor if op == '/'
        return left * factor
      elsif right.is_a?(IntervalValue) && op == '*' && (left.is_a?(Integer) || left.is_a?(Decimal))
        return datetime_binary(op, right, left)
      end
      if left.is_a?(DateValue) && right.is_a?(DateValue) && op == '-'
        raise Error.new('22008', 'cannot subtract infinite dates') if left.days.is_a?(Float) || right.days.is_a?(Float)
        value = left.days - right.days
        raise Error.new('22008', 'date difference out of range') unless value.between?(-2_147_483_648, 2_147_483_647)
        return value
      elsif left.is_a?(TimestampValue) && right.is_a?(TimestampValue) && op == '-'
        raise Error.new('22008', 'cannot subtract infinite timestamps') if left.micros.is_a?(Float) || right.micros.is_a?(Float)
        return Functions.justify(IntervalValue.new(0, 0, left.micros - right.micros), 'justify_hours')
      elsif (left.is_a?(DateValue) || left.is_a?(TimestampValue)) && right.is_a?(IntervalValue)
        instant = left.is_a?(DateValue) ? left.days * TimestampValue::DAY : left.micros
        return TimestampValue.new(instant, zoned: left.is_a?(TimestampValue) && left.zoned) if instant.is_a?(Float)
      end
      value = super
      TimeZoneData.finite(value.micros) if value.is_a?(TimestampValue) && !value.micros.is_a?(Float)
      value
    end
  end
  Types.singleton_class.prepend(DateTimeArithmeticTypes)

  module DateTimeArithmetic
    def contextual_binary(operator, left, right)
      if %w[+ -].include?(operator)
        [[left, right, true], [right, left, false]].each do |literal, other, first|
          next unless literal.kind == :literal && literal.extra&.dig(:untyped) && literal.value.is_a?(String)
          next unless %w[timestamp timestamptz].include?(other.type)
          target = operator == '+' ? 'interval' : other.type
          literal.value = cast_value(literal.value, target)
          literal.type = target
        end
      end
      super
    end

    def shift_zoned_timestamp(left, right, sign = 1)
      return left if left.micros.is_a?(Float)
      instant = left.micros
      [[:months, right.months], [:days, right.days]].each do |field, amount|
        next if amount.zero?
        charge
        zone = resolve_timezone
        wall = TimestampValue.new(zone.instant_to_local(instant))
        step = IntervalValue.new(field == :months ? amount : 0, field == :days ? amount : 0, 0)
        wall = Types.datetime_binary(sign == 1 ? '+' : '-', wall, step)
        instant = zone.local_to_instant(wall.micros)
      end
      return TimestampValue.new(TimeZoneData.finite(instant + sign * right.micros), zoned: true)
    end

    def evaluate(expression, row, group = nil)
      if expression.kind == :binary && %w[+ -].include?(expression.value) && expression.type == 'timestamptz' && expression.args.any? { |arg| arg.type == 'interval' }
        charge
        left, right = expression.args.map { |arg| evaluate(arg, row, group) }
        return nil if left.nil? || right.nil?
        left, right = right, left if left.is_a?(IntervalValue)
        return shift_zoned_timestamp(left, right, expression.value == '-' ? -1 : 1)
      end
      super
    end

    def binary_type(operator, left, right)
      numeric = %w[i16 i32 i64 decimal unknown null]
      return 'interval' if %w[+ -].include?(operator) && [left, right].include?('interval') && [left, right].any? { |type| %w[unknown null].include?(type) }
      if %w[+ - * / %].include?(operator) && ([left, right] & %w[date timestamp timestamptz interval]).any?
        return 'date' if operator == '+' && left == 'date' && Types::INTEGER_BITS.key?(right) || operator == '+' && right == 'date' && Types::INTEGER_BITS.key?(left)
        return 'date' if operator == '-' && left == 'date' && Types::INTEGER_BITS.key?(right)
        return 'i32' if operator == '-' && left == 'date' && right == 'date'
        return 'interval' if operator == '-' && left == right && %w[timestamp timestamptz].include?(left)
        if %w[+ -].include?(operator) && right == 'interval'
          return left == 'date' ? 'timestamp' : left if %w[date timestamp timestamptz interval].include?(left)
        end
        return right == 'date' ? 'timestamp' : right if operator == '+' && left == 'interval' && %w[date timestamp timestamptz].include?(right)
        return 'interval' if %w[* /].include?(operator) && left == 'interval' && numeric.include?(right)
        return 'interval' if operator == '*' && right == 'interval' && numeric.include?(left)
        raise Error.new('42804', 'incompatible date/time arithmetic operands')
      end
      super
    end
  end
  Engine.prepend(DateTimeArithmetic)
end
