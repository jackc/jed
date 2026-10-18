# frozen_string_literal: true

module JedNative
  class JsonPathLexer
    SPECIAL = '?%$.[]{}()|&!=<>@#,*:-+/'.freeze
    SPACE = " \t\n\r\f".freeze
    CONTROLS = { 'b' => "\b", 'f' => "\f", 'n' => "\n", 'r' => "\r", 't' => "\t", 'v' => "\v" }.freeze
    def initialize(source)
      @source, @index = source, 0
    end
    def error = raise(Error.new('42601', 'invalid JSON path syntax'))
    def tokens
      result = []
      while @index < @source.length
        char = @source[@index]
        if SPACE.include?(char)
          @index += 1
          next
        end
        if @source[@index, 2] == '/*'
          comment
          next
        end
        if char == '"'
          result << [:string, quoted]
        elsif char == '$'
          @index += 1
          next_char = @source[@index]
          if next_char == '"'
            result << [:variable, quoted]
          elsif next_char && !SPECIAL.include?(next_char) && !SPACE.include?(next_char) && next_char != '\\'
            first = @index
            @index += 1 while (next_char = @source[@index]) && !SPECIAL.include?(next_char) && !SPACE.include?(next_char) && !['"', '\\'].include?(next_char)
            result << [:variable, @source[first...@index]]
          else
            result << ['$', '$']
          end
        elsif char.between?('0', '9') || char == '.' && @source[@index + 1]&.between?('0', '9')
          rest = @source[@index..]
          number = /\A(?:0|[1-9][0-9]*)(?:\.[0-9]*)?(?:[eE][+-]?[0-9]+)?|\A\.[0-9]+(?:[eE][+-]?[0-9]+)?/.match(rest)&.to_s
          error unless number
          raw = rest.each_char.take_while { |c| !SPECIAL.include?(c) && !SPACE.include?(c) && c != '"' }.join
          adjacent_dot = @index.positive? && @source[@index - 1] == '.'
          competing = /\A(?:0[xX]_?[0-9a-fA-F](?:_?[0-9a-fA-F])*|0[oO]_?[0-7](?:_?[0-7])*|0[bB]_?[01](?:_?[01])*|[0-9](?:_?[0-9])*(?:\.[0-9](?:_?[0-9])*)?(?:[eE][+-]?[0-9](?:_?[0-9])*)?)/.match(rest)&.to_s || number
          junk_width = competing.match?(/\A0[xXoObB]/) ? 0 : 1
          if !adjacent_dot && raw.bytesize > competing.bytesize + junk_width && !number.include?('.')
            word = name
            result << [:name, word] if word
          else
            @index += number.length
            following = @source[@index]
            error if following && !SPECIAL.include?(following) && !SPACE.include?(following) && following != '"'
            result << [:number, Decimal.parse(number), number]
          end
        elsif SPECIAL.include?(char)
          pair = @source[@index, 2]
          op = %w[** == != <> <= >= && ||].include?(pair) ? pair : char
          @index += op.length
          result << [op, op]
        else
          word = name
          result << [:name, word] if word
        end
      end
      result << [:eof, nil]
      result
    end
    private
    def comment
      ending = @source.index('*/', @index + 2)
      error unless ending
      @index = ending + 2
    end
    def quoted
      @index += 1
      result = +''
      while @index < @source.length
        char = @source[@index]
        @index += 1
        return result if char == '"'
        result << (char == '\\' ? escape : char)
      end
      error
    end
    def name
      result = +''
      while (char = @source[@index]) && !SPECIAL.include?(char) && !SPACE.include?(char) && char != '"'
        @index += 1
        result << (char == '\\' ? escape : char)
      end
      error if result.empty?
      if @source[@index, 2] == '/*'
        comment
        return nil
      end
      result
    end
    def escape
      char = @source[@index]
      error unless char
      @index += 1
      return CONTROLS[char] if CONTROLS.key?(char)
      if char == 'u'
        points = [unicode_point]
        while @source[@index, 2] == '\\u'
          @index += 2
          points << unicode_point
        end
        return decode_points(points)
      end
      if char == 'x'
        hex = @source[@index, 2]
        error unless hex&.length == 2 && hex.match?(/\A[0-9a-fA-F]{2}\z/)
        @index += 2
        return decode_points([hex.to_i(16)])
      end
      char
    end
    def unicode_point
      if @source[@index] == '{'
        @index += 1
        ending = @source.index('}', @index)
        error unless ending
        hex = @source[@index...ending]
        error unless hex.match?(/\A[0-9a-fA-F]{1,6}\z/)
        @index = ending + 1
      else
        hex = @source[@index, 4]
        error unless hex&.match?(/\A[0-9a-fA-F]{4}\z/)
        @index += 4
      end
      point = hex.to_i(16)
      error if point > 0x10ffff
      point
    end
    def decode_points(points)
      result, index = +'', 0
      while index < points.length
        point = points[index]
        raise Error.new('22P05', 'unsupported Unicode escape sequence') if point.zero?
        if point.between?(0xd800, 0xdbff)
          low = points[index + 1]
          raise Error.new('22P02', 'invalid input syntax for type jsonpath') unless low&.between?(0xdc00, 0xdfff)
          point = 0x10000 + (point - 0xd800) * 1024 + low - 0xdc00
          index += 1
        elsif point.between?(0xdc00, 0xdfff)
          raise Error.new('22P02', 'invalid input syntax for type jsonpath')
        end
        result << point.chr(Encoding::UTF_8)
        index += 1
      end
      result
    end
  end
end
