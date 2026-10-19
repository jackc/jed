# frozen_string_literal: true

module JedNative
  module ArraySemantics
    def compile(node, columns)
      if node[:kind] == :call
        if node[:variadic]
          return super if node[:name].start_with?('json') || %w[concat concat_ws format].include?(node[:name])
          raise Error.new('42883', 'function has no variadic overload') unless %w[num_nulls num_nonnulls].include?(node[:name])
          argument = compile(node[:args].first, columns)
          raise Error.new('42804', 'VARIADIC argument must be an array') unless argument.type.end_with?('[]')
          return Expression.new(kind: :variadic_count, type: 'i32', value: node[:name], args: [argument])
        end
        if %w[num_nulls num_nonnulls].include?(node[:name]) && node[:args].empty?
          raise Error.new('42883', 'function needs at least one argument')
        end
        if node[:name] == 'array_to_string'
          argument = compile(node[:args].first, columns)
          raise Error.new(%w[text null unknown].include?(argument.type) ? '42804' : '42883', 'array_to_string requires an array') unless argument.type.end_with?('[]')
          node = node.merge(args: [{ kind: :resolved_expression, expression: argument }] + node[:args].drop(1))
        end
      elsif node[:kind] == :quantified && node[:right][:kind] != :subquery
        left, right = compile(node[:left], columns), compile(node[:right], columns)
        raise Error.new('42P18', 'array type is indeterminate') if right.type == 'unknown'
        raise Error.new('42809', 'ANY/ALL requires an array operand') unless right.type.end_with?('[]')
        element = Expression.new(kind: :element, type: right.type.delete_suffix('[]'))
        begin
          contextual_binary(node[:op], left, element)
          validate_comparison(left, element)
        rescue Error => error
          raise Error.new('42883', error.message)
        end
        return Expression.new(kind: :quantified, type: 'boolean', args: [left, right], extra: node)
      end
      expression = super(node, columns)
      if expression.kind == :binary && expression.value == '||' && expression.type.end_with?('[]')
        left, right = expression.args
        left.type = expression.type if %w[null unknown].include?(left.type)
        right.type = expression.type if %w[null unknown].include?(right.type)
        name = left.type.end_with?('[]') ? (right.type.end_with?('[]') ? 'array_cat' : 'array_append') : 'array_prepend'
        expression.kind, expression.value, expression.extra = :call, name, {}
      end
      expression
    end

    def contextual_binary(operator, left, right)
      if %w[|| @> <@ &&].include?(operator)
        [[left, right], [right, left]].each do |value, sibling|
          next unless sibling.type.end_with?('[]')
          target = sibling.type.delete_suffix('[]')
          if operator == '||' && value.kind == :literal && value.extra&.dig(:untyped) && literal_adapts?(value, target)
            value.value, value.type = cast_value(value.value, target), target
          end
          next unless value.kind == :array
          if value.args.all? { |child| child.kind == :literal && child.extra&.dig(:untyped) && literal_adapts?(child, target) }
            value.args.each { |child| child.value = cast_value(child.value, target); child.type = target }
            value.type = sibling.type
          end
        end
      end
      super
    end

    def binary_type(operator, left, right)
      if [left, right].any? { |type| type.end_with?('[]') }
        if %w[@> <@ &&].include?(operator)
          raise Error.new('42883', 'array operands must have the same element type') unless left == right
          return 'boolean'
        elsif operator == '||'
          begin
            return super
          rescue Error
            raise Error.new('42883', 'array concatenation has incompatible operands')
          end
        end
      end
      super
    end

    def evaluate(expression, row, group = nil)
      if expression.kind == :variadic_count
        charge
        values = evaluate(expression.args.first, row, group)
        return nil unless values
        nulls = values.count(nil)
        return expression.value == 'num_nulls' ? nulls : values.length - nulls
      elsif expression.kind == :subscript && expression.extra.any? { |spec| spec[:slice] }
        charge
        value = evaluate(expression.args.first, row, group)
        return nil unless value
        ranges = expression.extra.each_with_index.map do |spec, index|
          base = value.lower_bounds[index] || 1
          last = base + (value.dimensions[index] || 0) - 1
          low = spec[:slice] ? (spec[:low] ? evaluate(spec[:low], row, group) : base) : 1
          high = spec[:slice] ? (spec[:high] ? evaluate(spec[:high], row, group) : last) : evaluate(spec[:index], row, group)
          return nil if low.nil? || high.nil?
          [[low, base].max - base, [high, last].min - base]
        end
        empty = ranges.length > value.dimensions.length || ranges.any? { |low, high| low > high }
        return ArrayValue.new([], element_type: value.element_type) if empty || value.empty?
        value.dimensions.each_with_index { |length, index| ranges[index] ||= [0, length - 1] }
        dimensions = ranges.map { |low, high| high - low + 1 }
        offsets = [0]
        ranges.each_with_index do |(low, high), index|
          offsets = offsets.flat_map { |offset| (low..high).map { |coordinate| offset * value.dimensions[index] + coordinate } }
        end
        return ArrayValue.new(offsets.map { |offset| value[offset] }, element_type: value.element_type, dimensions: dimensions)
      end
      super
    end
  end
  Engine.prepend(ArraySemantics)

  module ArrayFunctionRules
    def call(name, args, context: nil, arg_types: nil)
      if %w[array_position array_positions array_remove].include?(name)
        raise Error.new('0A000', 'function requires a one-dimensional array') if args[0] && args[0].dimensions.length > 1
        raise Error.new('22004', 'initial subscript must not be NULL') if name == 'array_position' && args.length == 3 && args[2].nil?
      end
      super(name, args, context: context, arg_types: arg_types)
    end
  end
  Functions.singleton_class.prepend(ArrayFunctionRules)

  module ArrayBindingRules
    def function(node, scope, wanted)
      if node[:name] == 'array_to_string'
        type = expression(node[:args].first, scope)
        raise Error.new(%w[text null unknown].include?(type) ? '42804' : '42883', 'array_to_string requires an array') unless type.end_with?('[]')
      end
      super
    end
  end
  ParameterBindings.prepend(ArrayBindingRules)
end
