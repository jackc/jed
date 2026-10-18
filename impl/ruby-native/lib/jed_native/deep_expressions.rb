# frozen_string_literal: true

module JedNative
  # A left-associative expression at the legal depth boundary must not consume
  # one Ruby frame per resolver layer per SQL operator. Keep the same tree and
  # preorder charging, but resolve and fold its long arithmetic spine iteratively.
  module DeepExpressions
    ARITHMETIC = %w[+ - * / %].freeze
    def compile(node, columns)
      return super unless node[:kind] == :binary && ARITHMETIC.include?(node[:op])
      spine, current = [], node
      while current[:kind] == :binary && ARITHMETIC.include?(current[:op])
        spine << current
        current = current[:left]
      end
      return super if spine.length < 32
      expression = compile(current, columns)
      spine.reverse_each do |item|
        expression = super(item.merge(left: { kind: :resolved_expression, expression: expression }), columns)
      end
      expression.instance_variable_set(:@native_deep_evaluation, true)
      expression.instance_variable_set(:@native_iterative_arithmetic, true) if expression.kind == :binary
      expression
    end
    def evaluate(expression, row, group = nil)
      return super unless expression.instance_variable_get(:@native_iterative_arithmetic)
      spine, current = [], expression
      while current.kind == :binary && ARITHMETIC.include?(current.value)
        charge
        spine << current
        current = current.args.first
      end
      value = evaluate(current, row, group)
      spine.reverse_each do |item|
        right = evaluate(item.args.last, row, group)
        value = if item.extra&.dig(:integer_kernel)
          integer_binary(item, value, right)
        else
          scalar_work(item.value, value, right)
          Types.binary(item.value, value, right, type: item.type)
        end
      end
      value
    end
  end
  Engine.prepend(DeepExpressions)
end
