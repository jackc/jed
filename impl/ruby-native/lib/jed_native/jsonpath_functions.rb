# frozen_string_literal: true

require_relative 'jsonpath_eval'

module JedNative
  module JsonPathFunctions
    NAMES = %w[jsonb_path_exists jsonb_path_match jsonb_path_query jsonb_path_query_array jsonb_path_query_first].freeze
    module_function
    def function?(name) = NAMES.include?(name.delete_suffix('_tz'))
    def volatility(name) = name.end_with?('_tz') ? 'stable' : 'immutable'
    def result_type(name) = %w[jsonb_path_exists jsonb_path_match].include?(name.delete_suffix('_tz')) ? 'boolean' : 'jsonb'
    def call(name, args, context: nil, operator: false, set_returning: false, constant: false)
      return nil if args.any?(&:nil?)
      document, path, variables, silent = args
      document = JsonValue.parse(document, binary: true) unless document.is_a?(JsonValue)
      path = JsonPathValue.parse(path)
      variables = JsonValue.parse(variables, binary: true) unless variables.is_a?(JsonValue)
      base = name.delete_suffix('_tz')
      evaluator = JsonPathEvaluator.new(path, document.value, vars: variables.value, context: context, variable_bases: operator ? 0 : 1)
      result = evaluator.run(exists: base == 'jsonb_path_exists', silent: silent)
      items = result[:items]
      context&.charge(items.length) unless set_returning
      case base
      when 'jsonb_path_exists'
        result[:error] ? nil : !items.empty?
      when 'jsonb_path_match'
        if items.length != 1 || ![nil, true, false].include?(items[0])
          return nil if silent
          raise Error.new('22038', 'JSON path result is not a singleton boolean')
        end
        items[0]
      when 'jsonb_path_query_array'
        context&.charge(JsonValue.payload(items)) unless constant
        JsonValue.new(items)
      when 'jsonb_path_query_first'
        items.empty? ? nil : JsonValue.new(items[0])
      when 'jsonb_path_query'
        items.map { |item| JsonValue.new(item) }
      end
    end
  end

  module JsonPathExpressions
    def compile(node, columns)
      if node[:kind] == :binary && %w[@? @@].include?(node[:op])
        names = node[:op] == '@?' ? 'jsonb_path_exists' : 'jsonb_path_match'
        synthetic = { kind: :call, name: names, args: [node[:left], node[:right], { kind: :literal, type: 'jsonb', value: JsonValue.new({}) }, { kind: :literal, type: 'boolean', value: true }] }
        expression = compile_jsonpath_call(synthetic, columns)
        expression.extra = { operator: true }
        return expression
      end
      return super unless node[:kind] == :call && JsonPathFunctions.function?(node[:name])
      if node[:name].delete_suffix('_tz') == 'jsonb_path_query' && !@jsonpath_source_compile
        raise Error.new('42883', 'JSON path set-returning functions require FROM')
      end
      compile_jsonpath_call(node, columns)
    end

    def compile_jsonpath_call(node, columns)
      arguments = Array.new(4)
      slots = %w[target path vars silent]
      (node[:args] || []).each_with_index do |arg, index|
        name = node[:arg_names]&.[](index)
        position = name ? slots.index(name) : index
        raise Error.new('42883', 'invalid JSON path function arguments') unless position && position < 4 && !arguments[position]
        arguments[position] = compile(arg, columns)
      end
      raise Error.new('42883', 'JSON path target and path are required') unless arguments[0] && arguments[1]
      arguments[2] ||= Expression.new(kind: :literal, type: 'jsonb', value: JsonValue.new({}))
      arguments[3] ||= Expression.new(kind: :literal, type: 'boolean', value: false)
      arguments.each_with_index do |argument, index|
        expected = %w[jsonb jsonpath jsonb boolean][index]
        if argument.kind == :literal && (%w[text null unknown].include?(argument.type) || argument.type == expected)
          argument.value = cast_value(argument.value, expected)
          argument.type = expected
        end
        raise Error.new('42883', 'invalid JSON path function argument type') unless argument.type == expected || argument.type == 'unknown'
      end
      Expression.new(kind: :jsonpath_function, type: JsonPathFunctions.result_type(node[:name]), value: node[:name], args: arguments, extra: {})
    end

    def evaluate(expression, row, group = nil)
      return super unless expression.kind == :jsonpath_function
      charge
      args = expression.args.map { |arg| evaluate(arg, row, group) }
      JsonPathFunctions.call(expression.value, args, context: self, operator: expression.extra[:operator], constant: respond_to?(:constant_expression?) && constant_expression?(expression))
    end

    def source_relation(source)
      call = source[:expr] || source[:call] || source
      unless %i[function call srf].include?(source[:kind]) && %w[jsonb_path_query jsonb_path_query_tz].include?(call[:name])
        return super
      end
      @jsonpath_source_compile = true
      expression = compile(call, [])
      @jsonpath_source_compile = false
      arguments = expression.args.map { |argument| evaluate(argument, []) }
      values = JsonPathFunctions.call(expression.value, arguments, context: self, set_returning: true) || []
      charge(values.length)
      name = source[:columns]&.first || source[:column_aliases]&.first || source[:alias] || call[:name]
      columns = [{ name: name, table: source[:alias] || call[:name], type: 'jsonb' }]
      rows = values.map { |value| [value] }
      if source[:ordinality]
        columns << { name: source[:columns]&.[](1) || 'ordinality', table: source[:alias] || call[:name], type: 'i64' }
        rows.each_with_index { |row, index| row << index + 1 }
      end
      Relation.new(columns: columns, rows: rows)
    ensure
      @jsonpath_source_compile = false
    end
  end
  Engine.prepend(JsonPathExpressions)
end
