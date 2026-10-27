# frozen_string_literal: true

module JedNative
  module FilterPredicates
    SUBQUERY_KINDS = %i[subquery exists array_query query_value].freeze

    def compile_boolean_value(node, columns)
      return unless node
      expression = compile(node, columns)
      raise Error.new('42804', 'predicate must be boolean') unless %w[boolean unknown null].include?(expression.type)
      expression
    end

    # A WHERE, ON, or HAVING cost-orders its top-level AND chain (planner.md
    # §3.0); compile_source_order_filter keeps the written order instead.
    def compile_filter(node, columns)
      source_order = @native_source_order_filter
      @native_source_order_filter = false
      expression = super
      expression = order_filter_conjuncts(expression) unless source_order
      mark_filter_conjunction(expression)
      expression
    ensure
      @native_source_order_filter = source_order
    end

    # A partial index's WHERE, ON CONFLICT ... WHERE, a check option, and a
    # MERGE WHEN condition evaluate their conjuncts as written.
    def compile_source_order_filter(node, columns)
      previous = @native_source_order_filter
      @native_source_order_filter = true
      compile_filter(node, columns)
    ensure
      @native_source_order_filter = previous
    end

    def evaluate(expression, row, group = nil)
      return super unless expression.instance_variable_defined?(:@native_filter_conjunction)
      charge
      left = evaluate(expression.args[0], row, group)
      return left unless left == true
      evaluate(expression.args[1], row, group)
    end

    # Stable-sorts the chain: a conjunct without a correlated subquery before
    # one with, then fewer estimated operator_eval first, ties as written. An
    # ordered chain is returned unchanged; a reordered one is rebuilt left-deep.
    def order_filter_conjuncts(expression)
      return expression unless filter_conjunction?(expression)
      conjuncts = []
      pending = [expression]
      until pending.empty?
        current = pending.pop
        if filter_conjunction?(current)
          pending << current.args[1] << current.args[0]
        else
          conjuncts << current
        end
      end
      keys = conjuncts.each_with_index.map { |conjunct, index| [correlated_subquery?(conjunct) ? 1 : 0, expression_work(conjunct), index] }
      sorted = keys.sort
      return expression if sorted == keys
      sorted.map { |_, _, index| conjuncts[index] }.reduce do |left, right|
        Expression.new(kind: :binary, type: 'boolean', value: 'and', args: [left, right])
      end
    end

    private

    def filter_conjunction?(expression)
      expression.is_a?(Expression) && expression.kind == :binary && expression.value == 'and'
    end

    def correlated_subquery?(expression)
      return false unless expression.is_a?(Expression)
      return true if SUBQUERY_KINDS.include?(expression.kind)
      return true if %i[query_membership].include?(expression.kind) && expression.extra[:correlated]
      (expression.args || []).any? { |argument| argument.is_a?(Array) ? argument.any? { |item| correlated_subquery?(item) } : correlated_subquery?(argument) }
    end

    def mark_filter_conjunction(expression)
      return unless expression && expression.kind == :binary && expression.value == 'and'
      expression.instance_variable_set(:@native_filter_conjunction, true)
      expression.args.each { |argument| mark_filter_conjunction(argument) }
    end
  end
  Engine.prepend(FilterPredicates)
end
