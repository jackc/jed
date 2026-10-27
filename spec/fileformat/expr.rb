# A small evaluator for the canonical expression text the catalog stores (index key expressions and
# partial-index predicates — indexes.md §1/§9), so the reference can derive expression and partial
# index entries without SQL (integrity-check.md §6). It covers a deliberately small subset; anything
# outside it raises StoredExpr::Unsupported, and the caller treats the object as unchecked rather than
# guessing. Its results must match the cores' evaluation wherever it answers:
#
#   column references; integer, 'text', true/false/null literals; unary -, + - * over i16/i32/i64
#   (result the wider operand type, an integer literal i32 when it fits else i64, overflow 22003);
#   || over text; = <> != < <= > >= over integers, C-collation text, booleans; AND/OR/NOT (3VL);
#   IS [NOT] NULL; lower()/upper() over ASCII text.
#
# Canonical text is space-separated tokens (`lower ( email )`, `status = 'active'`).

module StoredExpr
  # The text uses something outside the subset: the reference does not know its value.
  class Unsupported < StandardError; end

  # Evaluation raised, as a core's would (22003 integer overflow).
  class EvalError < StandardError
    attr_reader :code

    def initialize(code, message)
      @code = code
      super(message)
    end
  end

  INT_RANGE = { "i16" => -(2**15)...(2**15), "i32" => -(2**31)...(2**31), "i64" => -(2**63)...(2**63) }.freeze
  INT_RANK = { "i16" => 0, "i32" => 1, "i64" => 2 }.freeze
  KEYWORDS = %w[and or not is null true false].freeze

  module_function

  def tokenize(text)
    toks = []
    s = text
    i = 0
    while i < s.size
      c = s[i]
      if c =~ /\s/
        i += 1
      elsif c == "'"
        j = i + 1
        buf = +""
        loop do
          raise Unsupported, "unterminated string" if j >= s.size

          if s[j] == "'"
            if s[j + 1] == "'"
              buf << "'"
              j += 2
              next
            end
            break
          end
          buf << s[j]
          j += 1
        end
        toks << [:str, buf]
        i = j + 1
      elsif c =~ /[0-9]/
        j = i
        j += 1 while j < s.size && s[j] =~ /[0-9]/
        raise Unsupported, "non-integer numeric literal" if j < s.size && s[j] =~ /[.eE_a-zA-Z]/

        toks << [:int, s[i...j].to_i]
        i = j
      elsif c =~ /[a-z_]/
        j = i
        j += 1 while j < s.size && s[j] =~ /[a-z0-9_]/
        word = s[i...j]
        toks << (KEYWORDS.include?(word) ? [:kw, word] : [:ident, word])
        i = j
      else
        op = %w[<> != <= >= || :: = < > + - * / % ( ) ,].find { |o| s[i, o.size] == o }
        raise Unsupported, "unsupported character #{c.inspect}" if op.nil?

        toks << [:op, op]
        i += op.size
      end
    end
    toks
  end

  # Parse to an AST: [:col, name] [:lit, type, value] [:neg, e] [:bin, op, l, r] [:and, l, r]
  # [:or, l, r] [:not, e] [:isnull, e, negated] [:call, name, args].
  def parse(text)
    p = Parser.new(tokenize(text))
    ast = p.or_expr
    raise Unsupported, "trailing tokens" unless p.done?

    ast
  end

  class Parser
    def initialize(toks)
      @t = toks
      @i = 0
    end

    def done? = @i == @t.size
    def peek = @t[@i]

    def accept(kind, val = nil)
      tok = @t[@i]
      return nil unless tok && tok[0] == kind && (val.nil? || tok[1] == val)

      @i += 1
      tok
    end

    def expect(kind, val)
      accept(kind, val) or raise Unsupported, "expected #{val}"
    end

    def or_expr
      l = and_expr
      l = [:or, l, and_expr] while accept(:kw, "or")
      l
    end

    def and_expr
      l = not_expr
      l = [:and, l, not_expr] while accept(:kw, "and")
      l
    end

    def not_expr
      return [:not, not_expr] if accept(:kw, "not")

      is_expr
    end

    def is_expr
      e = cmp_expr
      while accept(:kw, "is")
        negated = !accept(:kw, "not").nil?
        expect(:kw, "null")
        e = [:isnull, e, negated]
      end
      e
    end

    def cmp_expr
      l = concat_expr
      %w[= <> != < <= > >=].each do |op|
        return [:bin, op == "!=" ? "<>" : op, l, concat_expr] if accept(:op, op)
      end
      l
    end

    def concat_expr
      l = add_expr
      l = [:bin, "||", l, add_expr] while accept(:op, "||")
      l
    end

    def add_expr
      l = mul_expr
      loop do
        if accept(:op, "+") then l = [:bin, "+", l, mul_expr]
        elsif accept(:op, "-") then l = [:bin, "-", l, mul_expr]
        else return l
        end
      end
    end

    def mul_expr
      l = unary
      l = [:bin, "*", l, unary] while accept(:op, "*")
      raise Unsupported, "division" if peek && peek[0] == :op && %w[/ % ::].include?(peek[1])

      l
    end

    def unary
      return [:neg, unary] if accept(:op, "-")

      primary
    end

    def primary
      tok = peek or raise Unsupported, "unexpected end"
      @i += 1
      case tok[0]
      when :int then [:lit, INT_RANGE["i32"].cover?(tok[1]) ? "i32" : "i64", tok[1]]
      when :str then [:lit, "text", tok[1]]
      when :kw
        case tok[1]
        when "true" then [:lit, "boolean", true]
        when "false" then [:lit, "boolean", false]
        when "null" then [:lit, nil, nil]
        else raise Unsupported, "unexpected #{tok[1]}"
        end
      when :ident
        if accept(:op, "(")
          args = []
          unless accept(:op, ")")
            args << or_expr
            args << or_expr while accept(:op, ",")
            expect(:op, ")")
          end
          [:call, tok[1], args]
        else
          [:col, tok[1]]
        end
      when :op
        raise Unsupported, "unexpected #{tok[1]}" unless tok[1] == "("

        e = or_expr
        expect(:op, ")")
        e
      end
    end
  end

  # Evaluate `ast` over a row: `columns` the table's column defs, `row` its decoded values. Returns
  # [type, value] (value nil for NULL; type nil for an untyped NULL literal).
  def value_of(ast, columns, row)
    case ast[0]
    when :lit then [ast[1], ast[2]]
    when :col
      ci = columns.index { |c| c[:name] == ast[1] } or raise Unsupported, "unknown column #{ast[1]}"
      c = columns[ci]
      raise Unsupported, "collated column" if c[:collation] && c[:collation] != "C"

      [c[:type], row[ci]]
    when :neg
      t, v = value_of(ast[1], columns, row)
      raise Unsupported, "negation of #{t}" unless INT_RANK.key?(t)

      [t, v.nil? ? nil : checked(t, -v)]
    when :bin then binary(ast[1], value_of(ast[2], columns, row), value_of(ast[3], columns, row))
    when :and, :or
      l = truth(value_of(ast[1], columns, row))
      r = truth(value_of(ast[2], columns, row))
      ["boolean", ast[0] == :and ? and3(l, r) : or3(l, r)]
    when :not
      v = truth(value_of(ast[1], columns, row))
      ["boolean", v.nil? ? nil : !v]
    when :isnull
      _t, v = value_of(ast[1], columns, row)
      ["boolean", v.nil? != ast[2]]
    when :call then call(ast[1], ast[2].map { |a| value_of(a, columns, row) })
    end
  end

  # A filter (a partial-index predicate, indexes.md §9): true only when the predicate is TRUE. Its
  # top-level AND chain stops at the first conjunct that is not TRUE.
  def filter?(ast, columns, row)
    return filter?(ast[1], columns, row) && filter?(ast[2], columns, row) if ast[0] == :and

    truth(value_of(ast, columns, row)) == true
  end

  def truth(tv)
    t, v = tv
    raise Unsupported, "#{t} is not boolean" unless t.nil? || t == "boolean"

    v
  end

  def and3(l, r)
    return false if l == false || r == false
    return nil if l.nil? || r.nil?

    true
  end

  def or3(l, r)
    return true if l == true || r == true
    return nil if l.nil? || r.nil?

    false
  end

  def checked(type, v)
    raise EvalError.new("22003", "#{type} out of range") unless INT_RANGE.fetch(type).cover?(v)

    v
  end

  def binary(op, (lt, lv), (rt, rv))
    case op
    when "+", "-", "*"
      raise Unsupported, "#{op} over #{lt}, #{rt}" unless INT_RANK.key?(lt) && INT_RANK.key?(rt)

      t = INT_RANK[lt] >= INT_RANK[rt] ? lt : rt
      return [t, nil] if lv.nil? || rv.nil?

      [t, checked(t, lv.send(op, rv))]
    when "||"
      raise Unsupported, "|| over #{lt}, #{rt}" unless [lt, rt].all? { |t| t.nil? || t == "text" }
      return ["text", nil] if lv.nil? || rv.nil?

      ["text", lv + rv]
    else
      comparable = (INT_RANK.key?(lt) && INT_RANK.key?(rt)) || (lt == rt && %w[text boolean].include?(lt))
      raise Unsupported, "#{op} over #{lt}, #{rt}" unless comparable
      return ["boolean", nil] if lv.nil? || rv.nil?

      c = lt == "text" ? (lv.b <=> rv.b) : (lt == "boolean" ? ((lv ? 1 : 0) <=> (rv ? 1 : 0)) : (lv <=> rv))
      ["boolean", { "=" => c.zero?, "<>" => !c.zero?, "<" => c.negative?, "<=" => c <= 0,
                    ">" => c.positive?, ">=" => c >= 0 }.fetch(op)]
    end
  end

  def call(name, args)
    raise Unsupported, "function #{name}" unless %w[lower upper].include?(name) && args.size == 1

    t, v = args[0]
    raise Unsupported, "#{name}(#{t})" unless t == "text"
    return ["text", nil] if v.nil?
    raise Unsupported, "non-ASCII case mapping" unless v.ascii_only?

    ["text", name == "lower" ? v.downcase : v.upcase]
  end
end
