# frozen_string_literal: true

module JedNative
  # Handwritten SQL scanner. Names are ASCII-folded only when unquoted; token
  # offsets are byte offsets into the original UTF-8 SQL.
  class Lexer
    Token = Struct.new(:kind, :value, :offset)
    MAX_IDENTIFIER_BYTES = 63
    MAX_INTEGER_MAGNITUDE = 1 << 63
    OPERATORS = %w[!~* ->> #>> -|- <= >= <> != :: => || -> #> ?| ?& #- @> <@ && << >> &< &> @? @@ ~* !~].freeze
    PUNCTUATION = '(),.[]+-*/%=<>:;?~'.freeze

    def initialize(sql)
      @sql = sql.to_s
      raise Error.new('22021', 'invalid UTF-8 in SQL input') unless @sql.valid_encoding?
      @length = @sql.bytesize
      @offset = 0
    end

    def tokens
      result = []
      while @offset < @length
        skip_space
        break if @offset >= @length
        start = @offset
        byte = @sql.getbyte(@offset)
        if (byte == 85 || byte == 117) && @sql.getbyte(@offset + 1) == 38 && [34, 39].include?(@sql.getbyte(@offset + 2))
          @offset += 2
          quote = @sql.getbyte(@offset)
          value = quoted(quote)
          escape = unicode_escape_character
          value = decode_unicode(value, escape)
          kind = quote == 34 ? :identifier : :string
          identifier_length(value) if kind == :identifier
          result << Token.new(kind, value, start)
        elsif byte == 39 || byte == 34
          value = quoted(byte)
          kind = byte == 34 ? :identifier : :string
          identifier_length(value) if kind == :identifier
          result << Token.new(kind, value, start)
        elsif digit?(byte) || (byte == 46 && digit?(@sql.getbyte(@offset + 1)))
          result << number
        elsif alpha?(byte)
          @offset += 1
          @offset += 1 while alpha?(@sql.getbyte(@offset)) || digit?(@sql.getbyte(@offset))
          value = @sql.byteslice(start, @offset - start).downcase
          identifier_length(value)
          result << Token.new(:word, value, start)
        elsif byte == 36
          @offset += 1
          nstart = @offset
          @offset += 1 while digit?(@sql.getbyte(@offset))
          fail_syntax('expected parameter number after $') if @offset == nstart
          fail_syntax('parameter index must not have a leading zero') if @sql.getbyte(nstart) == 48
          index = @sql.byteslice(nstart, @offset - nstart).to_i
          fail_syntax('parameter index must be between 1 and 65535') unless index.between?(1, 65_535)
          fail_syntax('trailing junk after parameter') if alpha?(@sql.getbyte(@offset))
          result << Token.new(:param, index, start)
        else
          op = OPERATORS.find { |candidate| @sql.byteslice(@offset, candidate.bytesize) == candidate }
          if op
            @offset += op.bytesize
            result << Token.new(:symbol, op == '!=' ? '<>' : op, start)
          elsif byte && PUNCTUATION.include?(byte.chr)
            @offset += 1
            result << Token.new(:symbol, byte.chr, start)
          else
            fail_syntax("unexpected character #{@sql.byteslice(@offset, 1).inspect}")
          end
        end
      end
      result << Token.new(:eof, nil, @offset)
    end

    private

    def alpha?(byte)
      byte && (byte == 95 || byte.between?(65, 90) || byte.between?(97, 122))
    end

    def digit?(byte)
      byte && byte.between?(48, 57)
    end

    def space?(byte)
      byte && (byte == 32 || byte.between?(9, 13))
    end

    def skip_space
      loop do
        @offset += 1 while space?(@sql.getbyte(@offset))
        if @sql.byteslice(@offset, 2) == '--'
          @offset += 2
          @offset += 1 while @offset < @length && @sql.getbyte(@offset) != 10
        elsif @sql.byteslice(@offset, 2) == '/*'
          @offset += 2
          depth = 1
          until depth.zero?
            fail_syntax('unterminated block comment') if @offset >= @length
            pair = @sql.byteslice(@offset, 2)
            if pair == '/*'
              depth += 1
              @offset += 2
            elsif pair == '*/'
              depth -= 1
              @offset += 2
            else
              @offset += 1
            end
          end
        else
          return
        end
      end
    end

    def quoted(quote)
      @offset += 1
      start = @offset
      parts = nil
      loop do
        fail_syntax(quote == 34 ? 'unterminated quoted identifier' : 'unterminated string literal') if @offset >= @length
        if @sql.getbyte(@offset) == quote
          if @sql.getbyte(@offset + 1) == quote
            parts ||= []
            parts << @sql.byteslice(start, @offset - start + 1)
            @offset += 2
            start = @offset
          else
            tail = @sql.byteslice(start, @offset - start)
            @offset += 1
            value = parts ? (parts << tail).join : tail
            fail_syntax('zero-length delimited identifier') if quote == 34 && value.empty?
            fail_syntax('zero byte in SQL literal') if value.include?("\0")
            return value
          end
        else
          @offset += 1
        end
      end
    end

    def unicode_escape_character
      saved = @offset
      skip_space
      if @sql.byteslice(@offset, 7)&.downcase == 'uescape' && !alpha?(@sql.getbyte(@offset + 7)) && !digit?(@sql.getbyte(@offset + 7))
        @offset += 7
        skip_space
        fail_syntax('UESCAPE must be followed by a simple string literal') unless @sql.getbyte(@offset) == 39
        char = quoted(39)
        fail_syntax('invalid Unicode escape character') unless char.bytesize == 1 && !char.match?(/[0-9a-fA-F+'"\s]/)
        char
      else
        @offset = saved
        '\\'
      end
    end

    def decode_unicode(value, escape)
      output = +''
      chars = value.each_char.to_a
      index = 0
      pending = nil
      while index < chars.length
        char = chars[index]
        if char != escape
          fail_syntax('invalid Unicode surrogate pair') if pending
          output << char
          index += 1
          next
        end
        if chars[index + 1] == escape
          fail_syntax('invalid Unicode surrogate pair') if pending
          output << escape
          index += 2
          next
        end
        width = chars[index + 1] == '+' ? 6 : 4
        index += width == 6 ? 2 : 1
        digits = chars[index, width]&.join
        fail_syntax('invalid Unicode escape') unless digits && digits.length == width && digits.match?(/\A[0-9A-Fa-f]+\z/)
        point = digits.to_i(16)
        index += width
        fail_syntax('invalid Unicode escape value') if point.zero? || point > 0x10ffff
        if pending
          fail_syntax('invalid Unicode surrogate pair') unless point.between?(0xdc00, 0xdfff)
          output << [0x10000 + ((pending - 0xd800) << 10) + point - 0xdc00].pack('U')
          pending = nil
        elsif point.between?(0xd800, 0xdbff)
          pending = point
        elsif point.between?(0xdc00, 0xdfff)
          fail_syntax('invalid Unicode surrogate pair')
        else
          output << [point].pack('U')
        end
      end
      fail_syntax('invalid Unicode surrogate pair') if pending
      output
    end

    def number
      start = @offset
      prefix = @sql.byteslice(@offset, 2)&.downcase
      bases = { '0x' => 16, '0o' => 8, '0b' => 2 }
      if (base = bases[prefix])
        @offset += 2
        digits_start = @offset
        @offset += 1 if @sql.getbyte(@offset) == 95 && radix_digit?(@sql.getbyte(@offset + 1), base)
        scan_digits(base)
        fail_syntax('invalid radix integer') if @offset == digits_start
        numeric_junk!
        value = @sql.byteslice(digits_start, @offset - digits_start).delete('_').to_i(base)
        fail_syntax('integer literal out of range') if value > MAX_INTEGER_MAGNITUDE
        return Token.new(:integer, value, start)
      end
      scan_digits(10)
      decimal = false
      if @sql.getbyte(@offset) == 46 && @sql.getbyte(@offset + 1) != 46
        decimal = true
        @offset += 1
        scan_digits(10) if digit?(@sql.getbyte(@offset))
      end
      if [69, 101].include?(@sql.getbyte(@offset))
        decimal = true
        @offset += 1
        @offset += 1 if [43, 45].include?(@sql.getbyte(@offset))
        fail_syntax('trailing junk after numeric literal') unless digit?(@sql.getbyte(@offset))
        scan_digits(10)
      end
      numeric_junk!
      text = @sql.byteslice(start, @offset - start).delete('_')
      if decimal
        Token.new(:decimal, text, start)
      else
        value = text.to_i
        fail_syntax('integer literal out of range') if value > MAX_INTEGER_MAGNITUDE
        Token.new(:integer, value, start)
      end
    end

    def scan_digits(base)
      loop do
        if radix_digit?(@sql.getbyte(@offset), base)
          @offset += 1
        elsif @sql.getbyte(@offset) == 95 && radix_digit?(@sql.getbyte(@offset + 1), base)
          @offset += 2
        else
          return
        end
      end
    end

    def radix_digit?(byte, base)
      return false unless byte
      value = if byte.between?(48, 57)
                byte - 48
              elsif byte.between?(65, 70)
                byte - 55
              elsif byte.between?(97, 102)
                byte - 87
              else
                base
              end
      value < base
    end

    def numeric_junk!
      byte = @sql.getbyte(@offset)
      fail_syntax('trailing junk after numeric literal') if alpha?(byte) || (byte && byte >= 128)
    end

    def identifier_length(value)
      raise Error.new('42622', 'identifier exceeds maximum length of 63 bytes') if value.bytesize > MAX_IDENTIFIER_BYTES
    end

    def fail_syntax(message)
      raise Error.new('42601', "#{message} at byte #{@offset + 1}")
    end
  end
end
