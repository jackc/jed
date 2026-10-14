# frozen_string_literal: true

# The `# params:` directive's value list (spec/design/conformance.md §1.2): the host values a
# record binds as $1..$N. The reference parser for the spelling the three harnesses implement,
# shared by spec/conformance/verify.rb (the static check) and scripts/oracle_import.rb (the
# PostgreSQL replay, where each spelling is also the EXECUTE argument literal).
module CorpusParams
  I64_MIN = -(2**63)
  I64_MAX = 2**63 - 1

  # One bound value: kind is :null, :bool, :int, :decimal, or :text; sql is its spelling as a
  # PostgreSQL literal.
  Value = Struct.new(:kind, :value, :sql)

  # The value list of a `# params:` line, or nil when the line is not that directive. Raises
  # ArgumentError on a malformed list.
  def self.parse_line(line)
    m = line.match(/\A#\s*params:(.*)\z/m) or return nil
    parse(m[1])
  end

  def self.parse(body)
    values = []
    rest = body.strip
    return values if rest.empty?

    loop do
      value, rest = take_value(rest)
      values << value
      rest = rest.lstrip
      return values if rest.empty?
      raise ArgumentError, "expected `,` before #{rest.inspect}" unless rest.start_with?(",")

      rest = rest[1..].lstrip
    end
  end

  def self.take_value(s)
    if (m = s.match(/\A(ARRAY\s*\[|ROW\s*\()/))
      kind, close = m[1].start_with?("ARRAY") ? [:array, "]"] : [:row, ")"]
      rest = s[m[0].length..].lstrip
      items = []
      until rest.start_with?(close)
        raise ArgumentError, "unterminated #{kind} value #{s.inspect}" if rest.empty?

        item, rest = take_value(rest)
        items << item
        rest = rest.lstrip
        next if rest.start_with?(close)
        raise ArgumentError, "expected `,` or `#{close}` before #{rest.inspect}" unless rest.start_with?(",")

        rest = rest[1..].lstrip
      end
      raise ArgumentError, "ROW() needs at least one field" if kind == :row && items.empty?

      open = kind == :array ? "ARRAY[" : "ROW("
      return [Value.new(kind, items, "#{open}#{items.map(&:sql).join(', ')}#{close}"), rest[1..]]
    end
    if s.start_with?("'")
      text = +""
      i = 1
      loop do
        raise ArgumentError, "unterminated text value #{s.inspect}" if i >= s.length

        if s[i] == "'"
          if s[i + 1] == "'"
            text << "'"
            i += 2
            next
          end
          return [Value.new(:text, text, s[0..i]), s[(i + 1)..]]
        end
        text << s[i]
        i += 1
      end
    end
    # A scalar token runs to the next separator or the close of an enclosing ARRAY[…] / ROW(…).
    token = s[/\A[^,\])]*/]
    rest = s[token.length..]
    token = token.strip
    value =
      case token
      when "NULL" then Value.new(:null, nil, token)
      when "true", "false" then Value.new(:bool, token == "true", token)
      when /\A-?\d+\z/
        n = Integer(token, 10)
        raise ArgumentError, "integer #{token} is outside i64" unless n.between?(I64_MIN, I64_MAX)

        Value.new(:int, n, token)
      when /\A-?\d+\.\d+\z/ then Value.new(:decimal, token, token)
      else raise ArgumentError, "unrecognized value #{token.inspect}"
      end
    [value, rest]
  end
  private_class_method :take_value
end
