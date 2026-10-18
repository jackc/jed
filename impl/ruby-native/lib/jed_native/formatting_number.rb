# frozen_string_literal: true
require_relative 'formatting_tables'

module JedNative
  class NumberTemplate
    attr_accessor :pre, :post, :sign_position, :sign_digits, :multi, :zero_start, :zero_end
    attr_reader :nodes, :flags
    def initialize(template)
      @pre = @post = @sign_position = @sign_digits = @multi = @zero_start = @zero_end = 0
      @flags, @nodes = {}, []
      i = 0
      while i < template.length
        token = FormattingTables::NUM.find { |entry| template[i, entry['name'].length] == entry['name'] }
        if token
          prepare(token['id'])
          @nodes << [token['id'], nil]
          i += token['name'].length
        elsif template[i] == '"'
          i += 1
          while i < template.length && template[i] != '"'
            i += 1 if template[i] == '\\' && i + 1 < template.length
            @nodes << [nil, template[i]]
            i += 1
          end
          i += 1 if template[i] == '"'
        else
          i += 1 if template[i, 2] == '\\"'
          @nodes << [nil, template[i]]
          i += 1
        end
      end
    end
    def has?(flag) = @flags[flag]
    def invalid = raise(Error.new('42601', 'invalid number format template'))
    def prepare(id)
      invalid if has?(:scientific) && id != 'E'
      case id
      when '9'
        invalid if has?(:bracket)
        if has?(:multiply) then @multi += 1
        elsif has?(:decimal) then @post += 1
        else @pre += 1
        end
      when '0'
        invalid if has?(:bracket)
        if !has?(:zero) && !has?(:decimal)
          @flags[:zero], @zero_start = true, @pre + 1
        end
        has?(:decimal) ? @post += 1 : @pre += 1
        @zero_end = @pre + @post
      when 'B' then @flags[:blank] = true if @pre.zero? && @post.zero? && !has?(:zero)
      when 'D', 'DEC'
        invalid if has?(:decimal) || has?(:multiply)
        @flags[:decimal] = true
      when 'FM' then @flags[:fill] = true
      when 'S'
        invalid if %i[sign plus minus bracket].any? { |flag| has?(flag) }
        @sign_position = has?(:decimal) ? 1 : -1
        @sign_digits = @pre unless has?(:decimal)
        @flags[:sign] = true
      when 'MI', 'PL', 'SG'
        invalid if has?(:sign)
        @flags[:minus] = true if id != 'PL'
        @flags[:plus] = true if id != 'MI'
      when 'PR'
        invalid if %i[sign plus minus].any? { |flag| has?(flag) }
        @flags[:bracket] = true
      when 'RN', 'rn'
        invalid if has?(:roman)
        @flags[:roman] = true
      when 'V'
        invalid if has?(:decimal)
        @flags[:multiply] = true
      when 'E'
        invalid if %i[scientific blank fill sign bracket minus plus roman multiply].any? { |flag| has?(flag) }
        @flags[:scientific] = true
      end
      invalid if has?(:roman) && @flags.keys.any? { |flag| !%i[roman fill].include?(flag) }
    end
  end

  class NumberProcessor
    def initialize(template, number, padding, sign, input = nil)
      @template, @number, @padding, @sign, @input = template, number, padding, sign, input
      @number_index = @input_index = @current = @read_pre = @read_post = 0
      @sign_written = @number_in = @read_decimal = false
      @last = nil
      @out = +''
    end
    def at(index = @number_index) = @number[index] || "\0"
    def input_char(offset = 0) = @input[@input_index + offset] || "\0"
    def digit?(char) = char >= '0' && char <= '9'
    def leading_zero? = !@template.has?(:zero) && @number_index.zero? && at(0) == '0' && !@template.post.zero?
    def last_point? = @last && at(@last) == '.'

    def write_number(id)
      n = @template
      return if n.has?(:roman)
      @number_in = false
      fill = n.has?(:fill)
      if !@sign_written && (@current >= @padding || n.has?(:zero) && n.zero_start == @current) && (!leading_zero? || last_point?)
        if n.has?(:sign)
          if n.sign_position == -1
            @out << @sign
            @sign_written = true
          end
        elsif n.has?(:bracket)
          @out << (@sign == '+' ? ' ' : '<')
          @sign_written = true
        else
          @out << (@sign == '-' ? '-' : fill ? '' : ' ')
          @sign_written = true
        end
      end
      if @current < @padding && (n.zero_start > @current || !n.has?(:zero))
        @out << ' ' unless fill
      elsif n.has?(:zero) && @current < @padding && n.zero_start <= @current
        @out << '0'
        @number_in = true
      else
        if at == '.'
          @out << '.' if !last_point? || fill
        elsif @last && @number_index > @last && id != '0'
          # Optional trailing fractional zeros are omitted in fill mode.
        elsif leading_zero?
          @out << ' ' unless fill
          @out << '0' if fill && last_point?
        else
          @out << at
          @number_in = true
        end
        @number_index += 1 unless at == "\0"
      end
      finish = @count + (@padding.zero? ? 0 : 1) + (n.has?(:decimal) ? 1 : 0)
      finish = @current if @last && @last == @number_index
      if @current + 1 == finish
        if @sign_written && n.has?(:bracket)
          @out << (@sign == '+' ? ' ' : '>')
        elsif n.has?(:sign) && n.sign_position == 1
          @out << @sign
        end
      end
      @current += 1
    end

    def read_number(id)
      n = @template
      @input_index += 1 if input_char == ' '
      return if @input_index >= @input.length
      if @number[0] == ' ' && %w[0 9].include?(id) && @read_pre + @read_post == 0
        if n.has?(:sign) && n.sign_position == -1
          if %w[- +].include?(input_char)
            @number[0] = input_char
            @input_index += 1
          end
        elsif input_char == '-' || n.has?(:bracket) && input_char == '<'
          @number[0] = '-'
          @input_index += 1
        elsif input_char == '+'
          @number[0] = '+'
          @input_index += 1
        end
      end
      return if @input_index >= @input.length
      read = false
      if digit?(input_char)
        return if @read_decimal && @read_post == n.post
        @number << input_char
        @read_decimal ? @read_post += 1 : @read_pre += 1
        read = true
      elsif n.has?(:decimal) && !@read_decimal && input_char == '.'
        @number << '.'
        @read_decimal = read = true
      end
      if @number[0] == ' ' && @read_pre + @read_post > 0
        if n.has?(:sign) && read && !digit?(input_char(1)) && %w[- +].include?(input_char(1))
          @input_index += 1
          @number[0] = input_char
        elsif !read && !n.has?(:sign) && (n.has?(:plus) || n.has?(:minus)) && %w[- +].include?(input_char)
          @number[0] = input_char
        end
      end
    end

    def skip_nondata(count)
      count.times do
        break if @input_index >= @input.length || '0123456789.,+-'.include?(input_char)
        @input_index += 1
      end
    end

    def run
      n = @template
      writing = @input.nil?
      n.zero_start -= 1 unless n.zero_start.zero?
      if n.has?(:scientific)
        raise Error.new('0A000', 'EEEE is not supported for input') unless writing
        return @number
      end
      fill = n.has?(:fill)
      if writing
        if n.has?(:plus) || n.has?(:minus)
          @sign_written = !(n.has?(:plus) && !n.has?(:minus))
        else
          n.flags.delete(:bracket) if @sign != '-' && fill
          @sign_written = @sign == '+' && fill && !n.has?(:sign)
          n.sign_position = 1 if n.sign_position == -1 && n.pre == n.sign_digits
        end
      end
      @count = n.post + n.pre - 1
      if writing
        if fill && n.has?(:decimal) && (point = @number.index('.'))
          @last = point
          (point + 1...@number.length).each { |i| @last = i if at(i) != '0' }
          @last = [@last, [@number.length - 1, n.zero_end - @padding].min].max if n.zero_end > @padding
        end
        @count += 1 if !@sign_written && @padding.zero?
      else
        @number = +' '
      end
      n.nodes.each do |id, literal|
        break if !writing && @input_index >= @input.length
        unless id
          writing ? @out << literal : @input_index += 1
          next
        end
        case id
        when '9', '0', 'DEC', 'D'
          if writing then write_number(id); next else read_number(id) end
        when 'COMMA', 'G'
          if writing
            @out << (@number_in ? ',' : fill ? '' : ' ')
            next
          end
          next if !@number_in && fill || input_char != ','
        when 'L'
          writing ? @out << ' ' : skip_nondata(1)
          next
        when 'RN', 'rn'
          if writing
            text = id == 'rn' ? @number.downcase : @number
            @out << (fill ? text : text.rjust(15))
          else
            @input_index += 1 while @input_index < @input.length && " \t\n\r\f\v".include?(input_char)
            roman = +''
            while roman.length < 15 && 'IVXLCDM'.include?(input_char.upcase) && input_char != "\0"
              roman << input_char.upcase
              @input_index += 1
            end
            value = NumberFormatting.roman_value(roman)
            @number << value.to_s
            n.pre, n.post = value.to_s.length, 0
          end
          next
        when 'TH', 'th'
          next if n.has?(:roman) || at(0) == '#' || @sign == '-' || n.has?(:decimal)
          if writing
            number = @number.to_i
            suffix = number % 100 >= 11 && number % 100 <= 13 ? 'th' : { 1 => 'st', 2 => 'nd', 3 => 'rd' }.fetch(number % 10, 'th')
            @out << (id == 'TH' ? suffix.upcase : suffix)
          else skip_nondata(2)
          end
          next
        when 'MI', 'PL', 'SG'
          want = id == 'PL' ? '+' : '-'
          if writing
            @out << (id == 'SG' ? @sign : @sign == want ? want : fill ? '' : ' ')
            next
          end
          if id == 'SG' ? %w[- +].include?(input_char) : input_char == want
            @number[0] = input_char
          else
            skip_nondata(1)
            next
          end
        else next
        end
        @input_index += 1 unless writing
      end
      return @out.split("\0", 2).first || '' if writing
      n.post = @read_post
      @number.delete_suffix('.')
    end
  end

  module NumberFormatting
    module_function
    ROMAN = [[1000,'M'],[900,'CM'],[500,'D'],[400,'CD'],[100,'C'],[90,'XC'],[50,'L'],[40,'XL'],[10,'X'],[9,'IX'],[5,'V'],[4,'IV'],[1,'I']].freeze
    def roman(number)
      return '#' * 15 unless number.between?(1, 3999)
      ROMAN.map { |amount, text| count, number = number.divmod(amount); text * count }.join
    end
    def roman_value(text)
      values = { 'I'=>1, 'V'=>5, 'X'=>10, 'L'=>50, 'C'=>100, 'D'=>500, 'M'=>1000 }
      value = text.chars.each_with_index.sum { |char, i| values[char] < values.fetch(text[i + 1], 0) ? -values[char] : values[char] }
      raise Error.new('22P02', 'invalid Roman numeral') if text.empty? || roman(value) != text
      value
    end
    def rounded_rational(value, scale)
      scaled = value * 10**scale
      low, rest = scaled.numerator.divmod(scaled.denominator)
      low += 1 if rest * 2 > scaled.denominator || rest * 2 == scaled.denominator && low.odd?
      Decimal.new(low, scale)
    end
    def fixed_float(value, precision)
      return 'NaN' if value.nan?
      negative = value.negative? || value.zero? && (1.0 / value).negative?
      return negative ? '-Infinity' : 'Infinity' if value.infinite?
      actual = [precision, 350].min
      (negative ? '-' : '') + rounded_rational(value.abs.to_r, actual).to_s + '0' * (precision - actual)
    end
    def scientific(value, precision, float: false)
      decimal = float ? Decimal.parse(value.abs.to_s) : Decimal.parse(value).abs
      exponent = decimal.zero? ? 0 : decimal.coefficient.abs.to_s.length - decimal.scale - 1
      ratio = value.abs.to_r / (exponent.negative? ? Rational(1, 10**-exponent) : 10**exponent)
      significand = float ? rounded_rational(ratio, [precision, 350].min) : Decimal.new(ratio.numerator).divide(Decimal.new(ratio.denominator), precision)
      if float && significand >= Decimal.new(10)
        significand = rounded_rational(ratio / 10, [precision, 350].min)
        exponent += 1
      end
      sign = value.negative? || value.is_a?(Float) && value.zero? && (1.0 / value).negative? ? '-' : ' '
      sign + significand.to_s + 'e' + (exponent.negative? ? '-' : '+') + exponent.abs.to_s.rjust(2, '0')
    end
    def to_char(value, template, type: nil)
      return '' if template.empty?
      n = NumberTemplate.new(template)
      if n.has?(:roman)
        integer = value.is_a?(Float) ? value.finite? ? value.round(half: :even) : 0 : Decimal.parse(value).round.coefficient
        return NumberProcessor.new(n, roman(integer), 0, '').run
      end
      if n.has?(:scientific)
        if value.is_a?(Float) && !value.finite?
          result = '#' * (n.pre + n.post + 6)
          result[0], result[n.pre + 1] = ' ', '.'
          return result
        end
        return scientific(value, n.post, float: value.is_a?(Float) || %w[i16 i32].include?(type))
      end
      if n.has?(:multiply)
        if value.is_a?(Integer)
          factor = Types.check_integer(10**n.multi, type == 'i64' ? 'i64' : 'i32')
          value = Types.check_integer(value * factor, type == 'i64' ? 'i64' : 'i32')
        elsif value.is_a?(Float)
          factor = ("1e#{n.multi}").to_f
          factor = factor.next_float if [23, 210].include?(n.multi)
          value = type == 'f32' ? [value * [factor].pack('f').unpack1('f')].pack('f').unpack1('f') : value * factor
        else value *= 10**n.multi
        end
        n.pre += n.multi
      end
      if value.is_a?(Float)
        digits = type == 'f32' ? 6 : 15
        integer_length = fixed_float(value.abs, 0).length
        n.post = integer_length >= digits ? 0 : [n.post, digits - integer_length].min
        text = fixed_float(value, n.post)
      else
        text = Decimal.parse(value).round(n.post).to_s
      end
      sign = text.start_with?('-') ? '-' : '+'
      text = text.delete_prefix('-')
      length = text.index('.') || text.length
      padding = [n.pre - length, 0].max
      text = '#' * n.pre + '.' + '#' * n.post if length > n.pre
      NumberProcessor.new(n, text, padding, sign).run
    end
    def to_number(input, template, context: nil)
      return nil if template.empty?
      n = NumberTemplate.new(template)
      number = NumberProcessor.new(n, '', 0, '', input).run
      decimal = Decimal.parse(number)
      raise Error.new('22003', 'numeric field overflow') if !decimal.zero? && decimal.coefficient.abs.to_s.length - decimal.scale > n.pre + n.multi
      if n.has?(:multiply)
        decimal *= DecimalMath.power(Decimal.new(10), Decimal.new(-n.multi), context)
      end
      decimal
    end
  end

  module NumberFormattingExpressions
    def compile(node, columns)
      if node[:kind] == :call && node[:name] == 'to_char' && node[:args].length == 2
        value = compile(node[:args][0], columns)
        if %w[i16 i32 i64 decimal f32 f64].include?(value.type)
          template = compile(node[:args][1], columns)
          return Expression.new(kind: :number_format, type: 'text', args: [value, template])
        end
      end
      super
    end
    def evaluate(expression, row, group = nil)
      return super unless expression.kind == :number_format
      charge
      value, template = expression.args.map { |arg| evaluate(arg, row, group) }
      return nil if value.nil? || template.nil?
      result = NumberFormatting.to_char(value, template, type: expression.args[0].type)
      charge(result.bytesize) unless respond_to?(:constant_expression?) && constant_expression?(expression)
      result
    end
  end
  Engine.prepend(NumberFormattingExpressions)
end
