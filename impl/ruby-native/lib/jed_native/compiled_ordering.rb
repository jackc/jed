# frozen_string_literal: true

module JedNative
  # Specialize only comparison of already evaluated sort keys. The existing
  # operator still evaluates keys, owns rows, and chooses its heap/sort/spill
  # algorithm. Generated methods retain no engine, expressions, or input rows.
  class CompiledOrdering < CompiledExpression
    class Kernel < CompiledExpression::Kernel
      def initialize(source, node_count, pool)
        # Sorters take a block, while heaps call the kernel directly. This
        # self-only cycle is collectible and avoids rebuilding the block per use.
        @call_proc = method(:call).to_proc.freeze
        super
      end

      def to_proc = @call_proc
    end

    class Program < CompiledExpression::Program
      def materialize
        klass = Class.new(Kernel)
        klass.class_eval(@source, '(jed compiled ordering)', 1)
        klass.new(@source, @node_count, @pool)
      end
    end

    def self.prepare(keys, orders, types: {})
      new(types).prepare(keys, orders)
    rescue Unsupported
      nil
    end

    def self.build(keys, orders, types: {})
      prepare(keys, orders, types: types)&.materialize
    end

    def prepare(keys, orders)
      raise Unsupported unless keys.is_a?(Array) && orders.is_a?(Array) &&
                               !keys.empty? && keys.length <= MAX_NODES && keys.length == orders.length
      policies = keys.zip(orders).map do |expression, order|
        raise Unsupported unless expression.is_a?(Expression) && !@types.key?(expression.type) &&
                                 !expression.instance_variable_get(:@native_collation)
        nil_literal = %w[null unknown].include?(expression.type) && expression.kind == :literal && expression.value.nil?
        raise Unsupported unless integer?(expression.type) || expression.type == 'boolean' || nil_literal
        raise Unsupported unless order.is_a?(Hash) && [nil, :asc, :desc, 'asc', 'desc'].include?(order[:direction]) &&
                                 [nil, true, false].include?(order[:desc]) &&
                                 [nil, :first, :last, 'first', 'last'].include?(order[:nulls])
        descending = order[:desc] || order[:direction] == :desc || order[:direction] == 'desc'
        nulls = order[:nulls] || (descending ? :first : :last)
        [expression.type == 'boolean', descending, nulls == :first || nulls == 'first']
      end
      @node_count = keys.length
      line('def call(left, right)')
      @indent += 1
      policies.each_with_index do |(boolean, descending, nulls_first), index|
        line("a = left[1][#{index}]")
        line("b = right[1][#{index}]")
        line('if a.nil?')
        @indent += 1
        line("return #{nulls_first ? -1 : 1} unless b.nil?")
        @indent -= 1
        line('elsif b.nil?')
        @indent += 1
        line("return #{nulls_first ? 1 : -1}")
        @indent -= 1
        line('else')
        @indent += 1
        line(boolean ? 'comparison = (a ? 1 : 0) <=> (b ? 1 : 0)' : 'comparison = a <=> b')
        line("return #{descending ? '-comparison' : 'comparison'} unless comparison.zero?")
        @indent -= 1
        line('end')
      end
      # SortedSelect defines stable ordering by the source input ordinal.
      line('left[2] <=> right[2]')
      @indent -= 1
      line('end')
      Program.new(@lines.join, @node_count, @pool)
    end
  end
end
