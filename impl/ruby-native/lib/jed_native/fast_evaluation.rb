# frozen_string_literal: true

module JedNative
  # Common scalar leaves and resolved integer operators need none of the
  # extension dispatch used by arrays, collations, JSON, windows, or host calls.
  # Keep this small front door explicit; all other expressions retain the full
  # evaluator, including its stack slicing for deeply nested SQL.
  module FastEvaluation
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
