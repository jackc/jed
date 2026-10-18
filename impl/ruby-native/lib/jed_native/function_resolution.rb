# frozen_string_literal: true

module JedNative
  module FunctionResolution
    STRING_INPUTS = %w[text bytea uuid date timestamp timestamptz interval json jsonb jsonpath].freeze
    module_function

    def needed?(node)
      return false unless node[:kind] == :call && FunctionCatalog::SIGNATURES[node[:name]]
      return false if node[:name].start_with?('json') && !%w[json_typeof jsonb_typeof jsonb_array_length].include?(node[:name]) || node[:variadic] || node[:arg_names]
      node[:args].any? { |arg| arg[:kind] == :literal && (arg[:value].nil? || arg[:untyped] && arg[:type] == 'text') } || FunctionCatalog::SIGNATURES[node[:name]].any? { |entry| entry[2] }
    end

    def targets(node, types)
      args = node[:args]
      entries = FunctionCatalog::SIGNATURES[node[:name]]
      candidates = entries.select do |families, _, promote, variadic, defaults|
        next false unless variadic ? args.length >= families.length - 1 : args.length.between?(families.length - defaults, families.length)
        args.each_with_index.all? do |arg, index|
          family = families[[index, families.length - 1].min]
          actual = Functions.family(types[index])
          if arg[:kind] == :literal && arg[:value].nil? && %w[null unknown].include?(types[index])
            true
          elsif arg[:kind] == :literal && arg[:untyped] && types[index] == 'text'
            STRING_INPUTS.include?(family) || family.start_with?('any')
          elsif %w[null unknown].include?(types[index]) || %w[any anycompatible].include?(family)
            true
          elsif %w[anyarray anycompatiblearray].include?(family)
            types[index].end_with?('[]')
          elsif family == 'anyrange'
            Types::RANGES.key?(types[index])
          else
            actual == family || promote && family == 'decimal' && actual == 'integer' && types.any? { |type| type == 'decimal' } ||
              promote && family == 'float' && arg[:kind] == :literal && arg[:untyped] && %w[integer decimal].include?(actual) && types.any? { |type| %w[f32 f64].include?(type) }
          end
        end
      end
      raise Error.new('42883', "function #{node[:name]} has no matching overload") if candidates.empty?
      result = Array.new(args.length)
      args.each_with_index do |arg, index|
        unknown = arg[:kind] == :literal && (arg[:value].nil? && %w[null unknown].include?(types[index]) || arg[:untyped] && types[index] == 'text')
        next unless unknown
        choices = candidates.map { |entry| entry[0][[index, entry[0].length - 1].min] }.uniq
        if choices.include?('text')
          candidates.select! { |entry| entry[0][[index, entry[0].length - 1].min] == 'text' }
          choices = ['text']
        end
        raise Error.new('42725', "function #{node[:name]} is ambiguous") if choices.length > 1
        result[index] = { 'integer' => 'i64', 'float' => 'f64' }.fetch(choices.first, choices.first) unless choices.first.start_with?('any')
      end
      candidates.first[0].each_with_index do |family, index|
        arg = args[index]
        if family == 'float' && arg && arg[:kind] == :literal && arg[:untyped] && %w[i16 i32 i64 decimal].include?(types[index])
          result[index] = 'f64'
        end
      end
      result
    end
  end

  module ResolveFunctionLiterals
    def compile(node, columns)
      if FunctionResolution.needed?(node)
        arguments = node[:args].map { |argument| compile(argument, columns) }
        targets = FunctionResolution.targets(node, arguments.map(&:type))
        arguments.each_with_index do |argument, index|
          next unless targets[index]
          argument.value = cast_value(argument.value, targets[index], source_type: argument.type)
          argument.type = targets[index]
        end
        node = node.merge(args: arguments.map { |argument| { kind: :resolved_expression, expression: argument } })
      end
      super(node, columns)
    end
  end
  Engine.prepend(ResolveFunctionLiterals)

  module ResolveBindingLiterals
    def function(node, scope, wanted)
      if FunctionResolution.needed?(node)
        types = node[:args].map { |arg| expression(arg, scope) }
        targets = FunctionResolution.targets(node, types)
        node = node.merge(args: node[:args].each_with_index.map do |arg, index|
          targets[index] ? { kind: :cast, expr: arg, type: targets[index] } : arg
        end)
      end
      super(node, scope, wanted)
    end
  end
  ParameterBindings.prepend(ResolveBindingLiterals)
end
