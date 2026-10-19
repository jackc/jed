# frozen_string_literal: true

module JedNative
  # Specialize two independent phases over already materialized rows. Filtering
  # retains the original row objects; grouping-key evaluation retains its full
  # row-major matrix. Never interleave the phases: all predicate errors must
  # precede every grouping-key error, just as in the ordinary executor.
  class CompiledRelation < CompiledExpression
    class Kernel < CompiledExpression::Kernel; end

    class Program < CompiledExpression::Program
      def materialize
        klass = Class.new(Kernel)
        klass.class_eval(@source, '(jed compiled relation)', 1)
        klass.new(@source, @node_count, @pool)
      end
    end

    def self.prepare_filter(predicate, types: {}, memory: false, text: true)
      new(types, text: text).prepare_filter(predicate, memory: memory)
    rescue Unsupported
      nil
    end

    def self.prepare_keys(expressions, types: {}, text: true)
      new(types, text: text).prepare_keys(expressions)
    rescue Unsupported
      nil
    end

    def prepare_filter(predicate, memory: false)
      raise Unsupported unless (memory == true || memory == false) && predicate.is_a?(Expression) && boolean?(predicate.type)
      begin_rows
      accepted = emit(predicate, 1)
      if memory
        line("if #{accepted} == true")
        @indent += 1
        line('output << row')
        @indent -= 1
        line('else')
        @indent += 1
        # Keep byte inspection even when owner is nil. The existing helper
        # evaluates this argument before query_memory_release checks its owner.
        line('engine.query_memory_release(owner, engine.query_memory_row_bytes(row, columns, mask))')
        @indent -= 1
        line('end')
      else
        line("output << row if #{accepted} == true")
      end
      finish_rows
    end

    def prepare_keys(expressions)
      raise Unsupported unless expressions.is_a?(Array) && !expressions.empty? && expressions.length <= MAX_NODES
      begin_rows
      values = expressions.map { |expression| emit(expression, 1) }
      line("output << [#{values.join(', ')}]")
      finish_rows
    end

    private

    def begin_rows
      line('def call(engine, rows, params, owner = nil, columns = nil, mask = nil)')
      @indent += 1
      line('pool = @pool')
      line('output = []')
      line('index = 0')
      line('while index < rows.length')
      @indent += 1
      line('row = rows[index]')
      line('index += 1')
    end

    def finish_rows
      @indent -= 1
      line('end')
      line('output')
      @indent -= 1
      line('end')
      Program.new(@lines.join, @node_count, @pool)
    end
  end
end
