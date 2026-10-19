# frozen_string_literal: true

require 'digest'
require 'json'
require File.expand_path('impl/ruby-native/lib/jed_native', Dir.pwd)

def source_hash
  Digest::SHA256.hexdigest(Dir['impl/ruby-native/lib/**/*.rb'].sort.map { |path| File.binread(path) }.join)
end

def words(alphabet, maximum)
  output = ['']
  level = ['']
  maximum.times do
    level = level.flat_map { |prefix| alphabet.map { |character| prefix + character } }
    output.concat(level)
  end
  output
end

def resolve(pattern, operator, parameter: false)
  engine = JedNative::Engine.new
  engine.instance_variable_set(:@native_compilation_disabled, true)
  engine.instance_variable_set(:@native_binding_types, ['text']) if parameter
  subject = parameter ? '$1' : 's'
  statement = JedNative::Statement.new("SELECT #{subject} #{operator} '#{pattern.gsub("'", "''")}'")
  expression = engine.compile(statement.ast[:columns].first[:expr], [{ name: 's', type: 'text' }])
  [engine, expression, JedNative::CompiledExpression.build(expression, types: {})]
end

def outcome(engine)
  engine.instance_variable_set(:@cost, 0)
  engine.instance_variable_set(:@lifetime_cost, 0)
  result = yield
  [result, engine.cost, engine.lifetime_cost]
rescue StandardError => failure
  [[failure.class.name, failure.respond_to?(:sqlstate) ? failure.sqlstate : failure.message], engine.cost, engine.lifetime_cost]
end

report = { source_sha256: source_hash, patterns: 0, admitted: 0, rejected: 0, comparisons: 0, unlabeled_param_rejections: 0,
           runtime_guard_comparisons: 0, failures: [] }
patterns = words(['a', '%', '_', '\\', '😀'], 4)
subjects = words(['a', '%', '_', '\\', '😀'], 3) + [nil]
%w[like].push('not like').each do |operator|
  patterns.each do |pattern|
    report[:patterns] += 1
    engine, expression, kernel = resolve(pattern, operator)
    unless kernel
      report[:rejected] += 1
      next
    end
    report[:admitted] += 1
    subjects.each do |subject|
      reference = outcome(engine) { engine.evaluate(expression, [subject]) }
      compiled = outcome(engine) { kernel.call(engine, [subject], []) }
      report[:comparisons] += 1
      if reference != compiled
        report[:failures] << { operator: operator, pattern: pattern, subject: subject, reference: reference, compiled: compiled }
        raise 'exhaustive outcome mismatch' if report[:failures].length >= 8
      end
    end
  end
end

factories = {
  utf8: ->(events) { 'abc'.dup },
  invalid_utf8: ->(events) { "\xFFabc".b.force_encoding(Encoding::UTF_8) },
  utf16: ->(events) { 'abc'.encode(Encoding::UTF_16LE) },
  ascii: ->(events) { 'abc'.encode(Encoding::US_ASCII) },
  binary_ascii: ->(events) { 'abc'.b },
  binary_unicode: ->(events) { '😀'.b },
  bytea: ->(events) { JedNative::Bytea.new('abc') },
  singleton_chars: ->(events) {
    value = 'abc'.dup
    value.define_singleton_method(:chars) { events << :chars; ['z'] }
    value
  },
  singleton_predicates: ->(events) {
    value = 'abc'.dup
    %i[== start_with? end_with? include?].each do |method|
      value.define_singleton_method(method) { |*| events << method; raise 'unexpected predicate dispatch' }
    end
    value
  },
  subclass_chars: ->(events) {
    klass = Class.new(String) { define_method(:chars) { events << :chars; ['z'] } }
    klass.new('abc')
  }
}
[false, true].each do |parameter|
  ['', '%', 'a', 'a%', '%c', '%b%', '😀', '%😀%', '\\%', '\\\\%'].each do |pattern|
    ['like', 'not like'].each do |operator|
      engine, expression, kernel = resolve(pattern, operator, parameter: parameter)
      if parameter
        raise 'unlabeled parameter unexpectedly admitted' if kernel
        report[:unlabeled_param_rejections] += 1
        next
      end
      raise "guard fixture was not admitted: #{[parameter, pattern, operator].inspect}" unless kernel
      factories.each do |name, factory|
        reference_events, compiled_events = [], []
        value = factory.call(reference_events)
        engine.instance_variable_set(:@params, [value])
        reference = outcome(engine) { engine.evaluate(expression, [value]) }
        value = factory.call(compiled_events)
        compiled = outcome(engine) { kernel.call(engine, [value], [value]) }
        report[:runtime_guard_comparisons] += 1
        next if reference == compiled && reference_events == compiled_events
        report[:failures] << { guard: name, parameter: parameter, operator: operator, pattern: pattern,
                               reference: reference, compiled: compiled,
                               reference_events: reference_events, compiled_events: compiled_events }
      end
    end
  end
end
report[:final_source_sha256] = source_hash
report[:passed] = report[:failures].empty? && report[:source_sha256] == report[:final_source_sha256]
File.write('/tmp/jed-like-classifier-audit.json', JSON.pretty_generate(report) + "\n")
puts JSON.generate(report)
raise 'classifier audit failed' unless report[:passed]
