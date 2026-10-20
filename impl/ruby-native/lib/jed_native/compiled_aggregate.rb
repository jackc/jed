# frozen_string_literal: true

module JedNative
  # Specialize one primitive aggregate inside the existing grouping executor.
  # Keep argument evaluation and reduction as separate phases: a later input
  # error must still precede an overflow found while reducing earlier inputs.
  # The generated method owns only its immutable scalar pool, never a group,
  # engine, resolved expression, or parameter binding.
  class CompiledAggregate < CompiledExpression
    class Kernel < CompiledExpression::Kernel; end

    class Program < CompiledExpression::Program
      def materialize
        materialize_with(Kernel, '(jed compiled aggregate)')
      end
    end

    def self.prepare(expression, types: {}, text: true)
      new(types, text: text).prepare(expression)
    rescue Unsupported
      nil
    end

    def self.build(expression, types: {}, text: true)
      prepare(expression, types: types, text: text)&.materialize
    end

    def prepare(expression)
      raise Unsupported unless expression.is_a?(Expression) && expression.kind == :call &&
                               expression.args.is_a?(Array) && expression.args.length == 1 &&
                               !@types.key?(expression.type) &&
                               !expression.instance_variable_get(:@native_collation)
      extra = expression.extra || {}
      raise Unsupported unless extra.is_a?(Hash)
      raise Unsupported if extra[:distinct] || extra[:within_group] || extra[:over] || extra[:window_id] ||
                           !(extra[:aggregate_order] || []).empty? || !(extra[:order_by] || []).empty?
      argument = expression.args.first
      raise Unsupported unless argument.is_a?(Expression) && !@types.key?(argument.type) &&
                               !argument.instance_variable_get(:@native_collation)
      name = expression.value
      case name
      when 'count', 'count_star'
        raise Unsupported unless expression.type == 'i64' &&
                                 (primitive?(argument) || name == 'count' && argument.kind == :star)
      when 'sum'
        raise Unsupported unless expression.type == 'i64' && %w[i16 i32].include?(argument.type)
      when 'min', 'max'
        raise Unsupported unless (integer?(argument.type) || argument.type == 'boolean') && expression.type == argument.type
      when 'bool_and', 'bool_or', 'every'
        raise Unsupported unless expression.type == 'boolean' && argument.type == 'boolean'
      when 'bit_and', 'bit_or', 'bit_xor'
        raise Unsupported unless integer?(argument.type) && expression.type == argument.type
      else
        raise Unsupported
      end
      filter = extra[:filter_expr]
      raise Unsupported if filter && (!filter.is_a?(Expression) || !boolean?(filter.type))

      # Reserve a node for the aggregate itself; children use the same emitter
      # and limits as the scalar tier. All names below select fixed templates.
      @node_count = 1
      line('def call(engine, group, params)')
      @indent += 1
      line('pool = @pool')
      line('values = []')
      line('index = 0')
      line('while index < group.length')
      @indent += 1
      line('row = group[index]')
      line('index += 1')
      if filter
        accepted = emit(filter, 1)
        line("next unless #{accepted} == true")
      end
      charge
      if argument.kind == :star
        raise Unsupported if @node_count >= MAX_NODES
        @node_count += 1
        line('values << 1')
      else
        value = emit(argument, 1)
        line("values << #{value} unless #{value}.nil?")
      end
      @indent -= 1
      line('end')

      if name == 'count' || name == 'count_star'
        line('values.length')
      else
        line('return nil if values.empty?')
        emit_reduction(name, argument.type)
      end
      @indent -= 1
      line('end')
      Program.new(@lines.join, @node_count, @pool)
    end

    private

    def primitive?(argument)
      integer?(argument.type) || argument.type == 'boolean' ||
        %w[unknown null].include?(argument.type) && argument.kind == :literal && argument.value.nil?
    end

    def emit_reduction(name, type)
      if name == 'sum'
        line('result = 0')
        line('index = 0')
      else
        line('result = values[0]')
        line('index = 1')
      end
      line('while index < values.length')
      @indent += 1
      line('value = values[index]')
      case name
      when 'sum'
        line('result += value')
        checked_integer('result', 'i64')
      when 'min', 'max'
        if type == 'boolean'
          line(name == 'min' ? 'result &&= value' : 'result ||= value')
        else
          line("result = value if value #{name == 'min' ? '<' : '>'} result")
        end
      when 'bool_and', 'every' then line('result &&= value')
      when 'bool_or' then line('result ||= value')
      when 'bit_and' then line('result &= value')
      when 'bit_or' then line('result |= value')
      when 'bit_xor' then line('result ^= value')
      end
      line('index += 1')
      @indent -= 1
      line('end')
      line('result')
    end
  end
end
