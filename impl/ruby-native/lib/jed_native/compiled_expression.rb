# frozen_string_literal: true

module JedNative
  # A small runtime compiler for already-resolved scalar expressions. Source is
  # assembled only from the templates below and validated numeric ordinals.
  # SQL names, literals, and diagnostics remain data in a separate frozen pool.
  # The generated method receives its execution context on every invocation;
  # it retains no engine, session, bindings, or rows.
  class CompiledExpression
    MAX_NODES = 192
    MAX_DEPTH = 32
    MAX_SOURCE_BYTES = 32_768
    MAX_POOL = 256
    TYPES = %w[i16 i32 i64 boolean null unknown].freeze
    BOOLEAN_TYPES = %w[boolean null unknown].freeze
    COMPARISONS = { '=' => '==', '<>' => '!=', '!=' => '!=', '<' => '<',
                    '<=' => '<=', '>' => '>', '>=' => '>=' }.freeze
    ARITHMETIC = { '+' => '+', '-' => '-', '*' => '*' }.freeze
    INTEGER_FUNCTIONS = %w[abs coalesce nullif greatest least].freeze

    class Kernel
      attr_reader :source, :source_bytes, :node_count, :pool

      def initialize(source, node_count, pool)
        @source, @node_count, @pool = source.freeze, node_count, pool.freeze
        @source_bytes = source.bytesize
        freeze
      end
    end

    # Emission can be compared and cached before asking Ruby to compile it.
    # prepare_many only admits complete supported trees, so its pool contains
    # immutable scalar data and never a resolved expression or execution state.
    class Program
      attr_reader :source, :source_bytes, :node_count, :pool

      def initialize(source, node_count, pool)
        @source, @node_count, @pool = source.freeze, node_count, pool.freeze
        @source_bytes = source.bytesize
        freeze
      end

      def materialize
        klass = Class.new(Kernel)
        klass.class_eval(@source, '(jed compiled expression)', 1)
        klass.new(@source, @node_count, @pool)
      end
    end

    class Unsupported < StandardError; end
    private_constant :Unsupported

    def self.build(expression, types:)
      new(types).build(expression)
    rescue Unsupported
      nil
    end

    # A projection can contain scalar operations beside types whose evaluation
    # remains in the ordinary engine. Each unsupported root is delegated intact,
    # preserving all of its special evaluation rules and observable ordering.
    def self.build_many(expressions, types:)
      new(types).build_many(expressions)
    rescue Unsupported
      nil
    end

    def self.prepare_many(expressions, types:)
      new(types).prepare_many(expressions)
    rescue Unsupported
      nil
    end

    def initialize(types)
      @types = types
      @pool = []
      @lines = []
      @node_count = @source_bytes = @indent = 0
    end

    def build(expression)
      begin_method
      value = emit(expression, 1)
      line(value)
      finish_method.materialize
    end

    def build_many(expressions)
      raise Unsupported unless expressions.is_a?(Array) && !expressions.empty? && expressions.length <= MAX_NODES
      begin_method
      useful = false
      values = expressions.map do |expression|
        state = [@lines.length, @pool.length, @source_bytes, @node_count, @indent]
        begin
          value = emit(expression, 1)
          useful ||= !%i[literal column param].include?(expression.kind)
          value
        rescue Unsupported
          line_count, pool_count, @source_bytes, @node_count, @indent = state
          @lines.slice!(line_count..)
          @pool.slice!(pool_count..)
          raise Unsupported unless expression.is_a?(Expression) && @node_count < MAX_NODES
          value = "v#{@node_count}"
          @node_count += 1
          line("#{value} = engine.evaluate(#{pooled(expression)}, row)")
          value
        end
      end
      raise Unsupported unless useful
      line("[#{values.join(', ')}]")
      finish_method.materialize
    end

    def prepare_many(expressions)
      raise Unsupported unless expressions.is_a?(Array) && !expressions.empty? && expressions.length <= MAX_NODES
      begin_method
      values = expressions.map { |expression| emit(expression, 1) }
      raise Unsupported unless expressions.any? { |expression| !%i[literal column param].include?(expression.kind) }
      line("[#{values.join(', ')}]")
      finish_method
    end

    private

    def begin_method
      line('def call(engine, row, params)')
      @indent += 1
      line('pool = @pool')
    end

    def finish_method
      @indent -= 1
      line('end')
      Program.new(@lines.join, @node_count, @pool)
    end

    def line(source)
      raise Unsupported if @indent > MAX_DEPTH + 2
      value = ('  ' * @indent) + source + "\n"
      @source_bytes += value.bytesize
      raise Unsupported if @source_bytes > MAX_SOURCE_BYTES
      @lines << value
    end

    def pooled(value)
      raise Unsupported if @pool.length >= MAX_POOL
      value = value.dup.freeze if value.is_a?(String)
      index = @pool.length
      @pool << value
      "pool[#{index}]"
    end

    def integer?(type)
      Types::INTEGER_BITS.key?(type) && !@types.key?(type)
    end

    def boolean?(type)
      BOOLEAN_TYPES.include?(type) && !@types.key?(type)
    end

    def emit(expression, depth)
      raise Unsupported unless expression.is_a?(Expression)
      raise Unsupported if depth > MAX_DEPTH || @node_count >= MAX_NODES
      raise Unsupported unless TYPES.include?(expression.type) && !@types.key?(expression.type)
      raise Unsupported if expression.instance_variable_get(:@native_deep_evaluation) ||
                           expression.instance_variable_get(:@native_iterative_arithmetic) ||
                           expression.instance_variable_get(:@native_collation)
      output = "v#{@node_count}"
      @node_count += 1
      case expression.kind
      when :literal
        value = expression.value
        valid = value.nil? || integer?(expression.type) && value.is_a?(Integer) ||
                expression.type == 'boolean' && (value == true || value == false)
        raise Unsupported unless valid
        line("#{output} = #{pooled(value)}")
      when :column, :param
        ordinal = expression.value
        raise Unsupported unless ordinal.is_a?(Integer) && ordinal >= 0
        source = expression.kind == :column ? 'row' : 'params'
        line("#{output} = #{source}[#{ordinal}]")
        if expression.kind == :param && expression.extra&.dig(:context_type)
          line("#{output} = engine.cast_value(#{output}, #{pooled(expression.type)})")
        end
      when :cast then emit_cast(expression, output, depth)
      when :unary then emit_unary(expression, output, depth)
      when :binary then emit_binary(expression, output, depth)
      when :call then emit_call(expression, output, depth)
      when :case then emit_case(expression, output, depth)
      else raise Unsupported
      end
      output
    end

    def child(expression, depth)
      emit(expression, depth + 1)
    end

    def children(expression, count)
      args = expression.args
      raise Unsupported unless args.is_a?(Array) && args.length == count
      args
    end

    def charge
      line('engine.charge')
    end

    def checked_integer(value, type, message: nil)
      bits = Types::INTEGER_BITS.fetch(type)
      minimum, maximum = -(1 << (bits - 1)), (1 << (bits - 1)) - 1
      code = pooled('22003')
      message = pooled(message || "#{type} out of range")
      line("raise ::JedNative::Error.new(#{code}, #{message}) if #{value} < #{minimum} || #{value} > #{maximum}")
    end

    def emit_cast(expression, output, depth)
      argument = children(expression, 1).first
      raise Unsupported unless argument.is_a?(Expression)
      null = argument.kind == :literal && argument.value.nil? && BOOLEAN_TYPES.include?(argument.type)
      compatible = integer?(expression.type) && (integer?(argument.type) || null) ||
                   expression.type == 'boolean' && (argument.type == 'boolean' || null)
      raise Unsupported unless compatible
      raise Unsupported if expression.extra&.dig(:type_mod)
      charge
      value = child(argument, depth)
      line("#{output} = #{value}")
      if integer?(expression.type)
        line("unless #{output}.nil?")
        @indent += 1
        checked_integer(output, expression.type)
        @indent -= 1
        line('end')
      end
    end

    def emit_unary(expression, output, depth)
      argument = children(expression, 1).first
      raise Unsupported unless argument.is_a?(Expression)
      operator = expression.value
      if %w[+ - ~].include?(operator)
        raise Unsupported unless integer?(expression.type) && integer?(argument.type)
      elsif operator == 'not'
        raise Unsupported unless boolean?(argument.type) && expression.type == 'boolean'
      elsif !%w[is\ null is\ not\ null is\ true is\ false is\ not\ true is\ not\ false is\ unknown is\ not\ unknown].include?(operator)
        raise Unsupported
      end
      charge
      value = child(argument, depth)
      case operator
      when '+' then line("#{output} = #{value}")
      when '-', '~'
        line("#{output} = #{value}.nil? ? nil : #{operator}#{value}")
        line("unless #{output}.nil?")
        @indent += 1
        checked_integer(output, expression.type)
        @indent -= 1
        line('end')
      when 'not' then line("#{output} = #{value}.nil? ? nil : !#{value}")
      when 'is null', 'is unknown' then line("#{output} = #{value}.nil?")
      when 'is not null', 'is not unknown' then line("#{output} = !#{value}.nil?")
      when 'is true' then line("#{output} = #{value} == true")
      when 'is false' then line("#{output} = #{value} == false")
      when 'is not true' then line("#{output} = #{value} != true")
      when 'is not false' then line("#{output} = #{value} != false")
      end
    end

    def emit_binary(expression, output, depth)
      left, right = children(expression, 2)
      raise Unsupported unless left.is_a?(Expression) && right.is_a?(Expression)
      operator = expression.value
      if operator == 'and' || operator == 'or'
        raise Unsupported unless expression.type == 'boolean' && boolean?(left.type) && boolean?(right.type)
        return emit_boolean(expression, output, depth)
      end
      raise Unsupported unless expression.extra&.dig(:integer_kernel) && integer?(left.type) && integer?(right.type)
      comparison = COMPARISONS[operator]
      arithmetic = ARITHMETIC[operator]
      raise Unsupported unless comparison || arithmetic || operator == '/' || operator == '%'
      raise Unsupported unless comparison ? expression.type == 'boolean' : integer?(expression.type)
      charge
      a, b = child(left, depth), child(right, depth)
      line("if #{a}.nil? || #{b}.nil?")
      @indent += 1
      line("#{output} = nil")
      @indent -= 1
      line('else')
      @indent += 1
      if comparison
        line("#{output} = #{a} #{comparison} #{b}")
      else
        if arithmetic
          line("#{output} = #{a} #{arithmetic} #{b}")
        else
          line("raise ::JedNative::Error.new(#{pooled('22012')}, #{pooled('division by zero')}) if #{b}.zero?")
          if operator == '%'
            line("#{output} = #{a}.remainder(#{b})")
          else
            line("#{output} = (#{a}.abs / #{b}.abs) * (#{a}.negative? != #{b}.negative? ? -1 : 1)")
          end
        end
        checked_integer(output, expression.type, message: "integer out of range for #{expression.type}")
      end
      @indent -= 1
      line('end')
    end

    def emit_boolean(expression, output, depth)
      charge
      left = child(expression.args[0], depth)
      filter = expression.instance_variable_defined?(:@native_filter_conjunction)
      if filter
        raise Unsupported unless expression.value == 'and'
        line("if #{left} == true")
      else
        stop = expression.value == 'and' ? 'false' : 'true'
        line("if #{left} == #{stop}")
        @indent += 1
        line("#{output} = #{stop}")
        @indent -= 1
        line('else')
      end
      @indent += 1
      right = child(expression.args[1], depth)
      if filter
        line("#{output} = #{right}")
      elsif expression.value == 'and'
        line("#{output} = #{right} == false ? false : (#{left}.nil? || #{right}.nil? ? nil : true)")
      else
        line("#{output} = #{right} == true ? true : (#{left}.nil? || #{right}.nil? ? nil : false)")
      end
      @indent -= 1
      if filter
        line('else')
        @indent += 1
        line("#{output} = #{left}")
        @indent -= 1
      end
      line('end')
    end

    def emit_call(expression, output, depth)
      name, args = expression.value, expression.args
      raise Unsupported unless INTEGER_FUNCTIONS.include?(name) && integer?(expression.type)
      raise Unsupported unless args.is_a?(Array) && !args.empty? && args.all? { |argument| argument.is_a?(Expression) && integer?(argument.type) }
      extra = expression.extra || {}
      raise Unsupported if extra[:over] || extra[:filter] || extra[:filter_expr] || extra[:distinct] ||
                           extra[:within_group] || !(extra[:order_by] || []).empty?
      raise Unsupported if name == 'abs' && args.length != 1 || name == 'nullif' && args.length != 2
      charge
      if name == 'coalesce'
        emit_coalesce(args, 0, output, expression.type, depth)
        return
      end
      values = args.map { |argument| child(argument, depth) }
      case name
      when 'abs'
        line("#{output} = #{values[0]}.nil? ? nil : #{values[0]}.abs")
        line("unless #{output}.nil?")
        @indent += 1
        checked_integer(output, args[0].type)
        @indent -= 1
        line('end')
      when 'nullif'
        a, b = values
        line("#{output} = !#{a}.nil? && !#{b}.nil? && #{a} == #{b} ? nil : #{a}")
      when 'greatest', 'least'
        operator = name == 'greatest' ? '>' : '<'
        line("#{output} = nil")
        values.each do |value|
          line("#{output} = #{value} if !#{value}.nil? && (#{output}.nil? || #{value} #{operator} #{output})")
        end
      end
    end

    def emit_coalesce(args, index, output, type, depth)
      value = child(args[index], depth)
      line("if #{value}.nil?")
      @indent += 1
      if index + 1 < args.length
        emit_coalesce(args, index + 1, output, type, depth)
      else
        line("#{output} = nil")
      end
      @indent -= 1
      line('else')
      @indent += 1
      line("#{output} = #{value}")
      checked_integer(output, type)
      @indent -= 1
      line('end')
    end

    def emit_case(expression, output, depth)
      operand, branches, fallback = children(expression, 3)
      raise Unsupported if operand
      raise Unsupported unless integer?(expression.type) || expression.type == 'boolean'
      raise Unsupported unless branches.is_a?(Array) && !branches.empty? && branches.all? do |branch|
        branch.is_a?(Array) && branch.length == 2 && branch[0].is_a?(Expression) && boolean?(branch[0].type)
      end
      charge
      emit_case_branch(branches, 0, fallback, output, expression.type, depth)
    end

    def emit_case_branch(branches, index, fallback, output, type, depth)
      if index == branches.length
        value = child(fallback, depth)
        line("#{output} = engine.cast_value(#{value}, #{pooled(type)})")
        return
      end
      condition, result = branches[index]
      test = child(condition, depth)
      line("if #{test} == true")
      @indent += 1
      value = child(result, depth)
      line("#{output} = engine.cast_value(#{value}, #{pooled(type)})")
      @indent -= 1
      line('else')
      @indent += 1
      emit_case_branch(branches, index + 1, fallback, output, type, depth)
      @indent -= 1
      line('end')
    end
  end
end
