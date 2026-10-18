# frozen_string_literal: true

module JedNative
  module RangeConstructors
    def compile(node, columns)
      if node[:kind] == :call && (element = Types::RANGES[Types.normalize(node[:name])])
        args = node[:args]
        raise Error.new('42883', 'range constructor requires two or three arguments') unless args.length.between?(2, 3)
        args = args.each_with_index.map do |arg, index|
          target = index < 2 ? element : 'text'
          if arg[:kind] == :literal && (arg[:untyped] || arg[:value].nil?)
            { kind: :cast, type: target, expr: arg }
          else arg
          end
        end
        return super(node.merge(args: args), columns)
      end
      super
    end
  end
  Engine.prepend(RangeConstructors)

  module RangeBinding
    def function(node, scope, wanted)
      if (element = Types::RANGES[Types.normalize(node[:name])])
        args = node[:args] || []
        raise Error.new('42883', 'range constructor requires two or three arguments') unless args.length.between?(2, 3)
        args.each_with_index do |arg, index|
          target = index < 2 ? element : 'text'
          actual = expression(arg, scope, target)
          literal = arg[:kind] == :literal && (arg[:untyped] || arg[:value].nil?)
          unless literal || !known?(actual) || actual == target || index < 2 && Functions.family(actual) == Functions.family(target)
            raise Error.new('42883', 'range constructor has incompatible arguments')
          end
        end
        return Types.normalize(node[:name])
      end
      super
    end
  end
  ParameterBindings.prepend(RangeBinding)
end
