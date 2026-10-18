# frozen_string_literal: true

module JedNative
  module StrictContext
    STRING_NATIVE = %w[f32 f64 bytea uuid date timestamp timestamptz interval json jsonb jsonpath].freeze

    def compile(node, columns)
      return node[:expression] if node[:kind] == :resolved_expression
      super
    end

    def assign(value, column, source = nil)
      if source && !value.nil?
        source_type, target = Types.normalize(source.type), Types.normalize(column[:type])
        return super if target == 'date' && date_clock_offset(value)
        unless context_assignable?(source, target)
          raise Error.new('42804', "cannot assign #{source_type} to #{target}")
        end
      end
      super
    end

    def contextual_binary(operator, left, right)
      [[left, right], [right, left]].each do |constant, other|
        if constant.kind == :literal && constant.extra&.dig(:untyped) && constant.type == 'decimal' && Types::INTEGER_BITS.key?(other.type)
          constant.extra = constant.extra.merge(untyped: false)
        end
      end
      if Resolution::COMPARISONS.include?(operator)
        [[left, right], [right, left]].each do |constant, other|
          next unless constant.kind == :literal && constant.extra&.dig(:untyped) && !constant.value.nil?
          next if other.kind == :literal && other.extra&.dig(:untyped) || %w[unknown null].include?(other.type)
          unless literal_adapts?(constant, other.type)
            raise Error.new('42804', 'literal is incompatible with comparison type')
          end
        end
      end
      super
    end

    private

    def literal_adapts?(expression, target)
      source = Types.normalize(expression.type)
      return true if source == target
      if expression.value.is_a?(String)
        return STRING_NATIVE.include?(target) || Types::RANGES.key?(target) || @types.key?(target) || target.end_with?('[]')
      elsif expression.value.is_a?(Integer)
        return %w[i16 i32 i64 decimal f32 f64].include?(target)
      elsif expression.value.is_a?(Decimal)
        return %w[decimal f32 f64].include?(target)
      elsif expression.value.is_a?(Float)
        return %w[f32 f64].include?(target)
      end
      false
    end

    def context_assignable?(expression, target)
      source = Types.normalize(expression.type)
      return true if source == target || source == 'unknown'
      return true if @types[target] && source == 'record'
      return literal_adapts?(expression, target) if expression.kind == :literal && expression.extra&.dig(:untyped)
      if expression.kind == :array && target.end_with?('[]')
        return expression.args.all? { |child| context_assignable?(child, child.type.end_with?('[]') ? target : target.delete_suffix('[]')) }
      end
      %w[implicit assignment].include?(FunctionCatalog::CAST_MODES[[source, target]])
    end

    def validate_comparison(left, right)
      if left.type.end_with?('[]') && right.type.end_with?('[]')
        a, b = left.type.delete_suffix('[]'), right.type.delete_suffix('[]')
        unless a == b || Types::INTEGER_BITS.key?(a) && Types::INTEGER_BITS.key?(b)
          raise Error.new('42804', 'arrays have different element types')
        end
      end
      super
    end
  end
  Engine.prepend(StrictContext)
end
