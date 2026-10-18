# frozen_string_literal: true

module JedNative
  # PostgreSQL's JSON image depends on the static source type, most visibly for
  # f32 and named records. Keep that metadata through the call's evaluation.
  module JsonImages
    JSON_IMAGES = %w[to_json to_jsonb array_to_json row_to_json json_scalar json_build_array jsonb_build_array json_build_object jsonb_build_object json_object jsonb_object].freeze

    def compile(node, columns)
      return super unless node[:kind] == :call && JSON_IMAGES.include?(node[:name])
      args = node[:args].map { |arg| compile(arg, columns) }
      name = node[:name]
      if %w[json_object jsonb_object].include?(name)
        raise Error.new('42883', 'JSON object requires one or two text arrays') unless args.length.between?(1, 2)
        args = args.map { |arg| json_adapt_argument(arg, 'text[]') }
      elsif !name.include?('_build_')
        maximum = %w[array_to_json row_to_json].include?(name) ? 2 : 1
        raise Error.new('42883', 'invalid JSON function arity') unless args.length.between?(1, maximum)
        raise Error.new('42883', 'array_to_json requires an array') if name == 'array_to_json' && !args[0].type.end_with?('[]') && !%w[null unknown].include?(args[0].type)
        raise Error.new('42804', 'row_to_json requires a record') if name == 'row_to_json' && args[0].type != 'record' && !@types.key?(args[0].type)
      end
      Expression.new(kind: :json_image, value: name, args: args, type: name.start_with?('jsonb') || name == 'to_jsonb' ? 'jsonb' : 'json', extra: node)
    end

    def json_image_text(value, type, expression = nil, pretty: false)
      return 'null' if value.nil?
      return value.to_s if value.is_a?(JsonValue)
      if value.is_a?(Float)
        text = Types.format(value, type)
        return value.finite? ? text : JsonValue.escape(text)
      end
      fields = @types[type]
      if fields || type == 'record' || value.is_a?(CompositeValue)
        names = fields&.map { |field| field[:name] } || (value.field_names if value.is_a?(CompositeValue)) || value.each_index.map { |i| "f#{i + 1}" }
        elements = value.each_with_index.map do |item, i|
          child = expression&.kind == :row ? expression.args[i] : nil
          child_type = fields&.[](i)&.dig(:type) || child&.type || Types.infer(item)
          JsonValue.escape(names[i]) + ':' + json_image_text(item, child_type, child)
        end
        return '{' + elements.join(pretty ? ",\n " : ',') + '}'
      elsif value.is_a?(ArrayValue) || type.end_with?('[]')
        element_type = type.end_with?('[]') ? type[0...-2] : value.element_type
        dimensions = value.is_a?(ArrayValue) ? value.dimensions : [value.length]
        return '[]' if dimensions.empty? || value.empty?
        position = 0
        build = lambda do |depth|
          items = Array.new(dimensions[depth]) do
            if depth + 1 < dimensions.length
              build.call(depth + 1)
            else
              item = value[position]
              position += 1
              json_image_text(item, element_type)
            end
          end
          '[' + items.join(pretty && depth.zero? ? ",\n " : ',') + ']'
        end
        return build.call(0)
      elsif value.is_a?(TimestampValue)
        text = Types.format(value, type).sub(' ', 'T')
        text += ':00' if type == 'timestamptz' && /[+-]\d\d\z/.match?(text)
        return JsonValue.escape(text)
      elsif value.is_a?(DateValue) || value.is_a?(IntervalValue) || value.is_a?(UUID) || value.is_a?(Bytea) || value.is_a?(RangeValue) || value.is_a?(JsonPathValue)
        return JsonValue.escape(Types.format(value, type))
      end
      JsonValue.render(value, spaces: false)
    end

    def json_image_value(value, type, expression = nil, binary: true)
      JsonValue.parse(json_image_text(value, type, expression), binary: binary)
    end

    def json_key_text(value, type)
      if value.is_a?(TimestampValue) || value.is_a?(DateValue) || value.is_a?(IntervalValue)
        JsonValue.parse(json_image_text(value, type)).value
      else
        Types.format(value, type)
      end
    end

    def evaluate(expression, row, group = nil)
      return super unless expression.kind == :json_image
      charge
      values = expression.args.map { |arg| evaluate(arg, row, group) }
      args = expression.args
      name = expression.value
      binary = expression.type == 'jsonb'
      if expression.extra[:variadic]
        return nil if values.last.nil?
        array = values.pop
        expr = args.last
        type = expr.type.delete_suffix('[]')
        args = args[0...-1] + array.map { Expression.new(kind: :literal, type: type) }
        values.concat(array)
      end
      charge(values.length) if name.include?('_build_') || %w[json_object jsonb_object].include?(name)
      if %w[json_object jsonb_object].include?(name)
        return nil if values.any?(&:nil?)
        if values.length == 2
          raise Error.new('2202E', 'mismatched array dimensions') unless values[0].length == values[1].length
          pairs = values[0].zip(values[1])
        else
          raise Error.new('2202E', 'array must have an even number of elements') unless values[0].length.even?
          pairs = values[0].each_slice(2).to_a
        end
        text = '{' + pairs.map do |key, value|
          raise Error.new('22004', 'null JSON object key') if key.nil?
          JsonValue.escape(key) + ' : ' + (value.nil? ? 'null' : JsonValue.escape(value))
        end.join(', ') + '}'
      elsif name.end_with?('_build_array')
        text = '[' + values.each_with_index.map { |value, i| json_image_text(value, args[i].type, args[i]) }.join(', ') + ']'
      elsif name.end_with?('_build_object')
        raise Error.new('22023', 'argument list must have even number of elements') unless values.length.even?
        pairs = []
        0.step(values.length - 1, 2) do |i|
          key = values[i]
          raise Error.new('22023', 'JSON object key must not be null') if key.nil?
          raise Error.new('22023', 'JSON object key must be scalar') if key.is_a?(Array) || key.is_a?(JsonValue)
          pairs << JsonValue.escape(json_key_text(key, args[i].type)) + ' : ' + json_image_text(values[i + 1], args[i + 1].type, args[i + 1])
        end
        text = '{' + pairs.join(', ') + '}'
      else
        return nil if values[0].nil? || values.length > 1 && values[1].nil?
        text = json_image_text(values[0], args[0].type, args[0], pretty: values[1] == true)
      end
      result = JsonValue.parse(text, binary: binary)
      unless respond_to?(:constant_expression?) && constant_expression?(expression)
        charge(defined?(LogicalMemory) ? LogicalMemory.payload(result, expression.type) : binary ? JsonValue.payload(result.value) : text.bytesize)
      end
      result
    end
  end
  Engine.prepend(JsonImages)
end
