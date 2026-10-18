# frozen_string_literal: true

module JedNative
  module ScalarEdges
    def compile(node, columns)
      if node[:kind] == :literal && Types::INTEGER_BITS.key?(node[:type])
        Types.check_integer(node[:value], node[:type]) unless node[:value].nil?
      elsif node[:kind] == :call && node[:name] == 'nullif'
        arguments = node[:args].map { |arg| compile(arg, columns) }
        left, right = arguments
        contextual_binary('=', left, right)
        left.type = right.type if %w[unknown null].include?(left.type)
        left.type = 'text' if %w[unknown null].include?(left.type)
        if Types::INTEGER_BITS.key?(left.type) && right.type == 'decimal'
          left = Expression.new(kind: :numeric_context, type: 'decimal', args: [left])
          arguments[0] = left
        end
        node = node.merge(args: arguments.map { |argument| { kind: :resolved_expression, expression: argument } })
      elsif node[:kind] == :call && node[:name] == 'width_bucket' && node[:args].length == 4
        arguments = node[:args].map { |arg| compile(arg, columns) }
        if arguments.first(3).any? { |arg| %w[f32 f64].include?(arg.type) }
          arguments.first(3).each do |arg|
            if arg.kind == :literal && arg.extra&.dig(:untyped) && %w[i16 i32 i64 decimal].include?(arg.type)
              arg.value, arg.type = arg.value.to_f, 'f64'
            end
            raise Error.new('42883', 'width_bucket arguments have incompatible types') unless %w[f32 f64 null unknown].include?(arg.type)
          end
        else
          raise Error.new('42883', 'width_bucket arguments must be numeric') unless arguments.first(3).all? { |arg| %w[i16 i32 i64 decimal null unknown].include?(arg.type) }
        end
        raise Error.new('42883', 'width_bucket count must be an integer') unless %w[i16 i32 i64 null unknown].include?(arguments.last.type)
        node = node.merge(args: arguments.map { |argument| { kind: :resolved_expression, expression: argument } })
      end
      expression = super(node, columns)
      if expression.kind == :call && %w[coalesce greatest least].include?(expression.value)
        expression.type = 'text' if %w[null unknown].include?(expression.type)
        ensure_comparable_type(expression.type) if %w[greatest least].include?(expression.value)
      elsif expression.kind == :case && expression.args[0].nil?
        expression.args[1].each do |test, _|
          raise Error.new('42804', 'CASE condition must be boolean') unless %w[boolean null unknown].include?(test.type)
        end
      end
      expression
    end

    def evaluate(expression, row, group = nil)
      return Decimal.parse(evaluate(expression.args.first, row, group)) if expression.kind == :numeric_context
      super
    end

    def contextual_binary(operator, left, right)
      if Resolution::COMPARISONS.include?(operator)
        [[left, right], [right, left]].each do |literal, other|
          next unless literal.kind == :literal && literal.extra&.dig(:untyped) && literal.type == 'text' && literal.value.is_a?(String)
          next unless other.kind == :literal && !other.extra&.dig(:untyped) && other.type != 'date' && literal_adapts?(literal, other.type)
          literal.value = cast_value(literal.value, other.type)
          literal.type = other.type
        end
      end
      super
    end

    def binary_type(operator, left, right)
      if %w[+ - *].include?(operator) && (Types::RANGES.key?(left) || Types::RANGES.key?(right))
        raise Error.new('42883', 'range operators require identical range types') unless left == right
      elsif operator == '||' && !left.end_with?('[]') && !right.end_with?('[]')
        return 'text' if [left, right].any? { |type| %w[text null unknown].include?(type) }
        return 'bytea' if left == 'bytea' && right == 'bytea'
        return 'jsonb' if left == 'jsonb' && right == 'jsonb'
        raise Error.new('42883', 'concatenation requires text, bytea, jsonb, or an array')
      end
      super
    end

    def validate_type(type, modifier = nil)
      raise Error.new('0A000', 'integer display widths are not supported') if modifier && Types::INTEGER_BITS.key?(type)
      super
    end
  end
  Engine.prepend(ScalarEdges)

  module ScalarBindingEdges
    def function(node, scope, wanted)
      if node[:name] == 'nullif'
        left, right = node[:args]
        types = [expression(left, scope), expression(right, scope)]
        [0, 1].each do |index|
          argument = node[:args][index]
          other = types[1 - index]
          if argument[:kind] == :literal && argument[:untyped] && types[index] == 'text' && StrictContext::STRING_NATIVE.include?(other)
            types[index] = other
          end
        end
        common(*types)
        node[:args].each_with_index do |argument, index|
          if argument[:kind] == :param && known?(types[1 - index])
            types[index] = expression(argument, scope, types[1 - index])
          end
        end
        type = known?(types[0]) ? types[0] : types[1]
        type = 'decimal' if Types::INTEGER_BITS.key?(type) && types[1] == 'decimal'
        return known?(type) ? type : 'text'
      elsif node[:name] == 'generate_series'
        node[:args].each { |argument| expression(argument, scope, 'i64') }
        return 'i64'
      end
      result = super
      %w[coalesce greatest least].include?(node[:name]) && !known?(result) ? 'text' : result
    end
  end
  ParameterBindings.prepend(ScalarBindingEdges)
end
