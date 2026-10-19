# frozen_string_literal: true

module JedNative
  # JSON numbers stay exact decimals. This parser and renderer do not use a
  # native JSON extension and never convert a JSON number through Float.
  class JsonValue
    include Comparable
    attr_reader :value, :binary, :source
    def initialize(value, binary: true, source: nil)
      self.class.validate_depth(value)
      @value, @binary, @source = value, binary, source
      freeze
    end
    def sql_type = @binary ? 'jsonb' : 'json'
    def self.validate_depth(value)
      pending = [[value, 0]]
      until pending.empty?
        node, depth = pending.pop
        next unless node.is_a?(Hash) || node.is_a?(Array)
        depth += 1
        raise Error.new('54001', 'JSON nesting limit exceeded') if depth > 256
        children = node.is_a?(Hash) ? node.values : node
        children.each { |child| pending << [child, depth] }
      end
    end
    def self.parse(text, binary: false)
      text = text.to_s if text.is_a?(self)
      raise Error.new('22P02', 'invalid input syntax for JSON') unless text.is_a?(String)
      parser = JsonParser.new(text, unicode: binary)
      new(parser.parse, binary: binary, source: binary ? nil : text)
    end
    def self.from_ruby(value, binary: true)
      return new(value.value, binary: binary) if value.is_a?(self)
      data = case value
             when nil, true, false, Integer, Decimal, String then value
             when Float then value.finite? ? Decimal.parse(value.to_s) : Types.format(value)
             when CompositeValue
               value.each_with_index.to_h { |item, index| [(value.field_names || [])[index] || "f#{index + 1}", from_ruby(item).value] }
             when Array then value.map { |v| from_ruby(v).value }
             when Hash then value.to_h { |k, v| [k.to_s, from_ruby(v).value] }
             else Types.format(value)
             end
      new(data, binary: binary)
    end
    def self.escape(text)
      '"' + text.gsub(/["\\\x00-\x1f]/) do |char|
        case char
        when '"' then '\\"'
        when '\\' then '\\\\'
        when "\b" then '\\b'
        when "\f" then '\\f'
        when "\n" then '\\n'
        when "\r" then '\\r'
        when "\t" then '\\t'
        else format('\\u%04x', char.ord)
        end
      end + '"'
    end
    def self.render(value, spaces: true, pretty: false, level: 0)
      case value
      when nil then 'null'
      when true then 'true'
      when false then 'false'
      when String then escape(value)
      when Integer, Decimal then value.to_s
      when Array
        return pretty ? "[\n#{' ' * (4 * level)}]" : '[]' if value.empty?
        children = value.map { |v| render(v, spaces: spaces, pretty: pretty, level: level + 1) }
        return "[\n#{' ' * (4 * (level + 1))}#{children.join(",\n" + ' ' * (4 * (level + 1)))}\n#{' ' * (4 * level)}]" if pretty
        '[' + children.join(spaces ? ', ' : ',') + ']'
      when Hash
        return '{}' if value.empty?
        keys = value.keys.sort_by { |k| [k.bytesize, k.b] }
        children = keys.map { |k| escape(k) + (spaces ? ': ' : ':') + render(value[k], spaces: spaces, pretty: pretty, level: level + 1) }
        return "{\n#{' ' * (4 * (level + 1))}#{children.join(",\n" + ' ' * (4 * (level + 1)))}\n#{' ' * (4 * level)}}" if pretty
        '{' + children.join(spaces ? ', ' : ',') + '}'
      else raise Error.new('42804', 'unsupported JSON value')
      end
    end
    def to_s = @source || self.class.render(@value, spaces: @binary)
    def tree
      parser = JsonParser.new(to_s, unicode: false, spans: true)
      parser.parse
      parser.last_tree
    end
    def decoded_value = @binary ? @value : JsonParser.new(to_s).parse
    def self.payload(value)
      case value
      when String then value.bytesize
      when Integer, Decimal
        number = Decimal.parse(value)
        return 0 if number.zero?
        digits = number.coefficient.abs.to_s
        high = (digits.length - number.scale - 1).div(4)
        low = (digits.length - digits.sub(/0+\z/, '').length - number.scale).div(4)
        4 * (high - low + 1)
      when Array then value.sum { |item| 32 + payload(item) }
      when Hash then value.sum { |key, item| key.bytesize + 32 + payload(item) }
      else 0
      end
    end
    def pretty = self.class.render(@value, pretty: true)
    def text = @value.nil? ? nil : @value.is_a?(String) ? @value : self.class.render(@value)
    def type_name
      case @value
      when nil then 'null'
      when true, false then 'boolean'
      when Numeric then 'number'
      when String then 'string'
      when Array then 'array'
      when Hash then 'object'
      end
    end
    def <=>(other)
      return nil unless other.is_a?(JsonValue)
      self.class.compare_nodes(@value, other.value)
    end
    def self.node_rank(value)
      case value
      when nil then 0
      when String then 1
      when Numeric then 2
      when true, false then 3
      when Array then 4
      when Hash then 5
      end
    end
    def self.compare_nodes(a, b)
      rank = node_rank(a) <=> node_rank(b)
      return rank unless rank.zero?
      case a
      when nil then 0
      when Hash
        cmp = a.length <=> b.length
        return cmp unless cmp.zero?
        ak = a.keys.sort_by { |k| [k.bytesize, k.b] }
        bk = b.keys.sort_by { |k| [k.bytesize, k.b] }
        ak.each_index do |i|
          cmp = (ak[i] <=> bk[i])
          return cmp unless cmp.zero?
          cmp = compare_nodes(a[ak[i]], b[bk[i]])
          return cmp unless cmp.zero?
        end
        0
      when Array
        cmp = a.length <=> b.length
        return cmp unless cmp.zero?
        a.each_index do |i|
          cmp = compare_nodes(a[i], b[i])
          return cmp unless cmp.zero?
        end
        0
      else Types.compare(a, b)
      end
    end
    def self.contains_node?(a, b, top: true, depth: 0)
      # A legal 256-level JSON value must also fit while evaluating on a
      # cursor's smaller VM stack. Containment cannot yield a query row; a
      # fresh stack preserves its short-circuit order without growing Ruby's
      # process-wide Fiber stack setting or changing the JSON depth limit.
      return Fiber.new { contains_node?(a, b, top: top) }.resume if depth >= 24
      if a.is_a?(Hash) && b.is_a?(Hash)
        b.all? { |k, v| a.key?(k) && node_rank(a[k]) == node_rank(v) && contains_node?(a[k], v, top: false, depth: depth + 1) }
      elsif a.is_a?(Array)
        if b.is_a?(Array) then b.all? { |v| a.any? { |u| node_rank(u) == node_rank(v) && contains_node?(u, v, top: false, depth: depth + 1) } }
        else top && a.any? { |u| !u.is_a?(Array) && !u.is_a?(Hash) && compare_nodes(u, b).zero? }
        end
      else compare_nodes(a, b).zero?
      end
    end
    def contains?(other) = self.class.contains_node?(@value, other.is_a?(JsonValue) ? other.value : other)
    def concat_json(other)
      rhs = other.value
      result = @value.is_a?(Hash) && rhs.is_a?(Hash) ? @value.merge(rhs) : (@value.is_a?(Array) ? @value : [@value]) + (rhs.is_a?(Array) ? rhs : [rhs])
      self.class.new(result)
    end
    def operator(op, key)
      return verbatim_operator(op, key) if !@binary && %w[-> ->> #> #>>].include?(op)
      case op
      when '?', '?|', '?&'
        exists = ->(k) { @value.is_a?(Hash) ? @value.key?(k) : @value.is_a?(Array) ? @value.include?(k) : @value.is_a?(String) && @value == k }
        return exists.call(key) if op == '?'
        return op == '?|' ? key.compact.any?(&exists) : key.compact.all?(&exists)
      when '->', '->>'
        node = @value
        if node.is_a?(Hash) && key.is_a?(String)
          return nil unless node.key?(key)
          node = node[key]
        elsif node.is_a?(Array) && key.is_a?(Integer)
          return nil unless key >= -node.length && key < node.length
          node = node[key]
        else return nil
        end
      when '#>', '#>>'
        node = @value
        key.each do |part|
          return nil if part.nil?
          if node.is_a?(Hash)
            return nil unless node.key?(part)
            node = node[part]
          elsif node.is_a?(Array) && (index = self.class.path_index(part))
            return nil unless index >= -node.length && index < node.length
            node = node[index]
          else return nil
          end
        end
      else raise Error.new('0A000', "JSON operator #{op} not implemented")
      end
      wrapped = self.class.new(node, binary: @binary)
      op.end_with?('>>') ? wrapped.text : wrapped
    end
    def eql?(other) = other.is_a?(JsonValue) && self == other
    def hash = self.class.render(@value).hash
    def self.path_index(text)
      return nil unless text.is_a?(String) && /\A[ \t\r\n\v\f]*[+-]?\d+\z/.match?(text)
      number = text.to_i
      number.between?(-2_147_483_648, 2_147_483_647) ? number : nil
    end
  end

  class JsonParser
    attr_reader :last_tree
    def initialize(text, unicode: true, spans: false)
      @text, @index, @depth = text, 0, 0
      @unicode, @spans = unicode, spans
    end
    def parse
      value = node
      whitespace
      error unless @index == @text.length
      value
    end
    def error = raise(Error.new('22P02', 'invalid input syntax for JSON'))
    def whitespace
      @index += 1 while @index < @text.length && " \t\n\r".include?(@text[@index])
    end
    def node
      whitespace
      start = @index
      @depth += 1
      raise Error.new('54001', 'JSON nesting limit exceeded') if @depth > 257
      char = @text[@index]
      raise Error.new('54001', 'JSON nesting limit exceeded') if @depth > 256 && (char == '[' || char == '{')
      children = []
      value = case char
              when '"' then string
              when '['
                @index += 1
                list = []
                whitespace
                unless @text[@index] == ']'
                  loop do
                    list << node
                    children << @last_tree if @spans
                    whitespace
                    break unless @text[@index] == ','
                    @index += 1
                  end
                end
                error unless @text[@index] == ']'
                @index += 1
                list
              when '{'
                @index += 1
                object = {}
                whitespace
                unless @text[@index] == '}'
                  loop do
                    whitespace
                    error unless @text[@index] == '"'
                    key_start = @index
                    key = string
                    key_finish = @index
                    whitespace
                    error unless @text[@index] == ':'
                    @index += 1
                    object[key] = node
                    children << [key, @last_tree, key_start, key_finish] if @spans
                    whitespace
                    break unless @text[@index] == ','
                    @index += 1
                  end
                end
                error unless @text[@index] == '}'
                @index += 1
                object
              when 't' then literal('true', true)
              when 'f' then literal('false', false)
              when 'n' then literal('null', nil)
              else
                m = /\A-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?/.match(@text[@index..])
                error unless m
                @index += m[0].length
                Decimal.parse(m[0])
              end
      @depth -= 1
      @last_tree = { start: start, finish: @index, value: value, children: children, kind: char == '{' ? :object : char == '[' ? :array : :scalar } if @spans
      value
    end
    def literal(word, value)
      error unless @text[@index, word.length] == word
      @index += word.length
      value
    end
    def string
      @index += 1
      output = +''
      loop do
        error if @index >= @text.length
        char = @text[@index]
        @index += 1
        return output if char == '"'
        if char == '\\'
          escaped = @text[@index]
          @index += 1
          case escaped
          when '"', '\\', '/' then output << escaped
          when 'b' then output << "\b"
          when 'f' then output << "\f"
          when 'n' then output << "\n"
          when 'r' then output << "\r"
          when 't' then output << "\t"
          when 'u'
            hex = @text[@index, 4]
            error unless hex && /\A[0-9a-fA-F]{4}\z/.match?(hex)
            code = hex.to_i(16)
            @index += 4
            unless @unicode
              output << (code.between?(0xd800, 0xdfff) ? "\\u#{hex}" : code.chr(Encoding::UTF_8))
              next
            end
            if (0xd800..0xdbff).cover?(code)
              error unless @text[@index, 2] == '\\u'
              low = @text[@index + 2, 4]
              error unless low && /\A[dD][c-fC-F][0-9a-fA-F]{2}\z/.match?(low)
              code = 0x10000 + (code - 0xd800) * 1024 + low.to_i(16) - 0xdc00
              @index += 6
            end
            error if (0xd800..0xdfff).cover?(code)
            raise Error.new('22P05', 'unsupported Unicode escape sequence') if code.zero?
            output << code.chr(Encoding::UTF_8)
          else error
          end
        else
          error if char.ord < 32
          output << char
        end
      end
    end
  end
end
