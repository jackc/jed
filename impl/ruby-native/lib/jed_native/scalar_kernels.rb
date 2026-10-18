# frozen_string_literal: true

module JedNative
  # Amplifiers size their result before allocating it. Input scanning consumes
  # work, while only the output consumes the statement's allocation allowance.
  module ScalarKernels
    KERNELS = %w[repeat lpad rpad replace array_replace array_to_string concat concat_ws format overlay jsonb_pretty].freeze
    MAX_RESULT_BYTES = 0x3fffffff

    def call(name, args, context: nil, arg_types: nil)
      return super unless KERNELS.include?(name)
      types = arg_types || args.map { |value| Types.infer(value) }
      resolve_signature(name, types)
      return nil if !Functions::NULL_TOLERANT.include?(name) && args.any?(&:nil?)
      a, b, c, d = args
      case name
      when 'jsonb_pretty'
        context&.charge(LogicalMemory.payload(a))
        reserve(context, json_pretty_bytes(a.value))
        a.pretty
      when 'repeat'
        return '' if b <= 0 || a.empty?
        context&.charge(a.bytesize)
        reserve(context, a.bytesize * b)
        a * b
      when 'lpad', 'rpad'
        raise Error.new('54000', 'requested length too large') if b > MAX_RESULT_BYTES
        return '' if b <= 0
        fill = c || ' '
        context&.charge(a.bytesize + fill.bytesize)
        if b <= a.length || fill.empty?
          result = a[0, b]
          reserve(context, result.bytesize)
          return result
        end
        copies, remainder = (b - a.length).divmod(fill.length)
        suffix = fill[0, remainder]
        reserve(context, a.bytesize + copies * fill.bytesize + suffix.bytesize)
        padding = fill * copies + suffix
        name == 'lpad' ? padding + a : a + padding
      when 'replace'
        context&.charge(a.bytesize)
        count, position = 0, 0
        unless b.empty?
          while (found = a.index(b, position))
            count += 1
            position = found + b.length
          end
        end
        reserve(context, a.bytesize + count * (c.bytesize - b.bytesize))
        # Block replacement treats backslashes literally, like SQL replace.
        b.empty? ? a : a.gsub(b) { c }
      when 'array_replace'
        return nil if a.nil?
        input_size = LogicalMemory.payload(a)
        context&.charge(input_size)
        new_size = LogicalMemory.value_bytes(c, a.element_type)
        output_size = input_size
        a.each { |value| output_size += new_size - LogicalMemory.value_bytes(value, a.element_type) if Types.compare(value, b).zero? }
        reserve_array(context, output_size)
        array(a.map { |value| Types.compare(value, b).zero? ? c : value }, a)
      when 'array_to_string'
        return nil if a.nil? || b.nil?
        context&.charge(LogicalMemory.payload(a))
        items = a.filter_map { |value| value.nil? ? c : scalar_output_text(value, a.element_type, context) }
        reserve(context, items.sum(&:bytesize) + [items.length - 1, 0].max * b.bytesize)
        items.join(b)
      when 'concat'
        args.each_with_index.filter_map { |value, i| scalar_output_text(value, types[i], context) unless value.nil? }.join
      when 'concat_ws'
        return nil if a.nil?
        items = args.each_with_index.filter_map { |value, i| scalar_output_text(value, types[i], context) if i.positive? && !value.nil? }
        reserve(context, items.sum(&:bytesize) + [items.length - 1, 0].max * a.bytesize)
        items.join(a)
      when 'format'
        return nil if a.nil?
        format_parts(a, args[1..], types[1..], context)
      when 'overlay'
        raise Error.new('22011', 'negative substring length not allowed') if c <= 0
        tail = Types.check_integer(c + (d || b.length), 'i32')
        result = (a[0, c - 1] || a) + b + (a[[tail - 1, 0].max..] || '')
        a.is_a?(Bytea) ? Bytea.new(result) : result
      end
    end

    def reserve_array(context, bytes)
      if context
        context.charge(bytes)
        context.consume_scalar_bytes(bytes)
      elsif bytes > 64 * 1024 * 1024
        raise Error.new('54P04', 'scalar allocation limit exceeded')
      end
    end

    def scalar_output_text(value, type, context)
      return 't' if value == true
      return 'f' if value == false
      return context.render_value(value, type, meter: true) if context&.respond_to?(:render_value)
      if value.is_a?(TimestampValue) && value.zoned && value.micros.is_a?(Integer)
        context&.charge
      end
      Types.format(value, type)
    end

    def json_pretty_bytes(value, level = 0)
      case value
      when Array
        return 3 + 4 * level if value.empty?
        4 + 4 * level + (2 + 4 * (level + 1)) * value.length - 2 + value.sum { |item| json_pretty_bytes(item, level + 1) }
      when Hash
        return 2 if value.empty?
        4 + 4 * level + (2 + 4 * (level + 1)) * value.length - 2 + value.sum { |key, item| JsonValue.escape(key).bytesize + 2 + json_pretty_bytes(item, level + 1) }
      else
        JsonValue.render(value).bytesize
      end
    end

    # Store literal/rendered pieces and integer padding lengths, never padding
    # buffers. This also reports malformed later specifiers before admission.
    def format_parts(template, values, types, context)
      context&.charge(template.bytesize)
      parts, size, position, next_argument = [], 0, 0, 0
      while position < template.length
        percent = template.index('%', position)
        unless percent
          text = template[position..]
          parts << [text, 0, false]
          size += text.bytesize
          break
        end
        if percent > position
          text = template[position...percent]
          parts << [text, 0, false]
          size += text.bytesize
        end
        position = percent + 1
        if template[position] == '%'
          parts << ['%', 0, false]
          size += 1
          position += 1
          next
        end
        argument = nil
        number, after = format_number(template, position)
        if number && template[after] == '$'
          raise Error.new('22023', 'format specifies argument 0') if number.zero?
          argument, position = number - 1, after + 1
        end
        left = false
        while template[position] == '-'
          left = true
          position += 1
        end
        width = 0
        if template[position] == '*'
          position += 1
          number, after = format_number(template, position)
          if number
            raise Error.new('22023', 'width argument position must be ended by "$"') unless template[after] == '$'
            raise Error.new('22023', 'format specifies argument 0') if number.zero?
            width_index, position = number - 1, after + 1
          else
            width_index = next_argument
          end
          raise Error.new('22023', 'too few arguments for format()') unless width_index < values.length
          value = values[width_index]
          width = value.nil? ? 0 : Types.cast(value.is_a?(Integer) ? value : scalar_output_text(value, types[width_index], context), 'i32')
          raise Error.new('22003', 'number is out of range') if width == -2147483648
          left ||= width.negative?
          width = width.abs
          next_argument = width_index + 1
        else
          number, after = format_number(template, position)
          width, position = number, after if number
        end
        conversion = template[position]
        raise Error.new('22023', 'unterminated format() type specifier') unless conversion
        raise Error.new('22023', "unrecognized format() type specifier #{conversion.inspect}") unless %w[s I L].include?(conversion)
        position += 1
        argument ||= next_argument
        raise Error.new('22023', 'too few arguments for format()') unless argument < values.length
        next_argument = argument + 1
        value = values[argument]
        text = value.nil? ? nil : scalar_output_text(value, types[argument], context)
        rendered = case conversion
        when 's' then text || ''
        when 'L' then text.nil? ? 'NULL' : quote_literal(text)
        when 'I'
          raise Error.new('22004', 'null values cannot be formatted as an SQL identifier') if text.nil?
          call('quote_ident', [text])
        end
        padding = [width - rendered.length, 0].max
        size += rendered.bytesize + padding
        parts << [rendered, padding, left]
      end
      reserve(context, size)
      result = +''
      parts.each do |text, padding, left|
        result << (' ' * padding) unless left
        result << text
        result << (' ' * padding) if left
      end
      result
    end

    def format_number(template, position)
      start = position
      position += 1 while (char = template[position]) && char >= '0' && char <= '9'
      return [nil, start] if position == start
      number = template[start...position].to_i
      raise Error.new('22003', 'number is out of range') if number > 2147483647
      [number, position]
    end
  end
  Functions.singleton_class.prepend(ScalarKernels)

  module TextBuilders
    BUILDERS = %w[concat concat_ws format].freeze
    def compile(node, columns)
      return super unless node[:kind] == :call && BUILDERS.include?(node[:name])
      args = node[:args].map { |arg| compile(arg, columns) }
      name = node[:name]
      minimum = name == 'concat_ws' ? 2 : 1
      raise Error.new('42883', 'too few text builder arguments') if args.length < minimum
      if node[:variadic]
        raise Error.new('42804', 'VARIADIC argument must be an array') unless args.last.type.end_with?('[]') || args.last.type == 'unknown'
      end
      if name != 'concat'
        first = args.first
        if first.type == 'unknown'
          first.type = 'text'
        elsif first.type != 'text'
          raise Error.new('42883', 'text builder requires text first argument')
        end
      end
      Expression.new(kind: :text_builder, type: 'text', value: name, args: args, extra: node)
    end

    def evaluate(expression, row, group = nil)
      return super unless expression.kind == :text_builder
      charge
      values = expression.args.map { |arg| evaluate(arg, row, group) }
      types = expression.args.map(&:type)
      if expression.extra[:variadic]
        array = values.pop
        type = types.pop.delete_suffix('[]')
        return nil if array.nil? && expression.value != 'format'
        values.concat(array || [])
        types.concat(Array.new((array || []).length, type))
      end
      result = Functions.call(expression.value, values, context: self, arg_types: types)
      charge(result.bytesize) if expression.value == 'concat' && result && !constant_expression?(expression)
      result
    end
  end
  Engine.prepend(TextBuilders)
end
