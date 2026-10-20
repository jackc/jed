# frozen_string_literal: true

# Independent, opt-in classifier check. The reference evaluator is unchanged;
# this audit does not use the compiler's classifier to decide expected admission.
require 'digest'
require 'json'
require 'optparse'

options = { source: File.expand_path('impl/ruby-native', Dir.pwd),
            output: '/tmp/jed-literal-like-classifier-audit.json' }
OptionParser.new do |parser|
  parser.on('--source PATH') { |path| options[:source] = File.expand_path(path) }
  parser.on('--output PATH') { |path| options[:output] = File.expand_path(path) }
end.parse!
require File.join(options.fetch(:source), 'lib/jed_native')

def source_hash(source)
  Digest::SHA256.hexdigest(Dir[File.join(source, 'lib/**/*.rb')].sort.map { |path| File.binread(path) }.join)
end

def words(alphabet, maximum)
  result, level = [''], ['']
  maximum.times do
    level = level.flat_map { |prefix| alphabet.map { |character| prefix + character } }
    result.concat(level)
  end
  result
end

def expected_admission(pattern)
  tokens = []
  escaping = false
  pattern.each_char do |character|
    if escaping
      tokens << :literal
      escaping = false
    elsif character == '\\'
      escaping = true
    elsif character == '_'
      tokens << :one
    elsif character == '%'
      tokens << :many
    else
      tokens << :literal
    end
  end
  return false if escaping
  first_many = tokens.index(:many)
  !tokens.include?(:one) || !first_many || tokens.drop(first_many).all? { |kind| kind == :many }
end

def resolve(pattern, operator, body: nil, parameter: false)
  current = JedNative::Engine.new
  current.instance_variable_set(:@native_compilation_disabled, true)
  current.instance_variable_set(:@native_binding_types, ['text']) if parameter
  subject = parameter ? '$1' : 's'
  body ||= "#{subject} #{operator} '#{pattern.gsub("'", "''")}'"
  ast = JedNative::Statement.new("SELECT #{body}").ast
  columns = [{ name: 's', type: 'text' }, { name: 'id', type: 'i32' }]
  expression = current.compile(ast[:columns].first[:expr], columns)
  [current, expression, JedNative::CompiledExpression.build(expression, types: {})]
end

def outcome(current, limit: 0, lifetime: false)
  current.instance_variable_set(:@cost, 0)
  current.instance_variable_set(:@lifetime_cost, 0)
  current.max_cost = lifetime ? 0 : limit
  current.lifetime_max_cost = lifetime ? limit : 0
  answer = [:value, yield]
  [answer, current.cost, current.lifetime_cost]
rescue StandardError => error
  [[:error, error.class.name, error.respond_to?(:sqlstate) ? error.sqlstate : nil, error.message], current.cost, current.lifetime_cost]
end

def fallback_projection(current, expression)
  ast = JedNative::Statement.new('SELECT id + 1').ast
  companion = current.compile(ast[:columns].first[:expr], [{ name: 's', type: 'text' }, { name: 'id', type: 'i32' }])
  kernel = JedNative::CompiledExpression.build_many([expression, companion], types: {})
  raise 'unsupported root lost projection fallback' unless kernel
  [kernel, companion]
end

report = { schema: 1, script_sha256: Digest::SHA256.file(__FILE__).hexdigest,
           source: options.fetch(:source), source_sha256: source_hash(options.fetch(:source)),
           runtime: RUBY_DESCRIPTION, yjit: defined?(RubyVM::YJIT) && RubyVM::YJIT.enabled?,
           zjit: defined?(RubyVM::ZJIT) && RubyVM::ZJIT.enabled?,
           patterns: 0, admitted: 0, rejected: 0, comparisons: 0, fallback_comparisons: 0,
           runtime_guard_comparisons: 0, prefix_comparisons: 0, unlabeled_param_rejections: 0,
           failures: [] }
digest = Digest::SHA256.new
record = lambda do |detail, reference, compiled|
  digest.update(JSON.generate([detail, reference, compiled]))
  if reference != compiled
    report[:failures] << detail.merge(reference: reference, compiled: compiled)
    raise 'classifier audit mismatch limit' if report[:failures].length >= 8
  end
end

patterns = words(['a', '%', '_', '\\', '😀'], 4)
patterns.concat(['row_5%', '_o_', '___%', '_😀_', 'a__b%', 'a\\__%', 'a\\%_%', '\\\\_%',
                 'a%b%c', 'a%%b%c', '%aa%aa', '%a%a%', '😀%x%é', 'a\\%b%c', 'a%\\_%c',
                 'a\\', '%a\\', '_\\', 'a%_b', '%a_b%', "a\n_b%"])
subjects = words(['a', '%', '_', '\\', '😀'], 3)
subjects.concat([nil, 'row05', 'row😀5tail', 'rowé5', 'bob', 'é', '中', '😀', 'é',
                 'a😀b', 'aéb', 'a%bzc', '😀axbé', 'aaaa', 'axbc', "a\u0000b", "a\nb"])
%w[like].push('not like').each do |operator|
  patterns.uniq.each do |pattern|
    report[:patterns] += 1
    current, expression, kernel = resolve(pattern, operator)
    admitted = !kernel.nil?
    expected = expected_admission(pattern)
    record.call({ admission: true, operator: operator, pattern: pattern }, expected, admitted)
    if admitted
      report[:admitted] += 1
    else
      report[:rejected] += 1
      fallback, companion = fallback_projection(current, expression)
    end
    subjects.uniq.each do |subject|
      reference = outcome(current) do
        value = current.evaluate(expression, [subject, 0])
        current.evaluate(companion, [subject, 0]) unless admitted
        value
      end
      compiled = outcome(current) do
        admitted ? kernel.call(current, [subject, 0], []) : fallback.call(current, [subject, 0], []).fetch(0)
      end
      report[admitted ? :comparisons : :fallback_comparisons] += 1
      record.call({ operator: operator, pattern: pattern, subject: subject, admitted: admitted }, reference, compiled)
    end
  end
end

factories = {
  utf8: ->(events) { 'a😀xbc'.dup },
  invalid_utf8: ->(events) { "\xFFabc".b.force_encoding(Encoding::UTF_8) },
  utf16: ->(events) { 'abc'.encode(Encoding::UTF_16LE) },
  ascii: ->(events) { 'abc'.encode(Encoding::US_ASCII) },
  binary_ascii: ->(events) { 'abc'.b },
  binary_unicode: ->(events) { '😀'.b },
  bytea: ->(events) { JedNative::Bytea.new('abc') },
  uuid: ->(events) { JedNative::UUID.new('00000000-0000-0000-0000-000000000000') },
  subclass_chars: ->(events) {
    Class.new(String) { define_method(:chars) { events << :chars; ['z'] } }.new('abc')
  },
  bytea_chars: ->(events) {
    Class.new(JedNative::Bytea) { define_method(:chars) { events << :chars; ['a', 'b', 'c'] } }.new('wrong')
  },
  singleton_operations: ->(events) {
    value = String.new('abc')
    %i[chars getbyte byteindex start_with? end_with? include?].each do |name|
      value.define_singleton_method(name) { |*| events << name; raise 'singleton method escaped C normalization' }
    end
    value.freeze
  },
  transient_label: ->(events) {
    value = JedNative::CollatedText.new('abc', JedNative::CollationLabel.new(conflict: true))
    value.define_singleton_method(:getbyte) { |*| events << :getbyte; raise 'old wrapper escaped C normalization' }
    value.freeze
  }
}
['', '%', 'a%', '_', 'a_c', '_%', 'a_%', 'a%b', 'a%b%c', '%a%b%', '😀_'].each do |pattern|
  ['like', 'not like'].each do |operator|
    current, expression, kernel = resolve(pattern, operator)
    raise 'guard fixture rejected' unless kernel
    _, _, parameter_kernel = resolve(pattern, operator, parameter: true)
    raise 'unlabeled parameter unexpectedly admitted' if parameter_kernel
    report[:unlabeled_param_rejections] += 1
    factories.each do |name, factory|
      reference_events, compiled_events = [], []
      value = factory.call(reference_events)
      reference = outcome(current) { current.evaluate(expression, [value, 0]) }
      value = factory.call(compiled_events)
      compiled = outcome(current) { kernel.call(current, [value, 0], []) }
      report[:runtime_guard_comparisons] += 1
      record.call({ guard: name, pattern: pattern, operator: operator },
                  [reference, reference_events], [compiled, compiled_events])
    end
  end
end

# These branches exercise different small charge and exception prefixes without
# changing the reference evaluator. Trailing escapes remain whole-root fallbacks.
bodies = ["s LIKE 'a_b%' AND 1 / id > 0", "s NOT LIKE 'a%b%c' OR 1 / id > 0",
          "CASE WHEN s LIKE '_%' THEN 7 ELSE 1 / id END", "s LIKE 'a\\'",
          "s LIKE 'a%_b' AND 1 / id > 0"]
bodies.each do |body|
  current, expression, kernel = resolve('', 'like', body: body)
  fallback, companion = fallback_projection(current, expression) unless kernel
  [[nil, 0], ['', 0], ['a', 0], ['ax', 0], ['a😀bc', 0], ['a😀bc', 2], ['other', 0]].each do |row|
    (0..8).each do |limit|
      [false, true].each do |lifetime|
        reference = outcome(current, limit: limit, lifetime: lifetime) do
          value = current.evaluate(expression, row)
          current.evaluate(companion, row) unless kernel
          value
        end
        compiled = outcome(current, limit: limit, lifetime: lifetime) do
          kernel ? kernel.call(current, row, []) : fallback.call(current, row, []).fetch(0)
        end
        report[:prefix_comparisons] += 1
        record.call({ body: body, row: row, limit: limit, lifetime: lifetime }, reference, compiled)
      end
    end
  end
end

report[:outcomes_sha256] = digest.hexdigest
report[:final_source_sha256] = source_hash(options.fetch(:source))
report[:passed] = report[:failures].empty? && report[:source_sha256] == report[:final_source_sha256]
File.write(options.fetch(:output), JSON.pretty_generate(report) + "\n")
puts JSON.generate(report)
raise 'classifier audit failed' unless report[:passed]
