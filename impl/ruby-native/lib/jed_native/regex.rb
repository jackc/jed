# frozen_string_literal: true

module JedNative
  # Character indexing without one heap String per input character. ASCII has
  # no index allocation; UTF-8 uses four packed bytes per character boundary.
  class RegexText
    include Enumerable
    ASCII = Array.new(128) { |value| value.chr.freeze }.freeze
    attr_reader :length

    def initialize(text)
      @text = text
      @insensitive = false
      if text.ascii_only?
        @length = text.bytesize
      else
        @offsets = String.new(capacity: text.bytesize + 4, encoding: Encoding::BINARY)
        cursor = 0
        while cursor < text.bytesize
          @offsets << [cursor].pack('L<')
          byte = text.getbyte(cursor)
          cursor += byte < 128 ? 1 : byte < 224 ? 2 : byte < 240 ? 3 : 4
        end
        @length = @offsets.bytesize / 4
        @offsets << [cursor].pack('L<')
      end
    end

    def with_case(insensitive)
      copy = dup
      copy.instance_variable_set(:@insensitive, insensitive)
      copy
    end

    def byte_offset(index)
      @offsets ? @offsets.unpack1('L<', offset: index * 4) : index
    end

    def [](index)
      return nil if index.negative? || index >= @length
      offset = byte_offset(index)
      byte = @text.getbyte(offset)
      if byte < 128
        byte += 32 if @insensitive && byte.between?(65, 90)
        ASCII[byte]
      else
        @text.byteslice(offset, byte_offset(index + 1) - offset)
      end
    end

    def slice(first, last)
      offset = byte_offset(first)
      @text.byteslice(offset, byte_offset(last) - offset)
    end

    def each
      return enum_for(:each) unless block_given?
      index = 0
      while index < @length
        yield self[index]
        index += 1
      end
    end
  end

  # The shared regex grammar lowers to the specified Pike VM. No SQL pattern is
  # passed to Ruby's backtracking regexp engine. Each input position visits an
  # instruction at most once, bounding work by program length times input length.
  class RegexProgram
    LIMIT = 32_768
    MAX_CHAR = 0x10ffff
    PREDEFINED = {
      'd' => [[48, 57]],
      'w' => [[48, 57], [65, 90], [95, 95], [97, 122]],
      's' => [[9, 13], [32, 32]]
    }.freeze
    CONTROLS = { 'n' => "\n", 't' => "\t", 'r' => "\r", 'f' => "\f", 'v' => "\v" }.freeze
    attr_reader :code, :groups

    def initialize(pattern, insensitive: false, dotall: false, multiline: false, literal: false, newline_sensitive: false)
      @insensitive, @dotall, @multiline = insensitive, dotall, multiline
      @newline_sensitive = newline_sensitive && !literal
      @characters, @position, @groups, @depth = RegexText.new(pattern), 0, 0, 0
      raise Error.new('54001', 'regular expression program is too large') if literal && @characters.length + 6 > LIMIT
      root = literal ? [:concat, @characters.map { |char| [:char, fold(char)] }] : alternatives
      @position = @characters.length if literal
      invalid('unbalanced parenthesis') unless @position == @characters.length
      raise Error.new('54001', 'regular expression program is too large') if size(root) + 6 > LIMIT
      @code = [[:split, 3, 1], [:any_char], [:jump, 0], [:save, 0]]
      emit(root)
      @code << [:save, 1] << [:match]
      @marks = Array.new(@code.length, 0)
      @generation = 0
      @slot_depth = 1
      @slot_depth += 1 while 16**@slot_depth < 2 * (@groups + 1)
      @characters = nil
    end

    def input(text)
      (text.is_a?(RegexText) ? text : RegexText.new(text)).with_case(@insensitive)
    end

    def match?(text, context: nil) = !search(input(text), 0, context: context).nil?

    def search(input, start = 0, context: nil)
      return nil if start > input.length
      current = []
      generation = next_generation
      closure(current, 0, nil, start, input, generation, context)
      matched = nil
      position = start
      while position <= input.length
        following = []
        generation = next_generation
        index = 0
        while index < current.length
          pc, saves = current[index]
          instruction = @code[pc]
          char = input[position]
          accepts = case instruction[0]
          when :char then char == instruction[1]
          when :any then char && char != "\n"
          when :any_char then !char.nil?
          when :class
            if char
              included = instruction[2].any? { |low, high| char.ord.between?(low, high) }
              instruction[1] ? !included && (!@newline_sensitive || !instruction[3] || char != "\n") : included
            end
          when :match
            matched = saves
            break
          end
          closure(following, pc + 1, saves, position + 1, input, generation, context) if accepts
          index += 1
        end
        current = following
        break if current.empty?
        position += 1
      end
      matched
    end

    def span(saves, group = 0)
      return nil unless saves && group <= @groups
      first, last = slot(saves, 2 * group), slot(saves, 2 * group + 1)
      first && last ? [first, last] : nil
    end

    private

    def next_generation
      @generation += 1
    end

    def closure(list, pc, saves, position, input, generation, context)
      stack = [[pc, saves]]
      until stack.empty?
        pc, saves = stack.pop
        next if @marks[pc] == generation
        @marks[pc] = generation
        context&.charge
        instruction = @code[pc]
        case instruction[0]
        when :jump then stack << [instruction[1], saves]
        when :split
          stack << [instruction[2], saves] << [instruction[1], saves]
        when :save then stack << [pc + 1, with_slot(saves, instruction[1], position, @slot_depth)]
        when :start then stack << [pc + 1, saves] if position.zero?
        when :end then stack << [pc + 1, saves] if position == input.length
        when :line_start then stack << [pc + 1, saves] if position.zero? || input[position - 1] == "\n"
        when :line_end then stack << [pc + 1, saves] if position == input.length || input[position] == "\n"
        else list << [pc, saves]
        end
      end
    end

    # A Save copies at most four 16-entry nodes, independent of capture count.
    def with_slot(tree, index, value, depth)
      copy = tree ? tree.dup : Array.new(16)
      branch = (index >> (4 * (depth - 1))) & 15
      copy[branch] = depth == 1 ? value : with_slot(copy[branch], index, value, depth - 1)
      copy
    end

    def slot(tree, index)
      depth = @slot_depth
      while tree && depth.positive?
        tree = tree[(index >> (4 * (depth - 1))) & 15]
        depth -= 1
      end
      tree
    end

    def fold(char) = @insensitive ? char.tr('A-Z', 'a-z') : char
    def peek = @characters[@position]
    def take
      value = peek
      @position += 1 if value
      value
    end
    def invalid(message) = raise(Error.new('2201B', "invalid regular expression: #{message}"))

    def alternatives
      branches = [concatenation]
      while peek == '|'
        take
        branches << concatenation
      end
      branches.length == 1 ? branches[0] : [:alternatives, branches]
    end

    def concatenation
      nodes = []
      nodes << quantified until peek.nil? || %w[| )].include?(peek)
      nodes.length == 1 ? nodes[0] : [:concat, nodes]
    end

    def quantified
      node = atom
      kind = { '*' => :star, '+' => :plus, '?' => :optional }[peek]
      if kind
        take
        lazy = peek == '?'
        take if lazy
        return [kind, node, lazy]
      end
      if (counts = repetition)
        @position = counts[2]
        lazy = peek == '?'
        take if lazy
        return [:repeat, node, counts[0], counts[1], lazy]
      end
      node
    end

    def repetition
      return nil unless peek == '{'
      cursor = @position + 1
      return nil unless @characters[cursor]&.between?('0', '9')
      low = 0
      while @characters[cursor]&.between?('0', '9')
        low = [low * 10 + @characters[cursor].ord - 48, LIMIT + 1].min
        cursor += 1
      end
      high = low
      if @characters[cursor] == ','
        cursor += 1
        high = nil
        if @characters[cursor]&.between?('0', '9')
          high = 0
          while @characters[cursor]&.between?('0', '9')
            high = [high * 10 + @characters[cursor].ord - 48, LIMIT + 1].min
            cursor += 1
          end
        end
      end
      return nil unless @characters[cursor] == '}'
      invalid('invalid repetition count') if high && high < low
      [low, high, cursor + 1]
    end

    def atom
      char = take
      case char
      when '(' 
        @depth += 1
        raise Error.new('54001', 'regular expression nesting is too deep') if @depth > 256
        capture = true
        if peek == '?'
          take
          invalid('unbalanced parenthesis') unless take == ':'
          capture = false
        end
        group = (@groups += 1) if capture
        body = alternatives
        invalid('unbalanced parenthesis') unless take == ')'
        @depth -= 1
        capture ? [:group, group, body] : body
      when '[' then character_class
      when '.' then [@dotall ? :any_char : :any]
      when '^' then [@multiline ? :line_start : :start]
      when '$' then [@multiline ? :line_end : :end]
      when '\\' then escape(false)
      when '*', '+', '?' then invalid('quantifier operand missing')
      when nil then [:concat, []]
      else
        if char == '{'
          @position -= 1
          counts = repetition
          @position += 1
          invalid('quantifier operand missing') if counts
        end
        [:char, fold(char)]
      end
    end

    def escape(in_class)
      char = take
      invalid('trailing backslash') unless char
      return [:char, CONTROLS[char]] if CONTROLS.key?(char)
      if PREDEFINED.key?(char.downcase)
        return [:class, char != char.downcase, PREDEFINED[char.downcase]]
      end
      return [:start] if char == 'A' && !in_class
      return [:end] if char == 'z' && !in_class
      return [:char, char] if '.+*?()[]{}|^$\\-'.include?(char)
      invalid("invalid escape \\#{char}")
    end

    def character_class
      negated = peek == '^'
      take if negated
      ranges, first = [], true
      while peek && peek != ']'
        invalid('unbalanced bracket expression') if peek == '[' && @characters[@position + 1] == ':'
        node = take == '\\' ? escape(true) : [:char, fold(@characters[@position - 1])]
        if node[0] == :char && peek == '-' && @characters[@position + 1] && @characters[@position + 1] != ']'
          take
          last = take == '\\' ? escape(true) : [:char, fold(@characters[@position - 1])]
          invalid('unbalanced bracket expression') unless last[0] == :char && node[1].ord <= last[1].ord
          ranges << [node[1].ord, last[1].ord]
        elsif node[0] == :class
          ranges.concat(node[1] ? complement(node[2]) : node[2])
        else
          ranges << [node[1].ord, node[1].ord]
        end
        first = false
      end
      invalid('unbalanced bracket expression') unless take == ']' && !first
      [:class, negated, normalize_ranges(ranges), true]
    end

    def normalize_ranges(ranges)
      sorted = ranges.sort
      merged = []
      sorted.each do |low, high|
        if !merged.empty? && low <= merged[-1][1] + 1
          merged[-1][1] = [high, merged[-1][1]].max
        else
          merged << [low, high]
        end
      end
      merged
    end

    def complement(ranges)
      result, cursor = [], 0
      normalize_ranges(ranges).each do |low, high|
        result << [cursor, low - 1] if cursor < low
        cursor = high + 1
      end
      result << [cursor, MAX_CHAR] if cursor <= MAX_CHAR
      result
    end

    def size(node)
      amount = case node[0]
      when :concat then node[1].sum { |child| size(child) }
      when :alternatives then node[1].sum { |child| size(child) } + 2 * (node[1].length - 1)
      when :group then size(node[2]) + 2
      when :star then size(node[1]) + 2
      when :plus, :optional then size(node[1]) + 1
      when :repeat
        sub = size(node[1])
        node[3] ? sub * node[3] + node[3] - node[2] : sub * node[2] + sub + 2
      else 1
      end
      raise Error.new('54001', 'regular expression program is too large') if amount > LIMIT
      amount
    end

    def emit(node)
      case node[0]
      when :concat then node[1].each { |child| emit(child) }
      when :alternatives then emit_alternatives(node[1], 0)
      when :group
        @code << [:save, 2 * node[1]]
        emit(node[2])
        @code << [:save, 2 * node[1] + 1]
      when :star, :plus, :optional
        start = @code.length
        split = [:split]
        @code << split unless node[0] == :plus
        body = @code.length
        emit(node[1])
        case node[0]
        when :star then @code << [:jump, start]
        when :plus then @code << split
        end
        ends = @code.length
        split.concat(node[2] ? [ends, body] : [body, ends])
      when :repeat
        node[2].times { emit(node[1]) } unless size(node[1]).zero?
        if node[3]
          (node[3] - node[2]).times { emit([:optional, node[1], node[4]]) }
        else
          emit([:star, node[1], node[4]])
        end
      else @code << node
      end
    end

    def emit_alternatives(nodes, index)
      # Emit the right-associative fold iteratively: a long a|b|... does not use
      # the Ruby call stack, and its instruction order stays the shared order.
      jumps = []
      while index < nodes.length - 1
        split = [:split, @code.length + 1]
        @code << split
        emit(nodes[index])
        jump = [:jump]
        @code << jump
        jumps << jump
        split << @code.length
        index += 1
      end
      emit(nodes[index])
      jumps.each { |jump| jump << @code.length }
    end
  end
end
