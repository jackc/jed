# frozen_string_literal: true

module JedNative
  # The array variant is a scalar/array comparison, with the same contextual
  # literal rules and strict short circuit as quantified comparisons.
  module WidthBucket
    def compile(node, columns)
      return super unless node[:kind] == :call && node[:name] == 'width_bucket' && node[:args].length == 2
      left, right = node[:args].map { |arg| compile(arg, columns) }
      if %w[unknown null].include?(left.type) && %w[unknown null].include?(right.type)
        raise Error.new('42P18', 'width_bucket needs an element type')
      end
      right.type = left.type + '[]' if %w[unknown null].include?(right.type)
      raise Error.new('42883', 'width_bucket thresholds must be an array') unless right.type.end_with?('[]') && !left.type.end_with?('[]')
      element = right.type.delete_suffix('[]')
      if left.kind == :literal && left.extra&.dig(:untyped) && Functions.family(left.type) != 'float' && %w[integer decimal].include?(Functions.family(left.type)) && Functions.family(element) == 'float'
        left.value = cast_value(left.value, element)
        left.type = element
      end
      left.type = element if %w[unknown null].include?(left.type)
      begin
        validate_comparison(left, Expression.new(kind: :element, type: element))
      rescue Error => error
        raise Error.new('42883', error.message)
      end
      label = CollationRules.combine([left, right].map { |arg| arg.instance_variable_get(:@native_collation) })
      raise Error.new('42P22', 'indeterminate collation') if label&.conflict || label&.none
      Expression.new(kind: :width_bucket_array, type: 'i32', args: [left, right], value: label)
    end

    def evaluate(expression, row, group = nil)
      return super unless expression.kind == :width_bucket_array
      charge
      operand = evaluate(expression.args[0], row, group)
      return nil if operand.nil?
      thresholds = evaluate(expression.args[1], row, group)
      return nil if thresholds.nil?
      raise Error.new('2202E', 'thresholds must be one-dimensional') if thresholds.dimensions.length > 1
      raise Error.new('22004', 'thresholds cannot contain NULL') if thresholds.any?(&:nil?)
      return thresholds.length if operand.is_a?(Float) && operand.nan?
      label = expression.value
      operand = CollationRules.wrap(operand, label)
      left, right = 0, thresholds.length
      while left < right
        middle = (left + right) / 2
        threshold = CollationRules.wrap(thresholds[middle], label)
        charge
        if label&.table
          charge(collation_comparison_points(operand, threshold))
        else
          scalar_work('<', operand, threshold)
        end
        if Types.compare(operand, threshold).negative?
          right = middle
        else
          left = middle + 1
        end
      end
      left
    end
  end
  Engine.prepend(WidthBucket)

  module WidthBucketBinding
    def function(node, scope, wanted)
      if node[:name] == 'width_bucket' && node[:args].length == 2
        left, right = node[:args]
        element = expression(right, scope).delete_suffix('[]')
        expression(left, scope, element)
        return 'i32'
      end
      super
    end
  end
  ParameterBindings.prepend(WidthBucketBinding)
end
