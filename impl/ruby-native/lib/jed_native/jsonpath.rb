# frozen_string_literal: true

require_relative 'jsonpath_lexer'
require_relative 'regex'

module JedNative
  class JsonPathValue
    attr_reader :root, :strict, :source
    def initialize(root, strict, source)
      @root, @strict, @source = root, strict, source
      freeze
    end
    def sql_type = 'jsonpath'
    def to_s = @source
    def self.parse(value)
      return value if value.is_a?(self)
      raise Error.new('42804', 'jsonpath input must be text') unless value.is_a?(String)
      JsonPathParser.new(value).parse
    end
  end

  class JsonPathParser
    PRIORITY = { '||' => 1, '&&' => 2, '==' => 3, '!=' => 3, '<>' => 3, '<' => 3, '<=' => 3, '>' => 3, '>=' => 3, '+' => 4, '-' => 4, '*' => 5, '/' => 5, '%' => 5 }.freeze
    METHODS = %w[type size double ceiling floor abs keyvalue bigint boolean integer number string decimal datetime date time time_tz timestamp timestamp_tz].freeze
    PREDICATES = %i[comparison connective not exists unknown regex starts].freeze
    def initialize(source)
      @tokens, @index, @depth, @filter_depth, @subscript_depth = JsonPathLexer.new(source).tokens, 0, 0, 0, 0
    end
    def parse(canonical: false)
      raise Error.new('22P02', 'invalid input syntax for type jsonpath') if at?(:eof)
      strict = false
      if keyword?('strict') || keyword?('lax')
        strict = keyword?('strict')
        advance
      end
      root = expression
      error unless at?(:eof)
      source = (strict ? 'strict ' : '') + render(root, top: true)
      JsonPathParser.new(source).parse(canonical: true) unless canonical
      JsonPathValue.new(root, strict, source)
    end
    private
    def deepen
      @depth += 1
      raise Error.new('54001', 'JSON path nesting limit exceeded') if @depth > 256
    end
    def nested
      deepen
      value = yield
      @depth -= 1
      value
    end
    def error = raise(Error.new('42601', 'invalid JSON path syntax'))
    def token = @tokens[@index]
    def at?(kind) = token[0] == kind
    def keyword?(word) = token[0] == :name && token[1].downcase == word
    def advance
      result = token
      @index += 1
      result
    end
    def accept(kind)
      return false unless at?(kind)
      advance
      true
    end
    def expect(kind)
      error unless accept(kind)
    end
    def word(word)
      error unless keyword?(word)
      advance
    end
    def predicate?(node) = PREDICATES.include?(node[:kind])
    def expression(minimum = 0)
      stack_depth = @parse_stack_depth || 0
      if stack_depth >= 24
        @parse_stack_depth = 0
        return Fiber.new { expression(minimum) }.resume
      end
      @parse_stack_depth = stack_depth + 1
      expression_body(minimum)
    ensure
      @parse_stack_depth = stack_depth
    end
    def expression_body(minimum)
      previous_depth = @depth
      left = prefix
      loop do
        if minimum <= 3 && keyword?('is')
          error unless left[:parenthesized] && predicate?(left)
          advance
          word('unknown')
          left = { kind: :unknown, expr: left }
          next
        end
        if minimum <= 3 && keyword?('like_regex')
          error if predicate?(left)
          advance
          error unless at?(:string)
          pattern = advance[1]
          flags = ''
          if keyword?('flag')
            advance
            error unless at?(:string)
            flags = advance[1]
          end
          error unless flags.each_char.all? { |char| 'ismxq'.include?(char) }
          raise Error.new('0A000', 'expanded JSON path regular expressions are not supported') if flags.include?('x') && !flags.include?('q')
          canonical_flags = 'ismxq'.chars.select { |char| flags.include?(char) }.join
          program = RegexProgram.new(pattern, insensitive: flags.include?('i'), dotall: flags.include?('s'), multiline: flags.include?('m'), literal: flags.include?('q'), newline_sensitive: !flags.include?('s'))
          left = { kind: :regex, expr: left, pattern: pattern, flags: canonical_flags, program: program }
          next
        end
        if minimum <= 3 && keyword?('starts')
          error if predicate?(left)
          advance
          word('with')
          error unless at?(:string) || at?(:variable)
          right = primary
          left = { kind: :starts, left: left, right: right }
          next
        end
        priority = PRIORITY[token[0]]
        break unless priority && priority >= minimum
        op = advance[0]
        deepen if priority != 3
        right = expression(priority + 1)
        kind = priority <= 2 ? :connective : priority == 3 ? :comparison : :arithmetic
        if kind == :connective
          error unless predicate?(left) && predicate?(right)
        else
          error if predicate?(left) || predicate?(right)
        end
        left = { kind: kind, op: op == '<>' ? '!=' : op, left: left, right: right }
      end
      left
    ensure
      @depth = previous_depth
    end
    def prefix
      if at?('+') || at?('-')
        op = advance[0]
        value = nested { expression(6) }
        error if predicate?(value)
        return { kind: :literal, value: op == '-' ? -value[:value] : value[:value] } if value[:kind] == :literal && value[:value].is_a?(Numeric)
        return { kind: :unary, op: op, expr: value }
      end
      if accept('!')
        error unless at?('(') || keyword?('exists')
        value = prefix
        error unless predicate?(value)
        return { kind: :not, expr: value }
      end
      value = if accept('(')
        inside = nested { expression }
        expect(')')
        inside.merge(parenthesized: true)
      elsif keyword?('exists')
        advance
        expect('(')
        inside = nested { expression }
        error if predicate?(inside)
        expect(')')
        { kind: :exists, expr: inside }
      else
        primary
      end
      steps = []
      loop do
        if accept('.')
          if accept('*')
            steps << { kind: :members }
          elsif accept('**')
            low, high = 0, :last
            if accept('{')
              low = level
              high = if keyword?('to') then advance; level else low end
              expect('}')
            end
            steps << { kind: :descendants, low: low, high: high }
          else
            error unless at?(:name) || at?(:string)
            quoted_name = at?(:string)
            name = advance[1]
            if accept('(')
              error if quoted_name
              name = name.downcase
              error unless METHODS.include?(name)
              args = []
              unless at?(')')
                loop do
                  if at?(:string)
                    args << advance[1]
                  else
                    sign = accept('-') ? -1 : (accept('+'); 1)
                    error unless at?(:number)
                    numeric_token = advance
                    numeric = numeric_token[1]
                    error unless numeric_token[2].match?(/\A(?:0|[1-9][0-9]*)\z/)
                    args << numeric.coefficient * sign
                  end
                  break unless accept(',')
                end
              end
              expect(')')
              allowed = case name
              when 'decimal' then args.length <= 2 && args.all? { |arg| arg.is_a?(Integer) }
              when 'datetime' then args.length <= 1 && args.all? { |arg| arg.is_a?(String) }
              when 'time', 'time_tz', 'timestamp', 'timestamp_tz' then args.length <= 1 && args.all? { |arg| arg.is_a?(Integer) && arg >= 0 }
              else args.empty?
              end
              error unless allowed
              steps << { kind: :method, name: name, args: args }
            else
              steps << { kind: :member, name: name }
            end
          end
        elsif accept('[')
          @subscript_depth += 1
          deepen
          if accept('*')
            step = { kind: :elements }
          else
            bounds = []
            loop do
              low = expression
              error if predicate?(low)
              high = nil
              if keyword?('to')
                advance
                high = expression
                error if predicate?(high)
              end
              bounds << [low, high]
              break unless accept(',')
            end
            step = { kind: :subscript, bounds: bounds }
          end
          expect(']')
          @subscript_depth -= 1
          @depth -= 1
          steps << step
        elsif accept('?')
          expect('(')
          deepen
          @filter_depth += 1
          condition = expression
          @filter_depth -= 1
          @depth -= 1
          error unless predicate?(condition)
          expect(')')
          steps << { kind: :filter, expr: condition }
        else
          break
        end
      end
      error if !steps.empty? && predicate?(value) && !value[:parenthesized]
      steps.empty? ? value : { kind: :path, base: value, steps: steps }
    end
    def primary
      return { kind: :root } if accept('$')
      if accept('@')
        error if @filter_depth.zero?
        return { kind: :current }
      end
      return { kind: :variable, name: advance[1] } if at?(:variable)
      return { kind: :literal, value: advance[1] } if at?(:number) || at?(:string)
      if at?(:name) && %w[true false null].include?(token[1])
        return { kind: :literal, value: { 'true' => true, 'false' => false, 'null' => nil }[advance[1]] }
      end
      if keyword?('last')
        error if @subscript_depth.zero?
        advance
        return { kind: :last }
      end
      error
    end
    def level
      if keyword?('last')
        advance
        return :last
      end
      error unless at?(:number)
      numeric_token = advance
      number = numeric_token[1]
      error unless numeric_token[2].match?(/\A(?:0|[1-9][0-9]*)\z/)
      raise Error.new('22003', 'JSON path level is out of range') if number.coefficient > 2_147_483_647
      number.coefficient
    end
    def render(node, parent: 0, top: false)
      stack_depth = @render_stack_depth || 0
      if stack_depth >= 24
        @render_stack_depth = 0
        return Fiber.new { render(node, parent: parent, top: top) }.resume
      end
      @render_stack_depth = stack_depth + 1
      render_body(node, parent: parent, top: top)
    ensure
      @render_stack_depth = stack_depth
    end
    def render_body(node, parent:, top:)
      kind = node[:kind]
      case kind
      when :root then '$'
      when :current then '@'
      when :variable then '$' + JsonValue.escape(node[:name])
      when :last then 'last'
      when :literal then JsonValue.render(node[:value], spaces: true)
      when :path
        base = render(node[:base], top: false)
        if predicate?(node[:base]) || %i[arithmetic unary].include?(node[:base][:kind]) || node[:base][:kind] == :literal && node[:base][:value].is_a?(Numeric)
          base = '(' + base + ')'
        end
        base + node[:steps].map { |step| render_step(step) }.join
      when :arithmetic, :comparison, :connective
        priority = PRIORITY.fetch(node[:op])
        text = render(node[:left], parent: priority) + " #{node[:op]} " + render(node[:right], parent: priority)
        top || priority <= parent ? '(' + text + ')' : text
      when :unary
        text = node[:op] + render(node[:expr], parent: 6)
        top || parent >= 6 ? '(' + text + ')' : text
      when :not then '!(' + render(node[:expr]) + ')'
      when :exists then 'exists (' + render(node[:expr]) + ')'
      when :unknown then '(' + render(node[:expr]) + ') is unknown'
      when :starts
        text = render(node[:left], parent: 3) + ' starts with ' + render(node[:right], parent: 3)
        top || parent >= 3 ? '(' + text + ')' : text
      when :regex
        operand = render(node[:expr])
        operand = '(' + operand + ')' if %i[arithmetic unary].include?(node[:expr][:kind])
        text = operand + ' like_regex ' + JsonValue.escape(node[:pattern])
        text += ' flag ' + JsonValue.escape(node[:flags]) unless node[:flags].empty?
        top || parent >= 3 ? '(' + text + ')' : text
      end
    end
    def render_step(step)
      case step[:kind]
      when :member then '.' + JsonValue.escape(step[:name])
      when :members then '.*'
      when :elements then '[*]'
      when :subscript
        '[' + step[:bounds].map { |low, high| render(low) + (high ? ' to ' + render(high) : '') }.join(',') + ']'
      when :method
        '.' + step[:name] + '(' + step[:args].map { |arg| arg.is_a?(String) ? JsonValue.escape(arg) : arg.to_s }.join(',') + ')'
      when :filter then '?(' + render(step[:expr]) + ')'
      when :descendants
        low, high = step.values_at(:low, :high)
        return '.**' if low == 0 && high == :last
        '.**{' + low.to_s + (low == high ? '' : ' to ' + high.to_s) + '}'
      end
    end
  end
end
