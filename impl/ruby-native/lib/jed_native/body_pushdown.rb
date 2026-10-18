# frozen_string_literal: true

module JedNative
  module BodyPushdown
    def compile(node, columns)
      expression = super
      if node[:kind] == :column && node[:native_body_type]
        expression = expression.dup
        expression.type = node[:native_body_type]
        expression.extra = (expression.extra || {}).merge(native_body_type: node[:native_body_type])
      end
      expression
    end

    def push_into_body(source, terms, columns)
      query = if %i[subquery derived].include?(source[:kind])
        source[:query]
      elsif source[:kind] == :table
        binding = @ctes[source[:name]]
        binding.respond_to?(:query) ? (binding.query if cte_body_pushable?(source[:name])) : @views[source[:name]]&.dig(:query)
      end
      return [source, terms] unless query && !source[:lateral]
      remaining = []
      terms.each do |term|
        resolved = compile(term, columns)
        equality_preserved = expression_nodes(resolved).none? do |expression|
          expression.kind == :cast && expression.type == 'text' && %w[decimal f32 f64].include?(expression.args.first.type)
        end
        indexed = body_output_slots(term, columns)
        rewritten = indexed && rewrite_body_predicate(query, indexed, columns.map { |column| column[:type] }, equality_preserved)
        if rewritten
          query = rewritten
        else
          remaining << term
        end
      end
      source = if %i[subquery derived].include?(source[:kind])
        source.merge(query: query)
      else
        source.merge(native_body_query: query)
      end
      [source, remaining]
    end

    private

    def body_output_slots(node, columns)
      case node
      when Array then node.map { |child| body_output_slots(child, columns) }
      when Hash
        if node[:kind] == :column
          index = columns.index { |column| column[:name] == node[:name] && (!node[:table] || column[:table] == node[:table]) }
          return nil unless index
          { kind: :body_output_slot, ordinal: index }
        else
          node.transform_values { |value| value.is_a?(Hash) || value.is_a?(Array) ? body_output_slots(value, columns) : value }
        end
      else node
      end
    end

    def body_slot_indices(node)
      case node
      when Array then node.flat_map { |child| body_slot_indices(child) }
      when Hash then node[:kind] == :body_output_slot ? [node[:ordinal]] : node.values.flat_map { |child| body_slot_indices(child) }
      else []
      end
    end

    def replace_body_slots(node, replacements)
      case node
      when Array then node.map { |child| replace_body_slots(child, replacements) }
      when Hash
        node[:kind] == :body_output_slot ? replacements.fetch(node[:ordinal]) : node.transform_values { |child| replace_body_slots(child, replacements) }
      else node
      end
    end

    def rewrite_body_predicate(query, indexed, output_types, equality_preserved)
      return if query[:limit] || query[:offset] || query[:with]
      indices = body_slot_indices(indexed).uniq
      if %i[set_operation setop].include?(query[:kind])
        return if !equality_preserved && !(query[:op].to_s == 'union' && query[:all])
        left = rewrite_body_predicate(query[:left], indexed, output_types, equality_preserved)
        return unless left
        right = rewrite_body_predicate(query[:right], indexed, output_types, equality_preserved)
        return query.merge(left: left, right: right) if right
        return
      end
      return unless query[:kind] == :select
      analyzer = ParameterBindings.new(self, nil)
      outer_scope = @outer.flat_map(&:first)
      scope = query_input_columns(query, @ctes.transform_values(&:columns), outer_scope, analyzer)
      scope = scope.take(scope.length - outer_scope.length)
      projections = expand_stars(query[:columns], scope)
      groups = resolve_group_nodes(query, scope)
      grouped = !groups.empty? || contains_aggregate?(query[:columns]) || contains_aggregate?(query[:having])
      return if grouped && (groups.empty? || groups.any? { |group| %i[grouping_sets rollup cube].include?(group[:kind]) })
      windows = window_nodes([query[:columns], query[:order_by]])
      return if !equality_preserved && (grouped || query[:distinct] || !windows.empty?)
      group_slots = groups.filter_map { |group| compile(group, scope).value if group[:kind] == :column }
      definitions = {}
      (query[:windows] || {}).each { |name, spec| definitions[name] = resolve_window_spec(spec, definitions) }
      partition_slots = windows.map do |window|
        resolve_window_spec(window[:over], definitions)[:partition_by].filter_map { |key| compile(key, scope).value if key[:kind] == :column }
      end
      replacements = {}
      indices.each do |index|
        expression = projections[index]&.dig(:expr)
        return unless expression && expression[:kind] == :column
        compiled = compile(expression, scope)
        return unless compiled.kind == :column
        return if grouped && !group_slots.include?(compiled.value)
        return unless partition_slots.all? { |slots| slots.include?(compiled.value) }
        target = output_types[index]
        return unless body_widening?(compiled.type, target)
        replacement = expression
        if Types::INTEGER_BITS.key?(compiled.type) && Types::INTEGER_BITS.key?(target) && compiled.type != target
          replacement = expression.merge(native_body_type: target)
        elsif compiled.type != target && (target == 'decimal' || compiled.type == 'f32' && target == 'f64')
          replacement = { kind: :cast, expr: expression, type: target }
        end
        replacements[index] = replacement
      end
      predicate = replace_body_slots(indexed, replacements)
      query.merge(where: query[:where] ? join_conjunctions([query[:where], predicate]) : predicate)
    end

    def body_widening?(from, to)
      return true if from == to || %w[null unknown].include?(from)
      return Types::INTEGER_BITS[from] <= Types::INTEGER_BITS[to] if Types::INTEGER_BITS.key?(from) && Types::INTEGER_BITS.key?(to)
      return true if Types::INTEGER_BITS.key?(from) && to == 'decimal' || from == 'f32' && to == 'f64'
      return body_widening?(from.delete_suffix('[]'), to.delete_suffix('[]')) if from.end_with?('[]') && to.end_with?('[]')
      false
    end
  end
  Engine.prepend(BodyPushdown)
end
