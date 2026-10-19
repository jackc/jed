# frozen_string_literal: true
require 'json'

class Base
  def call(name, args, context: nil, arg_types: nil)
    args
  end
  def cast_value(value, type, source_type: nil)
    value
  end
end

bare = Class.new(Base)
explicit = Class.new(Base)
6.times do
  bare.prepend(Module.new do
    def call(name, args, context: nil, arg_types: nil)
      super
    end
  end)
  explicit.prepend(Module.new do
    def call(name, args, context: nil, arg_types: nil)
      super(name, args, context: context, arg_types: arg_types)
    end
  end)
end
5.times do
  bare.prepend(Module.new do
    def cast_value(value, type, source_type: nil)
      super
    end
  end)
  explicit.prepend(Module.new do
    def cast_value(value, type, source_type: nil)
      super(value, type, source_type: source_type)
    end
  end)
end
args, types = [1, 2].freeze, %w[i64 i64].freeze
results = {}
{ bare: bare.new, explicit: explicit.new }.each do |name, target|
  2000.times { target.call('greatest', args, context: target, arg_types: types); target.cast_value(7, 'i64', source_type: 'i16') }
  GC.start
  before = GC.stat(:total_allocated_objects)
  20_000.times { target.call('greatest', args, context: target, arg_types: types) }
  calls = GC.stat(:total_allocated_objects) - before
  before = GC.stat(:total_allocated_objects)
  20_000.times { target.cast_value(7, 'i64', source_type: 'i16') }
  casts = GC.stat(:total_allocated_objects) - before
  results[name] = { call_allocations: calls / 20_000.0, cast_allocations: casts / 20_000.0 }
end
puts JSON.pretty_generate(runtime: RUBY_DESCRIPTION, iterations: 20_000, wrappers: { calls: 6, casts: 5 }, results: results)
