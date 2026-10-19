# frozen_string_literal: true

require 'weakref'
require_relative '../lib/jed_native'
require_relative '../lib/jed_native/compiled_expression'

# The shared corpus owns SQL behavior. These tests cover the runtime compiler's
# internal boundary: exact interpreter accounting, trusted generated source,
# delayed row faults, bounded compilation, and the objects retained by code.
module NativeCompiledExpressionTests
  module_function

  def assert(condition, message) = (raise(message) unless condition)

  def engine(params = [], types = [])
    instance = JedNative::Engine.new
    instance.instance_variable_set(:@params, params)
    instance.instance_variable_set(:@native_binding_types, types)
    instance.instance_variable_set(:@native_compilation_disabled, true)
    instance
  end

  def resolve(sql, types: [], columns: [], filter: false)
    instance = engine(Array.new(types.length), types)
    statement = JedNative::Statement.new(sql).ast
    expression = filter ? instance.compile_filter(statement[:where], columns) :
      instance.compile(statement[:columns].first[:expr], columns)
    [expression, instance.instance_variable_get(:@types)]
  end

  def build(expression, types = {})
    JedNative::CompiledExpression.build(expression, types: types)
  end

  class ProbeRow
    attr_reader :reads

    def initialize(values, failure = nil)
      @values, @failure, @reads = values, failure, []
    end

    def [](index)
      @reads << index
      raise JedNative::Error.new('XX001', 'deferred column failure') if index == @failure
      @values[index]
    end
  end

  def outcome(expression, kernel, params, types, values, failure, max_cost, lifetime_max_cost, lifetime)
    instance = engine(params, types)
    instance.max_cost = max_cost
    instance.lifetime_max_cost = lifetime_max_cost
    instance.instance_variable_set(:@lifetime_cost, lifetime)
    row = ProbeRow.new(values, failure)
    begin
      value = kernel ? kernel.call(instance, row, params) : instance.evaluate(expression, row)
      result = [:value, value]
    rescue JedNative::Error => error
      result = [:error, error.sqlstate, error.message]
    end
    [*result, instance.cost, instance.lifetime_cost, row.reads]
  end

  def compare(sql, params: [], types: [], columns: [], values: [], failure: nil, filter: false, expected: nil, resolved: nil)
    expression, registry = resolved ? [resolved, {}] : resolve(sql, types: types, columns: columns, filter: filter)
    kernel = build(expression, registry)
    assert(kernel, "supported expression was not compiled: #{sql}")
    plain = outcome(expression, nil, params, types, values, failure, 0, 0, 0)
    compiled = outcome(expression, kernel, params, types, values, failure, 0, 0, 0)
    assert(compiled == plain, "compiled expression changed #{sql}: #{compiled.inspect} != #{plain.inspect}")
    assert(plain[0, expected.length] == expected, "unexpected reference result for #{sql}: #{plain.inspect}") if expected

    # Check every guard boundary, including the exact first disallowed cost,
    # with an existing lifetime balance and with both ceilings tied.
    total = plain[-3]
    (1..total + 1).each do |limit|
      [[limit, 0, 0], [0, 7 + limit, 7], [limit, 7 + limit, 7]].each do |statement_limit, lifetime_limit, prior|
        args = [params, types, values, failure, statement_limit, lifetime_limit, prior]
        reference = outcome(expression, nil, *args)
        actual = outcome(expression, kernel, *args)
        assert(actual == reference,
          "compiled guard changed #{sql} at #{[statement_limit, lifetime_limit, prior].inspect}: #{actual.inspect} != #{reference.inspect}")
      end
    end
    assert(kernel.source_bytes <= JedNative::CompiledExpression::MAX_SOURCE_BYTES, 'generated source exceeded its limit')
    assert(kernel.node_count <= JedNative::CompiledExpression::MAX_NODES, 'generated node count exceeded its limit')
    assert(kernel.pool.length <= JedNative::CompiledExpression::MAX_POOL, 'generated pool exceeded its limit')
    assert(kernel.pool.frozen?, 'generated data pool is mutable')
    kernel
  end

  def integer_kernels
    compare('SELECT abs($1::i64) + 7', params: [-9], types: ['i64'], expected: [:value, 16])
    %w[i16 i32 i64].each do |type|
      bits = JedNative::Types::INTEGER_BITS.fetch(type)
      minimum, maximum = -(1 << (bits - 1)), (1 << (bits - 1)) - 1
      %w[+ - * = <> < <= > >=].each do |operator|
        [[3, -7], [-3, 7], [nil, 2], [2, nil], [nil, nil]].each do |params|
          compare("SELECT $1 #{operator} $2", params: params, types: [type, type])
        end
      end
      %w[/ %].each do |operator|
        [[7, 3], [-7, 3], [7, -3], [-7, -3], [nil, 0], [1, nil], [1, 0], [minimum, -1]].each do |params|
          compare("SELECT $1 #{operator} $2", params: params, types: [type, type])
        end
      end
      %w[+ -].each do |operator|
        [nil, minimum, maximum, -7, 0].each do |value|
          compare("SELECT #{operator}$1", params: [value], types: [type])
        end
      end
      complement = JedNative::Expression.new(kind: :unary, type: type, value: '~', args: [
        JedNative::Expression.new(kind: :param, type: type, value: 0)
      ])
      [nil, minimum, maximum, -7, 0].each do |value|
        compare('resolved bitwise complement', params: [value], types: [type], resolved: complement)
      end
      [minimum, nil, -1, 0, maximum].each do |value|
        compare('SELECT abs($1)', params: [value], types: [type])
      end
      compare('SELECT $1 + $2', params: [maximum, 1], types: [type, type], expected: [:error, '22003'])
      compare('SELECT $1 - $2', params: [minimum, 1], types: [type, type], expected: [:error, '22003'])
      compare('SELECT $1 * $2', params: [maximum, 2], types: [type, type], expected: [:error, '22003'])
      compare("SELECT $1::#{type}", params: [maximum], types: ['i64'])
      compare("SELECT $1::#{type}", params: [nil], types: ['i64'])
      compare("SELECT $1::#{type}", params: [maximum + 1], types: ['i64'], expected: [:error, '22003']) unless type == 'i64'
    end
    compare('SELECT $1 + (10 / $2)', params: [nil, 0], types: %w[i64 i64], expected: [:error, '22012'])
    compare('SELECT -7 / 3', expected: [:value, -2])
    compare('SELECT -7 % 3', expected: [:value, -1])
    compare('SELECT 7 % -3', expected: [:value, 1])
  end

  def boolean_control_flow
    [true, false, nil].repeated_permutation(2) do |params|
      %w[and or].each do |operator|
        compare("SELECT $1 #{operator} $2", params: params, types: %w[boolean boolean])
        compare("SELECT 1 WHERE $1 #{operator} $2", params: params, types: %w[boolean boolean], filter: true)
      end
    end
    [true, false, nil].each do |value|
      compare('SELECT NOT $1', params: [value], types: ['boolean'])
      ['is null', 'is not null', 'is true', 'is false', 'is not true', 'is not false', 'is unknown', 'is not unknown'].each do |operator|
        compare("SELECT $1 #{operator}", params: [value], types: ['boolean'])
      end
    end
    compare('SELECT FALSE AND 1 / 0 > 0', expected: [:value, false])
    compare('SELECT TRUE OR 1 / 0 > 0', expected: [:value, true])
    compare('SELECT NULL::boolean AND 1 / 0 > 0', expected: [:error, '22012'])
    compare('SELECT 1 WHERE NULL::boolean AND 1 / 0 > 0', filter: true, expected: [:value, nil])
    compare('SELECT 1 WHERE (NULL::boolean OR FALSE) AND 1 / 0 > 0', filter: true, expected: [:value, nil])
    compare('SELECT 1 WHERE (NULL::boolean AND 1 / 0 > 0) OR FALSE', filter: true, expected: [:error, '22012'])

    [true, false, nil].each do |condition|
      compare('SELECT CASE WHEN $1 THEN $2 + 1 ELSE $3 - 1 END',
        params: [condition, 7, 4], types: %w[boolean i64 i64])
      compare('SELECT CASE WHEN $1 THEN FALSE ELSE TRUE END', params: [condition], types: ['boolean'])
    end
    compare('SELECT CASE WHEN TRUE THEN 7::i64 ELSE 1 / 0 END', expected: [:value, 7])
    compare('SELECT CASE WHEN FALSE THEN 1 / 0 END', expected: [:value, nil])
    compare('SELECT CASE WHEN FALSE THEN 1 / 0 WHEN TRUE THEN 8 ELSE 1 / 0 END', expected: [:value, 8])
    compare('SELECT coalesce(7::i64, 1 / 0)', expected: [:value, 7])
    compare('SELECT coalesce(NULL::i64, NULL::i64, 9::i64)', expected: [:value, 9])
    %w[nullif greatest least].each do |name|
      [[nil, nil], [nil, 4], [4, nil], [4, 4], [-4, 4], [4, -4]].each do |params|
        compare("SELECT #{name}($1, $2)", params: params, types: %w[i64 i64])
      end
      compare("SELECT #{name}(NULL::i64, 1 / 0)", expected: [:error, '22012'])
    end
    compare('SELECT $1 BETWEEN 1 AND 3', params: [nil], types: ['i64'])
    compare('SELECT $1 NOT BETWEEN 1 AND 3', params: [2], types: ['i64'])
  end

  def lazy_rows_and_parameters
    integers = [{ name: 'a', type: 'i64' }, { name: 'b', type: 'i64' }]
    booleans = [{ name: 'a', type: 'boolean' }, { name: 'b', type: 'boolean' }]
    compare('SELECT a + b', columns: integers, values: [nil, 2], failure: 1, expected: [:error, 'XX001'])
    compare('SELECT a + b * 2', columns: integers, values: [7, 2], failure: 1, expected: [:error, 'XX001'])
    compare('SELECT a AND b', columns: booleans, values: [false, true], failure: 1, expected: [:value, false])
    compare('SELECT a AND b', columns: booleans, values: [nil, true], failure: 1, expected: [:error, 'XX001'])
    compare('SELECT 1 WHERE a AND b', columns: booleans, values: [nil, true], failure: 1, filter: true, expected: [:value, nil])
    compare('SELECT coalesce(a, b)', columns: integers, values: [3, nil], failure: 1, expected: [:value, 3])
    compare('SELECT greatest(a, b)', columns: integers, values: [3, nil], failure: 1, expected: [:error, 'XX001'])
    # A contextual parameter must still narrow at use, even if its Ruby value
    # happens to fit the wider input type used by another part of a plan.
    expression = JedNative::Expression.new(kind: :param, type: 'i16', value: 0, extra: { context_type: true })
    kernel = build(expression)
    assert(kernel, 'contextual integer parameter was not compiled')
    [nil, -32_768, 32_767, 32_768].each do |value|
      args = [[value], ['i64'], [], nil, 0, 0, 0]
      assert(outcome(expression, kernel, *args) == outcome(expression, nil, *args), 'contextual parameter lost its narrowing check')
    end
  end

  def trusted_source
    marker = 'x"; $jed_compiled_source_escape = true; #'
    quoted = '"' + marker.gsub('"', '""') + '"'
    columns = [{ name: marker, type: 'i64' }]
    kernel = compare("SELECT #{quoted} + 1", columns: columns, values: [8], expected: [:value, 9])
    assert(!kernel.source.include?(marker), 'SQL identifier reached generated source')
    assert(!defined?($jed_compiled_source_escape), 'SQL identifier escaped into Ruby execution')
    literal = JedNative::Expression.new(kind: :literal, type: 'text', value: marker)
    assert(build(literal).nil?, 'string literal crossed the primitive compiler boundary')
    malicious = Object.new
    def malicious.inspect = raise('compiler inspected an unsupported SQL value')
    def malicious.to_s = raise('compiler stringified an unsupported SQL value')
    assert(build(JedNative::Expression.new(kind: :literal, type: 'i64', value: malicious)).nil?,
      'nonprimitive literal reached generated source')
    assert(!defined?($jed_compiled_source_escape), 'unsupported literal escaped into Ruby execution')
  end

  def fused_projections
    types = %w[i64 text i64]
    expressions = ['SELECT $1 + 1', 'SELECT upper($2)', 'SELECT 20 / $3'].map do |sql|
      resolve(sql, types: types).first
    end
    kernel = JedNative::CompiledExpression.build_many(expressions, types: {})
    assert(kernel, 'mixed projections did not compile their supported roots')
    reference = lambda do |instance, row, _params|
      expressions.map { |expression| instance.evaluate(expression, row) }
    end
    [[7, 'safe', 2], [nil, nil, 4], [7, 'early', 0]].each do |params|
      plain = outcome(nil, reference, params, types, [], nil, 0, 0, 0)
      assert(outcome(nil, kernel, params, types, [], nil, 0, 0, 0) == plain,
        'mixed projection changed value/error order')
      (1..plain[-3] + 1).each do |limit|
        [[limit, 0, 0], [0, limit + 7, 7]].each do |statement_limit, lifetime_limit, prior|
          args = [params, types, [], nil, statement_limit, lifetime_limit, prior]
          assert(outcome(nil, kernel, *args) == outcome(nil, reference, *args),
            'mixed projection changed its interpreter fallback cost prefix')
        end
      end
    end
    assert(JedNative::CompiledExpression.build_many([expressions[1]], types: {}).nil?,
      'unsupported-only projection created redundant generated code')
  end

  def bounded_fallback
    unsupported = ["SELECT 'text'", 'SELECT 1.25::decimal', 'SELECT 1::f64', 'SELECT ARRAY[1, 2]',
      'SELECT ROW(1, 2)', 'SELECT CASE 1 WHEN 1 THEN 2 ELSE 3 END', 'SELECT lower(\'a\')', 'SELECT now()']
    unsupported.each do |sql|
      expression, registry = resolve(sql)
      assert(build(expression, registry).nil?, "unsupported expression compiled: #{sql}")
    end
    # An unsupported descendant must reject the entire expression; no partial
    # code may have executed when the caller chooses the interpreter fallback.
    expression, registry = resolve('SELECT length(\'unsafe\') + 1')
    assert(build(expression, registry).nil?, 'unsupported child did not reject the whole expression')
    literal = -> { JedNative::Expression.new(kind: :literal, type: 'i64', value: 1) }
    node = literal.call
    (JedNative::CompiledExpression::MAX_DEPTH + 2).times do
      node = JedNative::Expression.new(kind: :unary, type: 'i64', value: '+', args: [node])
    end
    assert(build(node).nil?, 'overdeep code was compiled')
    nodes = Array.new(JedNative::CompiledExpression::MAX_NODES + 1) { literal.call }
    node = JedNative::Expression.new(kind: :call, type: 'i64', value: 'coalesce', args: nodes, extra: {})
    assert(build(node).nil?, 'oversized expression was compiled')
    node = literal.call
    node.instance_variable_set(:@native_deep_evaluation, true)
    assert(build(node).nil?, 'deep-stack expression bypassed its interpreter lane')
    node = literal.call
    node.instance_variable_set(:@native_collation, JedNative::CollationLabel.new(name: 'C'))
    assert(build(node).nil?, 'collation metadata crossed the primitive compiler boundary')
    assert(build(literal.call, { 'i64' => [{ name: 'nominal', type: 'i32' }] }).nil?, 'nominal registry precedence was bypassed')
    %i[column param].each do |kind|
      node = JedNative::Expression.new(kind: kind, type: 'i64', value: -1)
      assert(build(node).nil?, 'negative ordinal was embedded in generated code')
    end
    host = JedNative::Expression.new(kind: :host_call, type: 'i64', value: Object.new, args: [])
    assert(build(host).nil?, 'host callback crossed the primitive compiler boundary')
  end

  def code_without_engine
    instance = engine([7], ['i64'])
    expression = instance.compile(JedNative::Statement.new('SELECT $1 + 1').ast[:columns].first[:expr], [])
    [build(expression, instance.instance_variable_get(:@types)), WeakRef.new(instance)]
  end

  def discarded_code
    Array.new(16) do |index|
      expression = JedNative::Expression.new(kind: :literal, type: 'i64', value: index)
      kernel = build(expression)
      assert(kernel, 'small literal did not compile during lifetime test')
      WeakRef.new(kernel)
    end
  end

  def retained_objects
    kernel, engine_reference = code_without_engine
    discarded = discarded_code
    3.times { GC.start(full_mark: true, immediate_sweep: true) }
    assert(!engine_reference.weakref_alive?, 'generated code retained its compilation engine')
    assert(discarded.none?(&:weakref_alive?), 'discarded generated code remained globally rooted')
    instance = engine([91], ['i64'])
    assert(kernel.call(instance, [], [2]) == 3, 'compiled code captured old binding values or ignored explicit parameters')
    assert(kernel.call(instance, [], [8]) == 9, 'compiled code reused a prior result')
  end

  def prepared_programs
    first, = resolve('SELECT $1::i64 + 1', types: ['i64'])
    second, = resolve('SELECT $1::i64 + 1', types: ['i64'])
    changed, = resolve('SELECT $1::i64 + 2', types: ['i64'])
    programs = [first, second, changed].map { |expression| JedNative::CompiledExpression.prepare_many([expression], types: {}) }
    assert(programs.all? && programs.all?(&:frozen?), 'prepared programs were missing or mutable')
    keys = programs.map { |program| [program.source, program.pool] }
    assert(keys[0] == keys[1] && keys[0] != keys[2], 'program identity ignored resolved literals')
    instance = engine([7], ['i64'])
    assert(programs[0].materialize.call(instance, [], [7]) == [8], 'materialized program changed output')
    unsupported, = resolve('SELECT upper($1)', types: ['text'])
    assert(JedNative::CompiledExpression.prepare_many([first, unsupported], types: {}).nil?,
      'strict prepared argument program retained an interpreter fallback')
    leaf = JedNative::Expression.new(kind: :param, type: 'i64', value: 0)
    assert(JedNative::CompiledExpression.prepare_many([leaf], types: {}).nil?, 'trivial argument vector created a program')
  end

  def run
    integer_kernels
    boolean_control_flow
    lazy_rows_and_parameters
    trusted_source
    fused_projections
    bounded_fallback
    retained_objects
    prepared_programs
    puts 'ruby-native compiled expressions: interpreter parity, cost prefixes, NULL control flow, delayed faults, trusted source, bounds and code lifetime passed'
  end
end

NativeCompiledExpressionTests.run
