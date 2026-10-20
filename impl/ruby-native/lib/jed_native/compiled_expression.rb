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
    MAX_POOL_BYTES = 32_768
    MAX_METHOD_CACHE_ENTRIES = 64
    TYPES = %w[i16 i32 i64 boolean text null unknown].freeze
    BOOLEAN_TYPES = %w[boolean null unknown].freeze
    COMPARISONS = { '=' => '==', '<>' => '!=', '!=' => '!=', '<' => '<',
                    '<=' => '<=', '>' => '>', '>=' => '>=' }.freeze
    ARITHMETIC = { '+' => '+', '-' => '-', '*' => '*' }.freeze
    INTEGER_FUNCTIONS = %w[abs coalesce nullif greatest least].freeze
    # Preserve all flags, including nil versus false, without retaining a
    # resolved expression's label or a host-loaded collation table. Only these
    # fixed C labels can appear in emitted source, by validated numeric index.
    C_LABEL_FLAGS = [nil, false, true].product([nil, false], [nil, false, true], [nil, false]).map(&:freeze).freeze
    C_LABELS = C_LABEL_FLAGS.map do |explicit, conflict, default, none|
      CollationLabel.new(name: 'C'.freeze, explicit: explicit, conflict: conflict,
                         table: nil, default: default, none: none).freeze
    end.freeze

    class Kernel
      attr_reader :source, :source_bytes, :node_count, :pool

      def initialize(source, node_count, pool)
        @source, @node_count, @pool = source.freeze, node_count, pool.freeze
        @source_bytes = source.bytesize
        freeze
      end

      # The resolved comparison usually supplies its C label. Unlabeled
      # operands can still carry transient runtime metadata, so retain the
      # ordinary evaluator's dynamic-label path instead of assuming raw bytes.
      def compare_text(engine, left, right, operator, label)
        return nil if left.nil? || right.nil?
        label ||= CollationRules.combine([CollationRules.label(left), CollationRules.label(right)])
        if operator == '=' || operator == '<>' || operator == '!='
          engine.scalar_work(operator, left, right) if engine.respond_to?(:scalar_work)
          equal = left == right
          return operator == '=' ? equal : !equal
        end
        raise Error.new('42P22', 'indeterminate collation') if label&.conflict || label&.none
        if label&.table
          engine.charge(engine.collation_comparison_points(left, right))
          left, right = CollationRules.wrap(left, label), CollationRules.wrap(right, label)
        elsif engine.respond_to?(:scalar_work)
          engine.scalar_work(operator, left, right)
        end
        Types.binary(operator, left, right, type: 'boolean')
      end
    end

    # Literal pools belong to individual kernels, while identical templates can
    # share a Ruby method and its JIT compilation across plans and sessions. The
    # process cache owns only fixed source and generated classes, never a pool or
    # execution context. Ruby may retain JIT code after an entry is evicted.
    class MethodCache
      State = Struct.new(:pid, :mutex, :entries)
      private_constant :State

      def initialize
        @state = State.new(Process.pid, Mutex.new, {}).freeze
      end

      def fetch(kernel_class, source, filename)
        # Normal emitters already enforce this limit. Keep direct internal
        # Program construction from retaining an oversized cache key as well.
        return compile(kernel_class, source, filename) if source.bytesize > MAX_SOURCE_BYTES
        source = ::String.new(source).freeze

        pid = Process.pid
        state = @state
        if state.pid != pid
          # Never acquire an inherited mutex: its owner may not exist after a
          # fork. Publish a complete replacement rather than mutate old state.
          replacement = State.new(pid, Mutex.new, {}).freeze
          @state = replacement if @state.pid != pid
          state = @state
        end
        state.mutex.synchronize do
          key = [kernel_class, source]
          if (klass = state.entries.delete(key))
            state.entries[key.freeze] = klass
            return klass
          end
          klass = compile(kernel_class, source, filename)
          state.entries.shift if state.entries.length >= MAX_METHOD_CACHE_ENTRIES
          state.entries[[kernel_class, source].freeze] = klass
          klass
        end
      end

      private

      def compile(kernel_class, source, filename)
        klass = Class.new(kernel_class)
        klass.class_eval(source, filename, 1)
        klass.freeze
      end
    end
    METHOD_CACHE = MethodCache.new
    private_constant :MethodCache, :METHOD_CACHE

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
        materialize_with(Kernel, '(jed compiled expression)')
      end

      private

      def materialize_with(kernel_class, filename)
        klass = METHOD_CACHE.fetch(kernel_class, @source, filename)
        klass.new(@source, @node_count, @pool)
      end
    end

    class Unsupported < StandardError; end
    private_constant :Unsupported

    def self.build(expression, types:, text: true)
      new(types, text: text).build(expression)
    rescue Unsupported
      nil
    end

    # A projection can contain scalar operations beside types whose evaluation
    # remains in the ordinary engine. Each unsupported root is delegated intact,
    # preserving all of its special evaluation rules and observable ordering.
    def self.build_many(expressions, types:, text: true)
      new(types, text: text).build_many(expressions)
    rescue Unsupported
      nil
    end

    def self.prepare_many(expressions, types:, text: true)
      new(types, text: text).prepare_many(expressions)
    rescue Unsupported
      nil
    end

    def initialize(types, text: true)
      @types = types
      @text = text
      @pool = []
      @lines = []
      @node_count = @source_bytes = @pool_bytes = @indent = 0
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
        state = [@lines.length, @pool.length, @source_bytes, @pool_bytes, @node_count, @indent]
        begin
          value = emit(expression, 1)
          useful ||= !%i[literal column param].include?(expression.kind)
          value
        rescue Unsupported
          line_count, pool_count, @source_bytes, @pool_bytes, @node_count, @indent = state
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
      if value.is_a?(String)
        bytes = value.bytesize
        raise Unsupported if @pool_bytes + bytes > MAX_POOL_BYTES
        @pool_bytes += bytes
        value = value.dup.freeze
      end
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

    def c_label_source(label)
      return 'nil' unless label
      raise Unsupported unless @text && label.instance_of?(CollationLabel) &&
                               label.name == 'C' && label.table.nil?
      index = C_LABEL_FLAGS.index([label.explicit, label.conflict, label.default, label.none])
      raise Unsupported unless index
      "::JedNative::CompiledExpression::C_LABELS[#{index}]"
    end

    def emit(expression, depth, text_snapshot: false)
      raise Unsupported unless expression.is_a?(Expression)
      raise Unsupported if depth > MAX_DEPTH || @node_count >= MAX_NODES
      raise Unsupported unless TYPES.include?(expression.type) && !@types.key?(expression.type)
      raise Unsupported if expression.type == 'text' && !@text
      raise Unsupported if expression.instance_variable_get(:@native_deep_evaluation) ||
                           expression.instance_variable_get(:@native_iterative_arithmetic)
      label = c_label_source(expression.instance_variable_get(:@native_collation))
      line('# C text specialization') if expression.type == 'text'
      output = "v#{@node_count}"
      @node_count += 1
      case expression.kind
      when :literal
        value = expression.value
        valid = value.nil? || integer?(expression.type) && value.is_a?(Integer) ||
                expression.type == 'boolean' && (value == true || value == false) ||
                expression.type == 'text' && value.instance_of?(String)
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
      if expression.type == 'text' && label != 'nil'
        if text_snapshot && expression.kind == :literal
          # The pool already owns a canonical frozen String. Only the enclosing
          # C comparison can consume it; escaping text still carries its label.
        elsif text_snapshot
          line("if #{output}.instance_of?(::String) || #{output}.instance_of?(::JedNative::CollatedText)")
          @indent += 1
          # Wrapping snapshots bytes, strips singleton methods and replaces any
          # incoming label with this child's C label. Keep the byte copy before
          # the next operand read, without constructing a transient wrapper.
          line("#{output} = ::String.new(#{output})")
          @indent -= 1
          line('else')
          @indent += 1
          line("#{output} = ::JedNative::CollationRules.wrap(#{output}, #{label})")
          @indent -= 1
          line('end')
        else
          line("#{output} = ::JedNative::CollationRules.wrap(#{output}, #{label})")
        end
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
                   expression.type == 'boolean' && (argument.type == 'boolean' || null) ||
                   expression.type == 'text' && (argument.type == 'text' || null)
      raise Unsupported unless compatible
      raise Unsupported if expression.extra&.dig(:type_mod)
      charge
      value = child(argument, depth)
      if expression.type == 'text'
        line("#{output} = engine.cast_value(#{value}, #{pooled('text')}, source_type: #{pooled(argument.type)})")
      else
        line("#{output} = #{value}")
      end
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
      return emit_literal_like(expression, output, left, right, depth) if operator == 'like' || operator == 'not like'
      if COMPARISONS.key?(operator) && [left, right].any? { |argument| argument.type == 'text' }
        raise Unsupported unless expression.type == 'boolean' &&
                                 [left, right].all? { |argument| %w[text null unknown].include?(argument.type) }
        charge
        label = c_label_source(expression.instance_variable_get(:@native_collation))
        # Both children must normalize their own values under static C labels.
        # An unlabeled host String can have singleton comparison methods that
        # observe the other operand's class; leave that path entirely unchanged.
        snapshots = label != 'nil' && [left, right].all? do |argument|
          argument.type == 'text' && %i[literal column param].include?(argument.kind) &&
            argument.instance_variable_get(:@native_collation)
        end
        line('# C text snapshots') if snapshots
        a = emit(left, depth + 1, text_snapshot: snapshots)
        b = emit(right, depth + 1, text_snapshot: snapshots)
        if label == 'nil'
          line("#{output} = compare_text(engine, #{a}, #{b}, #{pooled(operator)}, nil)")
        else
          emit_c_comparison(expression, output, left, right, a, b, label, snapshots: snapshots)
        end
        return
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

    # Only boundary % tokens surround the decoded literal. Interior wildcards,
    # one-character matches and a trailing escape retain the ordinary matcher,
    # including its data-dependent escape error. Check bytes before copying or
    # decoding; admission itself must remain bounded for oversized SQL literals.
    def literal_like_shape(pattern)
      raise Unsupported unless pattern.instance_of?(String) && pattern.bytesize <= MAX_POOL_BYTES - @pool_bytes
      pattern = ::String.new(pattern)
      raise Unsupported unless pattern.encoding == Encoding::UTF_8 && pattern.valid_encoding?
      decoded = +''
      available = MAX_POOL_BYTES - @pool_bytes - pattern.bytesize
      leading = trailing = escaped = false
      pattern.each_char do |character|
        if !escaped && character == '\\'
          escaped = true
          next
        elsif !escaped && character == '%'
          decoded.empty? ? leading = true : trailing = true
          next
        elsif !escaped && character == '_'
          raise Unsupported
        end
        raise Unsupported if trailing || character.bytesize > available
        decoded << character
        available -= character.bytesize
        escaped = false
      end
      raise Unsupported if escaped
      shape = leading ? (trailing ? :contains : :suffix) : (trailing ? :prefix : :exact)
      [shape, decoded]
    end

    def emit_literal_like(expression, output, left, right, depth)
      raise Unsupported unless expression.type == 'boolean' && left.type == 'text' && right.type == 'text' && right.kind == :literal
      labels = [expression, left, right].map { |argument| c_label_source(argument.instance_variable_get(:@native_collation)) }
      raise Unsupported if labels.include?('nil')
      shape, literal = literal_like_shape(right.value)
      line('# literal LIKE specialization')
      charge
      # A C leaf normally wraps its value, stripping singleton methods and
      # replacing transient labels. Preserve that snapshot before reading the
      # pattern. Cast boundaries keep their ordinary wrapping behavior.
      snapshot = %i[literal column param].include?(left.kind)
      a = emit(left, depth + 1, text_snapshot: snapshot)
      b = emit(right, depth + 1, text_snapshot: true)
      payload = pooled(literal)
      line("if #{a}.nil? || #{b}.nil?")
      @indent += 1
      line("#{output} = nil")
      @indent -= 1
      # The primitive methods agree with code-point matching for valid UTF-8.
      # Bytea, UUID and unexpected host encodings retain Functions.like instead
      # of acquiring String encoding errors or different custom method calls.
      line("elsif (#{a}.instance_of?(::String) || #{a}.instance_of?(::JedNative::CollatedText)) && #{a}.encoding == ::Encoding::UTF_8 && #{a}.valid_encoding?")
      @indent += 1
      operation = case shape
      when :exact then "#{a} == #{payload}"
      when :prefix then "#{a}.start_with?(#{payload})"
      when :suffix then "#{a}.end_with?(#{payload})"
      when :contains then "#{a}.include?(#{payload})"
      end
      line("#{output} = #{operation}")
      @indent -= 1
      line('else')
      @indent += 1
      line("#{a} = ::JedNative::CollationRules.wrap(#{a}, #{labels[1]}) if #{a}.instance_of?(::String)") if snapshot
      line("#{b} = ::JedNative::CollationRules.wrap(#{b}, #{labels[2]})")
      line("#{output} = ::JedNative::Functions.like(#{a}, #{b}, context: engine)")
      @indent -= 1
      line('end')
      line("#{output} = !#{output} unless #{output}.nil?") if expression.value == 'not like'
    end

    def emit_c_comparison(expression, output, left, right, a, b, label, snapshots: false)
      line("if #{a}.nil? || #{b}.nil?")
      @indent += 1
      line("#{output} = nil")
      @indent -= 1
      # A statically C-labeled child produced a C wrapper or a private snapshot.
      # An unlabeled child must be an ordinary String: a CollatedText parameter
      # could carry a table, conflicting labels, or another dynamic derivation.
      guards = [[left, a], [right, b]].map do |argument, value|
        klass = argument.type == 'text' && argument.instance_variable_get(:@native_collation) ?
          '::JedNative::CollatedText' : '::String'
        guard = "#{value}.instance_of?(#{klass})"
        snapshots ? "(#{value}.instance_of?(::String) || #{guard})" : guard
      end
      line("elsif #{guards.join(' && ')}")
      @indent += 1
      line("left_length_#{output} = #{a}.length")
      line("right_length_#{output} = #{b}.length")
      line("length_#{output} = left_length_#{output} < right_length_#{output} ? left_length_#{output} : right_length_#{output}")
      line("engine.charge(length_#{output} - 1) if length_#{output} > 1")
      line("#{output} = #{a} #{COMPARISONS.fetch(expression.value)} #{b}")
      @indent -= 1
      line('else')
      @indent += 1
      if snapshots
        # Unexpected values retain the ordinary helper, with the same labels
        # that its wrapped inputs would have had. Never re-read either operand.
        [[left, a], [right, b]].each do |argument, value|
          child_label = c_label_source(argument.instance_variable_get(:@native_collation))
          line("#{value} = ::JedNative::CollationRules.wrap(#{value}, #{child_label}) if #{value}.instance_of?(::String)")
        end
      end
      line("#{output} = compare_text(engine, #{a}, #{b}, #{pooled(expression.value)}, #{label})")
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
