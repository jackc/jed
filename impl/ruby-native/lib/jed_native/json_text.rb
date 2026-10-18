# frozen_string_literal: true

module JedNative
  class JsonValue
    def subtree(node, text: false)
      source = to_s[node[:start]...node[:finish]]
      if text
        value = JsonParser.new(source).parse
        value.nil? ? nil : value.is_a?(String) ? value : source
      else
        JsonValue.parse(source)
      end
    end

    def verbatim_operator(op, key)
      parts = op.start_with?('#') ? key : [key]
      return nil if parts.any?(&:nil?)
      decoded_value
      root = tree
      if parts.empty?
        if op.end_with?('>>')
          return to_s[0...root[:finish]] if root[:kind] == :scalar && !root[:value].is_a?(String) && !root[:value].nil?
          return subtree(root, text: true)
        end
        start = root[:kind] == :scalar ? 0 : root[:start]
        return JsonValue.parse(to_s[start...root[:finish]])
      end
      indices = {}
      find_arrays = lambda do |node, depth|
        indices[depth] ||= node[:children].length if node[:kind] == :array
        children = node[:kind] == :object ? node[:children].map { |entry| entry[1] } : node[:children]
        children.each { |child| find_arrays.call(child, depth + 1) }
      end
      find_arrays.call(root, 0) if op.start_with?('#')
      matches = [root]
      parts.each_with_index do |part, depth|
        matches = matches.flat_map do |node|
          if node[:kind] == :object && part.is_a?(String)
            node[:children].select { |entry| JsonParser.new(to_s[entry[2]...entry[3]]).parse == part }.map { |entry| entry[1] }
          elsif node[:kind] == :array
            index = part.is_a?(Integer) ? part : op.start_with?('#') ? JsonValue.path_index(part) : nil
            next [] unless index
            index += op.start_with?('#') ? indices.fetch(depth, node[:children].length) : node[:children].length if index.negative?
            index >= 0 && index < node[:children].length ? [node[:children][index]] : []
          else []
          end
        end
      end
      matches.empty? ? nil : subtree(matches.last, text: op.end_with?('>>'))
    end

    def unique_keys?
      decoded_value
      check = lambda do |node|
        if node[:kind] == :object
          names = node[:children].map { |entry| JsonParser.new(to_s[entry[2]...entry[3]]).parse }
          names.uniq.length == names.length && node[:children].all? { |entry| check.call(entry[1]) }
        else
          node[:children].all? { |child| check.call(child) }
        end
      end
      check.call(tree)
    end
  end

  module JsonConstructors
    def compile(node, columns)
      if node[:kind] == :json_constructor || node[:kind] == :is_json
        arg = compile(node[:expr], columns)
        raise Error.new('42804', 'JSON expression requires JSON or text') unless %w[text json jsonb null unknown].include?(arg.type)
        return Expression.new(kind: node[:kind], type: node[:kind] == :is_json ? 'boolean' : 'json', args: [arg], extra: node)
      end
      super
    end

    def evaluate(expression, row, group = nil)
      if expression.kind == :json_constructor
        charge
        value = evaluate(expression.args[0], row, group)
        return nil if value.nil?
        value = JsonValue.parse(value)
        raise Error.new('22030', 'duplicate JSON object key') if expression.extra[:unique] && !value.unique_keys?
        return value
      elsif expression.kind == :is_json
        charge
        value = evaluate(expression.args[0], row, group)
        return nil if value.nil?
        begin
          json = value.is_a?(JsonValue) ? value : JsonValue.parse(value)
          kind = expression.extra[:json_kind]
          answer = !kind || kind == 'value' || kind == json.type_name || kind == 'scalar' && !%w[array object].include?(json.type_name)
          answer &&= json.unique_keys? if expression.extra[:unique]
        rescue Error
          answer = false
        end
        return expression.extra[:negated] ? !answer : answer
      end
      super
    end
  end
  Engine.prepend(JsonConstructors)
end
