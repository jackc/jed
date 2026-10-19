# frozen_string_literal: true

module JedNative
  module SpillSemantics
    def select(statement)
      return super unless spill_select_shape?(statement)
      labels = view_query_columns(statement, nil, @outer.flat_map(&:first)) if @environment && !@environment.collations.empty?
      scope = ((statement[:from] || []) + (statement[:joins] || []).map { |join| join[:relation] }).flat_map { |source| spill_source_columns(source) }
      resolve_group_nodes(statement, scope).each do |node|
        expand_grouping_sets([node]).flatten(1).each { |key| ensure_comparable_type(compile(key, scope).type) }
      end
      if statement[:distinct]
        expand_stars(statement[:columns], scope).each { |item| ensure_comparable_type(compile(item[:expr], scope).type) }
      end
      result = super
      labels ? decorate_collation_relation(result, labels) : result
    end

    private

    def spill_source_columns(source)
      return super unless @environment && !@environment.collations.empty?
      query_source_columns(source, @ctes.transform_values(&:columns), @outer.flat_map(&:first), ParameterBindings.new(self, source))
    end

    def spill_rewrite(expression, keys, aggregates)
      result = super
      if result.kind == :column && expression.kind != :column
        %i[@native_collation @record_fields].each do |name|
          result.instance_variable_set(name, expression.instance_variable_get(name)) if expression.instance_variable_defined?(name)
        end
      elsif result.kind == :column && expression.instance_variable_defined?(:@native_collation)
        result.instance_variable_set(:@native_collation, expression.instance_variable_get(:@native_collation))
      end
      if result.kind != :column && expression.extra
        result.extra = spill_rewrite_metadata(expression.extra, keys, aggregates)
      end
      result.args = spill_rewrite_metadata(expression.args, keys, aggregates) if result.kind != :column && expression.args
      result
    end

    def spill_rewrite_metadata(value, keys, aggregates)
      case value
      when Expression then spill_rewrite(value, keys, aggregates)
      when Array then value.map { |item| spill_rewrite_metadata(item, keys, aggregates) }
      when Hash then value.transform_values { |item| spill_rewrite_metadata(item, keys, aggregates) }
      else value
      end
    end

    def spill_collation_sort_value(value)
      label = CollationRules.label(value)
      return value unless label&.table && value
      if value.is_a?(ArrayValue)
        ArrayValue.new(value.map { |item| item && Bytea.new(label.table.sort_key(item)) }, element_type: 'bytea',
                       dimensions: value.dimensions, lower_bounds: value.lower_bounds)
      else
        Bytea.new(label.table.sort_key(value))
      end
    end
  end
  Engine.prepend(SpillSemantics)
end
