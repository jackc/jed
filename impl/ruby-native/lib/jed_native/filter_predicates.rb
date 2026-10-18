# frozen_string_literal: true

module JedNative
  module FilterPredicates
    def compile_boolean_value(node, columns)
      return unless node
      expression = compile(node, columns)
      raise Error.new('42804', 'predicate must be boolean') unless %w[boolean unknown null].include?(expression.type)
      expression
    end

    def compile_filter(node, columns)
      expression = super
      mark_filter_conjunction(expression)
      expression
    end

    def evaluate(expression, row, group = nil)
      return super unless expression.instance_variable_defined?(:@native_filter_conjunction)
      charge
      left = evaluate(expression.args[0], row, group)
      return left unless left == true
      evaluate(expression.args[1], row, group)
    end

    private

    def mark_filter_conjunction(expression)
      return unless expression && expression.kind == :binary && expression.value == 'and'
      expression.instance_variable_set(:@native_filter_conjunction, true)
      expression.args.each { |argument| mark_filter_conjunction(argument) }
    end
  end
  Engine.prepend(FilterPredicates)
end
