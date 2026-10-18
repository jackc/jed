# frozen_string_literal: true

module JedNative
  module JsonMutator
    module_function
    def change(document, path, replacement = nil, mode: :set, create: true, after: false)
      raise Error.new('2202E', 'wrong number of array subscripts') if path.is_a?(ArrayValue) && path.dimensions.length > 1
      root = document.value
      unless root.is_a?(Hash) || root.is_a?(Array)
        raise Error.new('22023', mode == :delete ? 'cannot delete path in scalar' : 'cannot set path in scalar')
      end
      return document if path.empty? || root.empty? && (mode == :delete || mode == :set && !create)
      JsonValue.new(walk(root, path, 0, replacement, mode, create, after))
    end

    def walk(node, path, position, value, mode, create, after)
      step = path[position]
      raise Error.new('22004', "path element at position #{position + 1} is null") if step.nil?
      return node unless node.is_a?(Hash) || node.is_a?(Array)
      last = position + 1 == path.length
      if node.is_a?(Hash)
        return node unless node.key?(step) || last && mode != :delete && create
        result = node.dup
        if last
          raise Error.new('22023', 'cannot replace existing key') if mode == :insert && node.key?(step)
          mode == :delete ? result.delete(step) : result[step] = value
        else
          result[step] = walk(node[step], path, position + 1, value, mode, create, after)
        end
        result
      else
        index = JsonValue.path_index(step)
        raise Error.new('22P02', "path element at position #{position + 1} is not an integer: #{step.inspect}") unless index
        index += node.length if index.negative?
        present = index >= 0 && index < node.length
        return node unless present || last && mode != :delete && create
        result = node.dup
        if last
          if mode == :delete
            result.delete_at(index) if present
          elsif mode == :insert
            index += 1 if after && present
            result.insert([[index, 0].max, result.length].min, value)
          elsif present
            result[index] = value
          elsif index.negative?
            result.unshift(value)
          else
            result << value
          end
        else
          result[index] = walk(node[index], path, position + 1, value, mode, create, after)
        end
        result
      end
    end

    def delete(document, key)
      root = document.value
      if key.is_a?(ArrayValue) && key.dimensions.length > 1
        raise Error.new('2202E', 'wrong number of array subscripts')
      end
      raise Error.new('22023', 'cannot delete from scalar') unless root.is_a?(Array) || root.is_a?(Hash)
      if key.is_a?(Integer)
        raise Error.new('22023', 'cannot delete from object using integer index') if root.is_a?(Hash)
        result = root.dup
        result.delete_at(key)
      else
        keys = key.is_a?(Array) ? key.compact : [key]
        result = root.is_a?(Hash) ? root.reject { |name, _| keys.include?(name) } : root.reject { |item| item.is_a?(String) && keys.include?(item) }
      end
      JsonValue.new(result)
    end
  end

  module JsonOperations
    JSON_OPERATORS = %w[-> ->> #> #>> ? ?| ?& @> <@ || - #-].freeze
    JSON_MUTATORS = %w[jsonb_set jsonb_set_lax jsonb_insert].freeze

    def compile(node, columns)
      if node[:kind] == :binary && JSON_OPERATORS.include?(node[:op])
        left = compile(node[:left], columns)
        right = nil
        if node[:op] == '<@' && left.kind == :literal && left.extra&.dig(:untyped)
          right = compile(node[:right], columns)
          left = json_adapt_argument(left, 'jsonb') if right.type == 'jsonb'
        end
        if %w[json jsonb].include?(left.type)
          op = node[:op]
          right ||= compile(node[:right], columns)
          wanted = case op
          when '#>', '#>>', '#-', '?|', '?&' then 'text[]'
          when '@>', '<@', '||' then 'jsonb'
          when '?' then 'text'
          end
          right = json_adapt_argument(right, wanted) if wanted
          type = %w[? ?| ?& @> <@].include?(op) ? 'boolean' : %w[->> #>>].include?(op) ? 'text' : left.type
          return Expression.new(kind: :json_operator, value: op, type: type, args: [left, right])
        end
        # The ordinary operator still needs the operand that we resolved while
        # selecting the JSON overload. Reusing it keeps left-deep trees linear.
        node = node.merge(left: { kind: :resolved_expression, expression: left })
        node = node.merge(right: { kind: :resolved_expression, expression: right }) if right
      elsif node[:kind] == :call && JSON_MUTATORS.include?(node[:name])
        name = node[:name]
        slots = ['jsonb_in', 'path', 'replacement', name == 'jsonb_insert' ? 'insert_after' : 'create_if_missing', 'null_value_treatment']
        args = []
        node[:args].each_with_index do |arg, i|
          index = node[:arg_names]&.[](i) ? slots.index(node[:arg_names][i]) : i
          raise Error.new('42883', 'invalid JSON mutator argument') unless index && !args[index]
          args[index] = compile(arg, columns)
        end
        maximum = name == 'jsonb_set_lax' ? 5 : 4
        raise Error.new('42883', 'invalid JSON mutator arguments') if args.length > maximum || args.first(3).length < 3 || args.first(3).any?(&:nil?)
        args[3] ||= Expression.new(kind: :literal, type: 'boolean', value: name != 'jsonb_insert')
        args[4] ||= Expression.new(kind: :literal, type: 'text', value: 'use_json_null') if name == 'jsonb_set_lax'
        args = args.each_with_index.map { |arg, i| json_adapt_argument(arg, %w[jsonb text[] jsonb boolean text][i]) }
        return Expression.new(kind: :json_mutator, type: 'jsonb', value: name, args: args)
      end
      super
    end

    def json_adapt_argument(argument, type)
      return argument unless type
      if argument.kind == :literal && %w[text null unknown].include?(argument.type)
        return Expression.new(kind: :literal, type: type, value: cast_value(argument.value, type))
      end
      raise Error.new('42883', 'invalid JSON argument type') unless argument.type == type || argument.type == 'unknown'
      argument
    end

    def evaluate(expression, row, group = nil)
      if expression.kind == :json_operator
        charge
        left, right = expression.args.map { |arg| evaluate(arg, row, group) }
        return nil if left.nil? || right.nil?
        return JsonMutator.delete(left, right) if expression.value == '-'
        return JsonMutator.change(left, right, mode: :delete) if expression.value == '#-'
        result = Types.binary(expression.value, left, right, type: expression.type)
        if %w[->> #>>].include?(expression.value) && left.binary && result && !constant_expression?(expression)
          selected = left.operator(expression.value == '->>' ? '->' : '#>', right)
          charge(result.bytesize) if selected && (selected.value.is_a?(Hash) || selected.value.is_a?(Array))
        end
        charge(JsonValue.payload(result.value)) if expression.value == '||' && !(respond_to?(:constant_expression?) && constant_expression?(expression))
        return result
      elsif expression.kind == :json_mutator
        charge
        target, path, replacement, create, treatment = expression.args.map { |arg| evaluate(arg, row, group) }
        return nil if target.nil? || path.nil? || create.nil?
        mode = expression.value == 'jsonb_insert' ? :insert : :set
        if expression.value == 'jsonb_set_lax'
          message = 'null_value_treatment must be "delete_key", "return_target", "use_json_null", or "raise_exception"'
          raise Error.new('22023', message) if treatment.nil?
          if replacement.nil?
            case treatment
            when 'return_target' then return target
            when 'raise_exception' then raise Error.new('22004', 'JSON value must not be null')
            when 'delete_key' then mode = :delete
            when 'use_json_null' then replacement = JsonValue.new(nil)
            else raise Error.new('22023', message)
            end
          end
        else
          return nil if replacement.nil?
        end
        result = JsonMutator.change(target, path, replacement&.value, mode: mode, create: mode == :insert || create, after: mode == :insert && create)
        charge(JsonValue.payload(result.value)) unless mode == :delete || respond_to?(:constant_expression?) && constant_expression?(expression)
        return result
      end
      super
    end
  end
  Engine.prepend(JsonOperations)
end
