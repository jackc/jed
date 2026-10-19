# frozen_string_literal: true

module JedNative
  # Fuse a resolved single-table scan's predicate and projections into its row
  # loop. Access-path selection and storage iteration stay with the caller;
  # supported expressions use exactly the existing bounded scalar emitter.
  # The method and its pool capture no input, output, engine, or parameter set.
  class CompiledPipeline < CompiledExpression
    class Kernel < CompiledExpression::Kernel; end

    class Program < CompiledExpression::Program
      def materialize
        klass = Class.new(Kernel)
        klass.class_eval(@source, '(jed compiled pipeline)', 1)
        klass.new(@source, @node_count, @pool)
      end
    end

    # offset/limit describe the SQL shape, never the bound values. Keeping them
    # out of the cache key lets one prepared statement vary either binding.
    # A pipeline admits only complete supported trees: host and other fallback
    # expressions keep their ordinary executor and callback/checkpoint schedule.
    def self.build(predicate, projections, types:, mode:, offset: false, limit: false)
      new(types).build(predicate, projections, mode: mode, offset: offset, limit: limit)
    rescue Unsupported
      nil
    end

    def build(predicate, projections, mode:, offset: false, limit: false)
      raise Unsupported unless mode == :array || mode == :stream
      raise Unsupported unless (offset == true || offset == false) && (limit == true || limit == false)
      raise Unsupported if mode == :array && (offset || limit)
      raise Unsupported unless projections.is_a?(Array) && !projections.empty? && projections.length <= MAX_NODES
      raise Unsupported if predicate && (!predicate.is_a?(Expression) || !BOOLEAN_TYPES.include?(predicate.type))

      line('def call(engine, input, params, output, offset = 0, limit = nil)')
      @indent += 1
      line('pool = @pool')
      line('return output if limit == 0') if limit
      if mode == :array
        # Advance before a filter's next so rejected rows cannot stall the loop.
        line('index = 0')
        line('while index < input.length')
        @indent += 1
        line('row = input[index]')
        line('index += 1')
      else
        line('input.each do |row|')
        @indent += 1
        # Scan-row charges follow the storage pull, before touching any column.
        charge
      end

      if predicate
        accepted = emit(predicate, 1)
        line("next unless #{accepted} == true")
      end
      if offset
        line('if offset.positive?')
        @indent += 1
        line('offset -= 1')
        line('next')
        @indent -= 1
        line('end')
      end
      values = projections.map { |expression| emit(expression, 1) }
      line("result_row = [#{values.join(', ')}]")
      # Streaming output must charge before CursorOutput can suspend execution.
      # The materialized-array executor charges its output count after the loop.
      charge if mode == :stream
      line('output << result_row')
      line('break if limit && output.length >= limit') if limit
      @indent -= 1
      line('end')
      line('output')
      @indent -= 1
      line('end')
      Program.new(@lines.join, @node_count, @pool).materialize
    end
  end
end
