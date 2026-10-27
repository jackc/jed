# frozen_string_literal: true
require_relative 'memory_schedule'

module JedNative
  module LogicalMemory
    extend MemorySchedule
    module_function
    def value_bytes(value, type = nil) = MemorySchedule::VALUE + payload(value, type)
    def row_bytes(row, types = nil)
      MemorySchedule::ROW + row.each_with_index.sum { |value, i| value_bytes(value, types && types[i]) }
    end
    def payload(value, type = nil)
      case value
      when nil, UUID then 0
      when String then value.bytesize
      when Decimal
        value.zero? ? 0 : (value.coefficient.abs.to_s.length + 3) / 4 * MemorySchedule::DECIMAL_GROUP
      when JsonValue then value.binary ? JsonValue.payload(value.value) : value.to_s.bytesize
      when JsonPathValue then value.to_s.bytesize
      when ArrayValue
        value.dimensions.length * MemorySchedule::ARRAY_DIM + value.sum { |item| value_bytes(item, value.element_type) }
      when Array then value.sum { |item| value_bytes(item) }
      when RangeValue
        value.empty ? 0 : [value.lower, value.upper].compact.sum { |bound| value_bytes(bound, value.element_type) }
      else 0
      end
    end
  end

  module ScalarGrowth
    GROWTH_FUNCTIONS = %w[array_append array_prepend array_cat overlay concat json_build_array jsonb_build_array
                          json_build_object jsonb_build_object json_object jsonb_object to_json to_jsonb
                          array_to_json row_to_json json_scalar json_serialize encode quote_literal quote_ident
                          quote_nullable like_escape similar_to_escape to_char jsonb_set jsonb_set_lax jsonb_insert].freeze

    def constant_expression?(value)
      case value
      when nil then true
      when Array then value.all? { |child| constant_expression?(child) }
      when Expression
        return false if value.extra.is_a?(Hash) && value.extra[:subquery_origin]
        return value.instance_variable_get(:@native_constant) if value.instance_variable_defined?(:@native_constant)
        constant = case value.kind
        when :literal, :param then true
        when :column, :outer, :subquery, :exists, :array_query, :query_compare, :query_value, :query_membership, :row_query_compare then false
        when :call
          entries = FunctionCatalog::VOLATILITY[value.value]
          (!entries || entries.all? { |_, volatility| volatility == 'immutable' }) && constant_expression?(value.args)
        else constant_expression?(value.args)
        end
        value.instance_variable_set(:@native_constant, constant)
        constant
      else true
      end
    end

    def evaluate(expression, row, group = nil)
      charge if %i[array row].include?(expression.kind) && !@native_literal_container
      result = super
      return result if result.nil? || constant_expression?(expression)
      growth = expression.kind == :array ||
               expression.kind == :binary && expression.value == '||' ||
               expression.kind == :call && GROWTH_FUNCTIONS.include?(expression.value)
      if expression.kind == :cast
        source = expression.args[0].type
        growth ||= expression.type == 'text' && !expression.extra&.dig(:type_mod) && (source.end_with?('[]') || %w[bytea jsonb].include?(source)) ||
                   expression.type == 'json' && source == 'jsonb' || expression.type == 'text[]' && source == 'bytea[]'
      end
      charge(LogicalMemory.payload(result, expression.type)) if growth
      result
    end

    def assignment_value(expression)
      previous = @native_literal_container
      @native_literal_container = literal_container?(expression)
      evaluate(expression, [])
    ensure
      @native_literal_container = previous
    end

    private

    def literal_container?(expression)
      %i[literal param].include?(expression.kind) || %i[array row].include?(expression.kind) && expression.args.all? { |arg| literal_container?(arg) }
    end
  end
  Engine.prepend(ScalarGrowth)
end
