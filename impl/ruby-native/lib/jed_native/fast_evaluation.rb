# frozen_string_literal: true

module JedNative
  # Common scalar leaves and resolved integer operators need none of the
  # extension dispatch used by arrays, collations, JSON, windows, or host calls.
  # Keep this small front door explicit; all other expressions retain the full
  # evaluator, including its stack slicing for deeply nested SQL.
  module FastEvaluation
    # Canonical integer values only need a range check. Avoid forwarding through
    # temporal/composite/collation keyword wrappers for every row in a numeric
    # filter or aggregate; nominal types keep their registry precedence.
    def cast_value(value, type, source_type: nil)
      if value.is_a?(Integer) && Types::INTEGER_BITS.key?(type) && !@types.key?(type)
        return Types.check_integer(value, type)
      end
      super(value, type, source_type: source_type)
    end

    def evaluate(expression, row, group = nil)
      # ROLLUP/CUBE can replace an entire grouping expression with a stored
      # grouping key (including NULL); evaluating its children would be wrong.
      return super if group && @aggregate_group_states&.key?(group.object_id)
      case expression.kind
      when :literal
        return expression.value unless collatable_type?(expression.type)
      when :column
        return row[expression.value] unless collatable_type?(expression.type)
      when :outer
        unless collatable_type?(expression.type)
          return @outer[-1 - expression.value[0]][1][expression.value[1]]
        end
      when :param
        unless collatable_type?(expression.type)
          value = @params[expression.value]
          return expression.extra&.dig(:context_type) ? cast_value(value, expression.type) : value
        end
      when :cast
        if Types::INTEGER_BITS.key?(expression.type) && Types::INTEGER_BITS.key?(expression.args[0].type) &&
           !@types.key?(expression.type) && !@types.key?(expression.args[0].type) && !expression.extra&.dig(:type_mod) &&
           !@native_evaluation_depth && !expression.instance_variable_get(:@native_deep_evaluation)
          charge
          value = evaluate(expression.args[0], row, group)
          return value.nil? ? nil : Types.check_integer(value, expression.type)
        end
      when :unary
        operator = expression.value
        if Types::INTEGER_BITS.key?(expression.type) && !@types.key?(expression.type) &&
           (operator == '+' || operator == '-' || operator == '~') && !@native_evaluation_depth &&
           !expression.instance_variable_get(:@native_deep_evaluation)
          charge
          value = evaluate(expression.args[0], row, group)
          return value if value.nil? || operator == '+'
          value = operator == '-' ? -value : ~value
          return Types.check_integer(value, expression.type)
        end
      when :call
        # Binding/compilation already resolved this builtin's signature. Its
        # integer overload has no collation, allocation meter, or extension
        # work, so preserve its ordinary charge/NULL/range order directly.
        if expression.value == 'abs' && expression.args.length == 1 &&
           Types::INTEGER_BITS.key?(expression.args[0].type) && !@types.key?(expression.args[0].type) &&
           !expression.extra&.dig(:over) && !@native_evaluation_depth &&
           !expression.instance_variable_get(:@native_deep_evaluation)
          charge
          value = evaluate(expression.args[0], row, group)
          return value.nil? ? nil : Types.check_integer(value.abs, expression.args[0].type)
        end
        name = expression.value
        if (name == 'coalesce' || name == 'nullif' || name == 'greatest' || name == 'least') &&
           Types::INTEGER_BITS.key?(expression.type) && !@types.key?(expression.type) &&
           (name != 'nullif' || expression.args.length == 2) &&
           !expression.extra&.dig(:over) && !@native_evaluation_depth &&
           !expression.instance_variable_get(:@native_deep_evaluation) &&
           expression.args.all? { |arg| Types::INTEGER_BITS.key?(arg.type) && !@types.key?(arg.type) }
          charge
          if name == 'coalesce'
            expression.args.each do |arg|
              value = evaluate(arg, row, group)
              return Types.check_integer(value, expression.type) unless value.nil?
            end
            return nil
          end
          # These strict integer comparisons have no variable-width work or
          # collation. Still evaluate every argument before folding, in order.
          values = expression.args.map { |arg| evaluate(arg, row, group) }
          if name == 'nullif'
            left, right = values
            return !left.nil? && !right.nil? && left == right ? nil : left
          end
          result = nil
          values.each do |value|
            next if value.nil?
            result = value if result.nil? || (name == 'greatest' ? value > result : value < result)
          end
          return result
        end
      when :binary
        if expression.extra&.dig(:integer_kernel) && !@native_evaluation_depth &&
           !expression.instance_variable_get(:@native_deep_evaluation)
          charge
          left = evaluate(expression.args[0], row, group)
          right = evaluate(expression.args[1], row, group)
          return integer_binary(expression, left, right)
        end
      end
      super
    end
  end
  Engine.prepend(FastEvaluation)
end
