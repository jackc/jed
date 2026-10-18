# frozen_string_literal: true

module JedNative
  # Work scales with logical values, independent of Ruby's integer limb size.
  module ScalarWork
    OPERATORS = (Resolution::COMPARISONS + %w[+ - * / %]).freeze

    def evaluate(expression, row, group = nil)
      if expression.kind == :binary && OPERATORS.include?(expression.value) && expression.args.any? { |arg| %w[decimal text bytea].include?(Types.base(arg.type)) }
        charge
        left, right = expression.args.map { |arg| evaluate(arg, row, group) }
        scalar_work(expression.value, left, right)
        return Types.binary(expression.value, left, right, type: expression.type)
      end
      super
    end

    def scalar_work(operator, left, right)
      return if left.nil? || right.nil?
      if (left.is_a?(Decimal) || right.is_a?(Decimal)) && [left, right].all? { |value| value.is_a?(Decimal) || value.is_a?(Integer) }
        left, right = Decimal.parse(left), Decimal.parse(right)
        return if %w[/ %].include?(operator) && right.zero?
        first = left.zero? ? 0 : left.coefficient.abs.to_s.length
        second = right.zero? ? 0 : right.coefficient.abs.to_s.length
        scale = [left.scale, right.scale].max
        aligned_first, aligned_second = first + scale - left.scale, second + scale - right.scale
        groups = ->(digits) { [1, (digits + 3).div(4)].max }
        units = case operator
        when '*' then groups.call(first) * groups.call(second)
        when '/' then groups.call(first + left.division_scale(right) + right.scale - left.scale) * groups.call(second)
        when '%' then groups.call(aligned_first) * groups.call(aligned_second)
        else [groups.call(aligned_first), groups.call(aligned_second)].max
        end
        charge(units - 1) if units > 1
      elsif Resolution::COMPARISONS.include?(operator) && left.is_a?(String) && right.is_a?(String)
        length = [left.is_a?(Bytea) ? left.bytesize : left.length, right.is_a?(Bytea) ? right.bytesize : right.length].min
        charge(length - 1) if length > 1
      end
    end
  end
  Engine.prepend(ScalarWork)

  module FoldComparisonWork
    def call(name, args, context: nil, arg_types: nil)
      if context && %w[greatest least nullif].include?(name)
        if name == 'nullif'
          context.scalar_work('=', *args)
        else
          accumulator = nil
          args.each do |value|
            next if value.nil?
            if accumulator
              context.scalar_work('<', accumulator, value)
              comparison = Types.compare(accumulator, value)
              accumulator = value if name == 'greatest' ? comparison.negative? : comparison.positive?
            else
              accumulator = value
            end
          end
        end
      end
      super
    end
  end
  Functions.singleton_class.prepend(FoldComparisonWork)
end
