# frozen_string_literal: true

require_relative 'regex'

module JedNative
  # SIMILAR TO's pattern surface over the Pike VM (regex.md §8f). The
  # translation is PostgreSQL's similar_escape_internal, byte for byte; every
  # SIMILAR program compiles dot-all so `%` and `_` match a newline.
  module Similar
    SEPARATORS = 'SQL regular expression may not contain more than two escape-double-quote separators'
    module_function

    # The part texts between the escape-double-quote separators (one to
    # three). A nil escape is the default backslash; an empty one is none.
    def parts(pattern, escape = nil)
      escape_char = '\\'
      unless escape.nil?
        case escape.length
        when 0 then escape_char = nil
        when 1 then escape_char = escape
        else raise Error.new('22025', 'invalid escape string')
        end
      end
      # PostgreSQL's multibyte path: with a non-ASCII escape, a non-ASCII
      # pattern character is copied without touching the class state.
      wide_escape = escape_char && escape_char.ord >= 0x80
      parts = [+'']
      after_escape, in_class, class_start = false, false, 0
      pattern.each_char do |char|
        output = parts[-1]
        is_escape = escape_char && char == escape_char
        if wide_escape && char.ord >= 0x80
          if after_escape
            output << '\\' << char
            after_escape = false
          elsif is_escape
            after_escape = true
          else
            output << char
          end
          next
        end
        if after_escape
          if char == '"' && !in_class
            raise Error.new('2200C', SEPARATORS) if parts.length == 3
            parts << +''
          else
            output << '\\' << char
            class_start = 3
          end
          after_escape = false
        elsif is_escape
          after_escape = true
        elsif in_class
          output << '\\' if char == '\\'
          output << char
          # `]` is literal right after `[` or `[^`, else it closes the class.
          if char == ']' && class_start > 2
            in_class = false
          elsif char == '^'
            class_start += 1
          else
            class_start = 3
          end
        else
          case char
          when '['
            output << '['
            in_class, class_start = true, 1
          when '%' then output << '.*'
          when '_' then output << '.'
          when '(' then output << '(?:'
          when '\\', '.', '^', '$' then output << '\\' << char
          else output << char
          end
        end
      end
      parts
    end

    def join(parts)
      text = +'^(?:'
      text << parts[0]
      text << '){1,1}?(' << parts[1] if parts.length > 1
      text << '){1,1}(?:' << parts[2] if parts.length > 2
      text << ')$'
    end

    # similar_to_escape(pattern [, escape]).
    def translate(pattern, escape = nil) = join(parts(pattern, escape))

    def compile(translated) = RegexProgram.new(translated, dotall: true)

    # The programs of substring(s, p, e) (§8f.3): `whole` alone without
    # separators, else A and B, and for three parts C and D.
    class Matcher
      def initialize(pattern, escape)
        texts = Similar.parts(pattern, escape)
        if texts.length == 1
          @whole = Similar.compile(Similar.join(texts))
          return
        end
        @a = gated("(?:#{texts[0]})", :leftmost)
        if texts.length == 2
          @b = gated("(?:#{texts[1]})$", :leftmost)
        else
          @b = gated("(?:#{texts[1]})(?:#{texts[2]})$", :leftmost)
          @c = gated("(?:#{texts[1]})", :leftmost)
          @d = gated("(?:#{texts[2]})$", :rightmost)
        end
      end

      # The regex_compile instruction count of every program.
      def instructions
        return @whole.code.length if @whole
        [@a, @b, @c, @d].compact.sum { |program| program.code.length }
      end

      # The code-point span [i, j) of the result, or nil when the pattern does
      # not match the whole subject. Charges every run's regex_steps.
      def span(original, context)
        input = (@whole || @a).input(original)
        length = input.length
        return @whole.search(input, 0, context: context) ? [0, length] : nil if @whole
        first_ends = Array.new(length + 1, false)
        @a.search(input, 0, context: context, gate: at(0, length), ends: first_ends)
        match = @b.search(input, 0, context: context, gate: first_ends)
        return nil unless match
        start = @b.span(match)[0]
        return [start, length] unless @c
        middle_ends = Array.new(length + 1, false)
        @c.search(input, 0, context: context, gate: at(start, length), ends: middle_ends)
        # B's match proves a split exists, so D always finds one.
        [start, @d.span(@d.search(input, 0, context: context, gate: middle_ends))[0]]
      end

      private

      def gated(body, wrapper) = RegexProgram.new(body, dotall: true, wrapper: wrapper)

      def at(position, length)
        gate = Array.new(length + 1, false)
        gate[position] = true
        gate
      end
    end
  end
end
