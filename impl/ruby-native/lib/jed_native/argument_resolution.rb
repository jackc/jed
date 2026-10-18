# frozen_string_literal: true

module JedNative
  module ArgumentResolution
    module_function
    def reorder(node)
      return node if node[:name].start_with?('json')
      entries = FunctionCatalog::ARGUMENTS[node[:name]]
      return node unless entries || node[:arg_names]
      raise Error.new('42883', 'function does not accept named arguments') unless entries
      provided = node[:args] || []
      names = node[:arg_names] || []
      entries.each do |parameters, families, defaults|
        next if provided.length > parameters.length
        args = Array.new(parameters.length)
        valid = true
        provided.each_with_index do |arg, index|
          position = names[index] ? parameters.index(names[index]) : index
          if position.nil? || args[position]
            valid = false
            break
          end
          args[position] = arg
        end
        next unless valid
        args.each_index do |index|
          if args[index].nil?
            default = index - (parameters.length - defaults.length)
            if default.negative?
              valid = false
              break
            end
            args[index] = { kind: :literal, type: families[index] == 'float' ? 'f64' : 'i64', value: families[index] == 'float' ? defaults[default].to_f : defaults[default].to_i }
          elsif families[index] == 'float' && args[index][:kind] == :literal && args[index][:untyped] && %w[i64 decimal].include?(args[index][:type])
            args[index] = { kind: :literal, type: 'f64', value: args[index][:value].to_f }
          end
        end
        next unless valid
        return node.merge(args: args).reject { |key, _| key == :arg_names }
      end
      raise Error.new('42883', 'function has no matching named argument signature')
    end
  end
  module NamedArguments
    def compile(node, columns)
      node = ArgumentResolution.reorder(node) if node[:kind] == :call
      super(node, columns)
    end
  end
  Engine.prepend(NamedArguments)
  module NamedBindingArguments
    def function(node, scope, wanted) = super(ArgumentResolution.reorder(node), scope, wanted)
  end
  ParameterBindings.prepend(NamedBindingArguments)
end
