# frozen_string_literal: true

module JedNative
  module Contradictions
    def plan_select(statement)
      previous = @native_explain_contradiction
      if statement[:where]
        outer = @outer.flat_map(&:first)
        columns = query_input_columns(statement, @ctes.transform_values(&:columns), outer, ParameterBindings.new(self, nil))
        columns = columns.take(columns.length - outer.length)
        @native_explain_contradiction ||= where_contradiction?(plan_compile(statement[:where], columns))
      end
      node = super
      if @native_explain_contradiction && statement[:where]
        filter = plan_nodes(node).find { |entry| entry.label == 'Filter' }
        if filter && !filter.detail.include?('; contradiction')
          filter.detail = filter.detail.include?('; output=') ? filter.detail.sub('; output=', '; contradiction; output=') : filter.detail + '; contradiction'
        end
      end
      node
    ensure
      @native_explain_contradiction = previous
    end

    def plan_mutation(statement)
      previous = @native_explain_contradiction
      if %i[update delete].include?(statement[:kind]) && statement[:where]
        columns = table_columns(table(statement[:table]), statement[:alias] || statement[:table])
        @native_explain_contradiction = where_contradiction?(plan_compile(statement[:where], columns))
      end
      node = super
      if @native_explain_contradiction
        filter = plan_nodes(node).find { |entry| entry.label == 'Filter' }
        filter.detail += '; contradiction' if filter
      end
      node
    ensure
      @native_explain_contradiction = previous
    end

    def rewrite_join_inputs(sources, joins, predicate = nil)
      return if @native_explain_contradiction
      super
    end

    def estimate_scan_candidate(stored, predicate, access, statement)
      result = super
      %i[scan_rows scan_cost output_rows pipeline_cost].each { |key| result[key] = 0 } if @native_explain_contradiction
      result
    end

    def plan_node(label, detail, rows, cost, children = [], phase = nil)
      if @native_explain_contradiction && (label.start_with?('Scan ', 'Subquery', 'CTE Scan ', 'SRF ', 'Catalog Scan ') || label == 'Result')
        rows = cost = 0
      end
      result = super(label, detail, rows, cost, children, phase)
      result.actual = 0 if @native_explain_contradiction
      result
    end

    def primary_scan(stored, predicate)
      return [] if where_contradiction?(predicate)
      super
    end

    def execute_scan_access(stored, predicate, access)
      return [] if where_contradiction?(predicate)
      super
    end

    def where_contradiction?(expression)
      return false unless expression
      columns = {}
      conjunctions(expression).each do |term|
        literal, value = contradiction_literal(term)
        return true if literal && (value.nil? || value == false)
        next unless term.kind == :binary && %w[= < <= > >=].include?(term.value)
        left, right = term.args
        operator = term.value
        a, av = contradiction_literal(left)
        b, bv = contradiction_literal(right)
        if a && b
          return true if av.nil? || bv.nil?
          comparison = contradiction_compare(av, bv, operator)
          return true if comparison && !contradiction_accepts?(comparison, operator)
        elsif left.kind == :column && b || right.kind == :column && a
          if right.kind == :column
            left, right, bv = right, left, av
            operator = { '=' => '=', '<' => '>', '<=' => '>=', '>' => '<', '>=' => '<=' }.fetch(operator)
          end
          return true if bv.nil?
          bounds = columns[left.value] ||= []
          bounds.each do |previous, value|
            comparison = contradiction_compare(value, bv, previous == '=' && operator == '=' ? '=' : '<')
            next unless comparison
            if previous == '='
              return true unless contradiction_accepts?(comparison, operator)
            elsif operator == '='
              return true unless contradiction_accepts?(-comparison, previous)
            elsif %w[> >=].include?(previous) && %w[< <=].include?(operator)
              return true if comparison.positive? || comparison.zero? && (previous == '>' || operator == '<')
            elsif %w[< <=].include?(previous) && %w[> >=].include?(operator)
              return true if comparison.negative? || comparison.zero? && (previous == '<' || operator == '>')
            end
          end
          bounds << [operator, bv]
        end
      end
      false
    end

    private

    def contradiction_literal(expression)
      return [false, nil] if expression.extra&.dig(:subquery_origin)
      return [true, expression.value] if expression.kind == :literal
      return [true, @params[expression.value]] if expression.kind == :param && expression.value < @params.length
      [false, nil]
    end

    def contradiction_compare(left, right, operator)
      return nil if (left.is_a?(String) || right.is_a?(String)) && operator != '='
      return nil unless left.class == right.class || left.is_a?(Numeric) && right.is_a?(Numeric)
      Types.compare(left, right)
    end

    def contradiction_accepts?(comparison, operator)
      case operator
      when '=' then comparison.zero?
      when '<' then comparison.negative?
      when '<=' then comparison <= 0
      when '>' then comparison.positive?
      when '>=' then comparison >= 0
      end
    end
  end
  Engine.prepend(Contradictions)
end
