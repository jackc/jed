# frozen_string_literal: true

require 'weakref'
require 'tmpdir'
require_relative '../lib/jed_native'

# The corpus owns aggregate SQL semantics. These probes cover generated code's
# input/reduction boundary, budget failure prefixes and host/cache lifetimes.
module NativeCompiledAggregateTests
  module_function

  COLUMNS = [{ name: 'id', type: 'i32' }, { name: 'v', type: 'i32' },
             { name: 'flag', type: 'boolean' }].freeze
  VALUES = [[1, 3, true], [2, nil, nil], [3, -7, false], [4, 0, true]].freeze

  def assert(value, message) = (raise(message) unless value)

  def engine(params = [], types = [])
    instance = JedNative::Engine.new
    instance.instance_variable_set(:@params, params)
    instance.instance_variable_set(:@native_binding_types, types)
    instance.instance_variable_set(:@native_aggregate_disabled, true)
    instance
  end

  def resolve(sql, types: [], columns: COLUMNS)
    instance = engine(Array.new(types.length), types)
    ast = JedNative::Statement.new(sql).ast
    expression = instance.compile(ast[:columns].first[:expr], columns)
    [expression, instance.instance_variable_get(:@types)]
  end

  class ProbeRow
    def initialize(values, ordinal, events, failure)
      @values, @ordinal, @events, @failure = values, ordinal, events, failure
    end

    def [](column)
      @events << [@ordinal, column]
      raise JedNative::Error.new('XX001', 'deferred aggregate input failure') if @failure == [@ordinal, column]
      @values[column]
    end
  end

  def direct_outcome(expression, kernel, values, params, types, failure, statement_limit, lifetime_limit, prior)
    instance = engine(params, types)
    instance.max_cost = statement_limit
    instance.lifetime_max_cost = lifetime_limit
    instance.instance_variable_set(:@lifetime_cost, prior)
    events = []
    rows = values.each_with_index.map { |row, index| ProbeRow.new(row, index, events, failure) }
    begin
      result = kernel ? kernel.call(instance, rows, params) : instance.compute_aggregate(expression, rows)
      outcome = [:value, result]
    rescue JedNative::Error => error
      outcome = [:error, error.sqlstate, error.message]
    end
    [*outcome, instance.cost, instance.lifetime_cost, events]
  end

  def direct_compare(sql, values: VALUES, params: [], types: [], columns: COLUMNS, failure: nil, expected: nil, expression: nil)
    expression, registry = expression ? [expression, {}] : resolve(sql, types: types, columns: columns)
    program = JedNative::CompiledAggregate.prepare(expression, types: registry)
    assert(program, "supported aggregate rejected: #{sql}")
    kernel = program.materialize
    reference = direct_outcome(expression, nil, values, params, types, failure, 0, 0, 0)
    actual = direct_outcome(expression, kernel, values, params, types, failure, 0, 0, 0)
    assert(actual == reference, "aggregate changed #{sql}: #{actual.inspect} != #{reference.inspect}")
    assert(reference[0, expected.length] == expected, "unexpected reference for #{sql}: #{reference.inspect}") if expected
    (1..reference[-3] + 1).each do |limit|
      [[limit, 0, 0], [0, 7 + limit, 7], [limit, 7 + limit, 7]].each do |statement_limit, lifetime_limit, prior|
        args = [values, params, types, failure, statement_limit, lifetime_limit, prior]
        expected_prefix = direct_outcome(expression, nil, *args)
        actual_prefix = direct_outcome(expression, kernel, *args)
        assert(actual_prefix == expected_prefix, "aggregate budget prefix changed #{sql} at #{[statement_limit, lifetime_limit, prior].inspect}: #{actual_prefix.inspect} != #{expected_prefix.inspect}")
      end
    end
    assert(program.frozen? && kernel.frozen? && kernel.pool.frozen?, 'aggregate code or pool is mutable')
    assert(kernel.source_bytes <= JedNative::CompiledExpression::MAX_SOURCE_BYTES &&
      kernel.node_count <= JedNative::CompiledExpression::MAX_NODES && kernel.pool.length <= JedNative::CompiledExpression::MAX_POOL,
      'aggregate kernel exceeded compiler bounds')
    kernel
  end

  def direct_kernels
    {
      'count(*)' => 4, 'count(v)' => 3, 'sum(v)' => -4,
      'min(v)' => -7, 'max(v)' => 3, 'min(flag)' => false,
      'max(flag)' => true, 'bool_and(flag)' => false,
      'bool_or(flag)' => true, 'every(flag)' => false,
      'bit_and(v)' => 0, 'bit_or(v)' => -5, 'bit_xor(v)' => -6
    }.each do |source, expected|
      direct_compare("SELECT #{source}", expected: [:value, expected])
      direct_compare("SELECT #{source}", values: [], expected: [:value, source.start_with?('count') ? 0 : nil])
      direct_compare("SELECT #{source}", values: [[nil, nil, nil]], expected: [:value, source == 'count(*)' ? 1 : source.start_with?('count') ? 0 : nil])
    end
    %w[i16 i32 i64].each do |type|
      columns = [{ name: 'v', type: type }]
      bits = JedNative::Types::INTEGER_BITS.fetch(type)
      values = [[-(1 << (bits - 1))], [nil], [0], [(1 << (bits - 1)) - 1]]
      %w[count min max bit_and bit_or bit_xor].each { |name| direct_compare("SELECT #{name}(v)", columns: columns, values: values) }
      direct_compare('SELECT sum(v)', columns: columns, values: values) unless type == 'i64'
    end
    [true, false, nil].repeated_permutation(3) do |values|
      %w[min max bool_and bool_or every count].each do |name|
        direct_compare("SELECT #{name}(flag)", values: values.map { |value| [0, 0, value] })
      end
    end
    [[], [nil], [-8], [9]].each do |items|
      direct_compare('SELECT sum(v + $1) FILTER (WHERE flag OR v < 0)', values: items.empty? ? [] : VALUES,
        params: items.empty? ? [2] : items, types: ['i32'])
    end
    direct_compare('SELECT count(*) FILTER (WHERE flag AND 10 / v > 0)')
    direct_compare('SELECT sum(10 / v)', expected: [:error, '22012'])
    direct_compare('SELECT sum(v) FILTER (WHERE flag)', failure: [2, 1])
    direct_compare('SELECT sum(v + id)', failure: [1, 1], expected: [:error, 'XX001'])
    direct_compare('SELECT bool_or(flag)', failure: [3, 2], expected: [:error, 'XX001'])
    direct_compare('SELECT bool_and(flag)', failure: [3, 2], expected: [:error, 'XX001'])
    expression, = resolve('SELECT sum(v)')
    expression.args = [JedNative::Expression.new(kind: :param, type: 'i16', value: 0, extra: { context_type: true })]
    [nil, -32_768, 32_767, 32_768].each do |value|
      direct_compare('contextual aggregate parameter', expression: expression, values: [[0], [0]], params: [value], types: ['i64'])
    end
  end

  def evaluation_precedes_reduction
    # Synthetic typed rows can reach an i64 SUM overflow without billions of
    # real i32 inputs. The later argument fault must win before reduction begins.
    column = JedNative::Expression.new(kind: :column, type: 'i32', value: 0)
    expression, = resolve('SELECT sum(v)')
    expression.args = [column]
    huge = (1 << 63) - 1
    direct_compare('synthetic sum overflow', expression: expression, values: [[huge], [1]], expected: [:error, '22003'])
    direct_compare('input fault before synthetic sum overflow', expression: expression,
      values: [[huge], [1], [0]], failure: [2, 0], expected: [:error, 'XX001'])
    # Column reads have no implicit cast. The expression result is statically
    # i32, while its direct argument values can exercise the reducer's boundary.
    resolved, = resolve('SELECT sum(greatest(v, 10 / id))')
    direct_compare('later division before sum overflow', expression: resolved,
      values: [[10, huge, true], [10, 1, true], [0, 0, false]], expected: [:error, '22012'])
  end

  def admission_and_source
    compiler = JedNative::CompiledAggregate
    [
      'SELECT sum(v::i64)', 'SELECT avg(v)', 'SELECT sum(v::decimal)', 'SELECT sum(v::f64)',
      'SELECT array_agg(v)', 'SELECT count(DISTINCT v)', 'SELECT sum(v ORDER BY id)',
      'SELECT sum(v) OVER (ORDER BY id)',
      'SELECT percentile_disc(0.5) WITHIN GROUP (ORDER BY v)',
      'SELECT min(v::text)', 'SELECT count(upper(v::text))',
      'SELECT sum(v) FILTER (WHERE upper(v::text) = \'3\')'
    ].each do |sql|
      expression, registry = resolve(sql)
      assert(compiler.prepare(expression, types: registry).nil?, "unsupported aggregate compiled: #{sql}")
    end
    expression, = resolve('SELECT sum(v)')
    assert(compiler.prepare(expression, types: { 'i32' => [] }).nil?, 'named argument entered primitive aggregate')
    assert(compiler.prepare(expression, types: { 'i64' => [] }).nil?, 'named output entered primitive aggregate')
    expression.instance_variable_set(:@native_collation, Object.new)
    assert(compiler.prepare(expression, types: {}).nil?, 'collated aggregate entered primitive kernel')
    expression, = resolve('SELECT min(v)')
    expression.args.first.instance_variable_set(:@native_collation, Object.new)
    assert(compiler.prepare(expression, types: {}).nil?, 'collated input entered primitive aggregate')
    marker = 'v\"; raise \'aggregate source injection\'; #'
    quoted = '"' + marker.gsub('"', '""') + '"'
    expression, registry = resolve("SELECT sum(#{quoted})", columns: [{ name: marker, type: 'i32' }])
    program = compiler.prepare(expression, types: registry)
    assert(program && !program.source.include?(marker), 'SQL identifier reached aggregate source')
    assert(program.pool.none? { |value| value.is_a?(JedNative::Expression) || value.is_a?(JedNative::Engine) }, 'aggregate pool retained resolution or execution state')
    malicious = Object.new
    def malicious.to_s = raise('aggregate compiler stringified an unsupported value')
    def malicious.inspect = raise('aggregate compiler inspected an unsupported value')
    expression.args = [JedNative::Expression.new(kind: :literal, type: 'i32', value: malicious)]
    assert(compiler.prepare(expression, types: {}).nil?, 'unsupported aggregate literal entered source')
    expression, = resolve('SELECT sum(v)')
    nested = expression.args.first
    40.times { nested = JedNative::Expression.new(kind: :unary, type: 'i32', value: '+', args: [nested]) }
    expression.args = [nested]
    assert(compiler.prepare(expression, types: {}).nil?, 'aggregate expression depth cap was bypassed')

    # A balanced tree reaches the node cap without reaching the depth cap.
    # COUNT(*) must reserve both its aggregate node and its implicit star node.
    balanced = lambda do |leaves|
      if leaves == 1
        JedNative::Expression.new(kind: :column, type: 'boolean', value: 0)
      else
        left = leaves / 2
        node = JedNative::Expression.new(kind: :binary, type: 'boolean', value: 'and',
          args: [balanced.call(left), balanced.call(leaves - left)])
        node.instance_variable_set(:@native_filter_conjunction, true)
        node
      end
    end
    expression, = resolve('SELECT count(*)')
    # 95 leaves and 94 binary nodes plus NOT = 190 FILTER nodes.
    expression.extra = expression.extra.merge(filter_expr: JedNative::Expression.new(kind: :unary,
      type: 'boolean', value: 'not', args: [balanced.call(95)]))
    adjacent = compiler.prepare(expression, types: {})
    assert(adjacent && adjacent.node_count == JedNative::CompiledExpression::MAX_NODES,
      'valid aggregate exactly at its node cap was rejected')
    # 96 leaves and 95 binary nodes = 191 FILTER nodes, or 193 including COUNT(*).
    expression.extra = expression.extra.merge(filter_expr: balanced.call(96))
    assert(compiler.prepare(expression, types: {}).nil?, 'COUNT star exceeded aggregate node cap after FILTER')
  end

  def setup(database)
    database.execute('CREATE TABLE aggregate_t(id i32 PRIMARY KEY, v i32, flag boolean)')
    database.execute('INSERT INTO aggregate_t VALUES(1,3,TRUE),(2,NULL,NULL),(3,-7,FALSE),(4,0,TRUE),(5,3,FALSE),(6,-7,TRUE)')
  end

  def session(database, disabled: false)
    current = database.session
    current.engine.instance_variable_set(:@native_aggregate_disabled, disabled)
    current
  end

  def invoke(current, statement, sql, params, api)
    case api
    when :query then current.query(sql, *params)
    when :execute then current.execute(sql, *params)
    when :query_prepared then current.query_prepared(statement, *params)
    when :execute_prepared then current.execute_prepared(statement, *params)
    end
  end

  def outcome(current, statement, sql, params, api, collect: false, take: nil)
    prior = current.lifetime_cost
    phase, rows, checkpoints = :admission, [], []
    result = invoke(current, statement, sql, params, api)
    phase = :pull
    if result.is_a?(JedNative::Rows) && !collect
      result.each do |row|
        rows << row.to_a
        checkpoints << [result.cost, current.lifetime_cost - prior]
        break if take && rows.length >= take
      end
    else
      rows = result.rows
    end
    [:rows, rows, result.columns, result.column_types, checkpoints, result.cost, current.lifetime_cost - prior]
  rescue JedNative::Error => error
    [:error, phase, error.sqlstate, error.message, rows, checkpoints, result&.cost || current.engine.cost, current.lifetime_cost - prior]
  ensure
    result.close if result.is_a?(JedNative::Rows)
  end

  def compare(database, sql, params = [], api: :query_prepared, setting: nil, limit: 0, collect: false, calls: nil)
    results = [true, false].map do |disabled|
      current = session(database, disabled: disabled)
      statement = current.prepare(sql)
      3.times { outcome(current, statement, sql, params, api) }
      if setting == :lifetime_max_cost
        current.lifetime_max_cost = current.lifetime_cost + limit
      elsif setting
        current.public_send("#{setting}=", limit)
      end
      calls&.clear
      value = outcome(current, statement, sql, params, api, collect: collect)
      value << calls.dup if calls
      value
    ensure
      current&.close
    end
    assert(results[0] == results[1], "compiled aggregate changed #{api} #{sql} #{params.inspect} #{setting}=#{limit}: #{results.inspect}")
    results[1]
  end

  def dispatched
    calls = []
    trace = TracePoint.new(:call) do |event|
      calls << event.self if event.method_id == :call && event.self.is_a?(JedNative::CompiledAggregate::Kernel)
    end
    trace.enable { yield }
    calls
  ensure
    trace&.disable
  end

  def api_equivalence(database)
    cases = [
      ['SELECT count(*), count(v), sum(v), min(v), max(v), bool_and(flag), bool_or(flag), every(flag), bit_and(v), bit_or(v), bit_xor(v) FROM aggregate_t', []],
      ['SELECT flag, count(*), sum(v + $1) FILTER (WHERE v < $2 OR flag), min(v) FROM aggregate_t GROUP BY flag ORDER BY flag', [4, 0]],
      ['SELECT flag, count(*), sum(v) FROM aggregate_t GROUP BY GROUPING SETS ((flag), ()) ORDER BY flag, count(*)', []],
      ['SELECT flag, sum(v) FROM aggregate_t GROUP BY flag HAVING sum(v) > $1 ORDER BY flag', [-20]],
      ['SELECT sum(10 / v) FROM aggregate_t', []],
      ['SELECT sum(v + $1) FROM aggregate_t', [2_147_483_647]],
      ['SELECT count(*), sum(v), min(v) FROM aggregate_t WHERE id > 100', []],
      ['SELECT count(*), count(v), sum(v) FROM aggregate_t WHERE id = 2', []],
      ['SELECT sum(DISTINCT v), sum(v ORDER BY id DESC), avg(v) FROM aggregate_t', []],
      ['SELECT id, sum(v) OVER (ORDER BY id) FROM aggregate_t ORDER BY id', []]
    ]
    %i[execute query execute_prepared query_prepared].each do |api|
      cases.each { |sql, params| compare(database, sql, params, api: api) }
    end
    current = session(database)
    sql = 'SELECT id, sum(v) FILTER (WHERE flag) OVER (ORDER BY id) FROM aggregate_t ORDER BY id'
    statement = current.prepare(sql)
    assert(dispatched { 3.times { outcome(current, statement, sql, [], :query_prepared) } }.empty?, 'window synthetic aggregate entered compilation')
    current.close
    %i[execute_prepared query_prepared].each do |api|
      [cases[0], cases[1], cases[4]].each do |sql, params|
        complete = compare(database, sql, params, api: api)
        %i[max_cost lifetime_max_cost].each do |setting|
          (1..complete[-2] + 1).each { |limit| compare(database, sql, params, api: api, setting: setting, limit: limit) }
        end
        [1, 127, 256, 512, 2048, 1_048_576].each do |limit|
          compare(database, sql, params, api: api, setting: :max_query_memory_bytes, limit: limit, collect: true)
        end
      end
    end
  end

  def plan_reuse(database)
    %i[execute query execute_prepared query_prepared].each do |api|
      current = session(database)
      sql = 'SELECT sum(v + $1) FILTER (WHERE id > $2) FROM aggregate_t'
      statement = current.prepare(sql)
      assert(dispatched { outcome(current, statement, sql, [2, 1], api) }.empty?, 'first-use aggregate compiled eagerly')
      calls = dispatched { outcome(current, statement, sql, [2, 1], api) }
      assert(!calls.empty?, "#{api} did not dispatch an aggregate kernel after reuse")
      kernel = calls.first
      [[5, 0], [nil, 0], [-2, 2], [4, 100]].each do |params|
        expected = compare(database, sql, params, api: api)
        actual = outcome(current, statement, sql, params, api)
        assert(actual == expected, 'aggregate kernel retained earlier parameters')
      end
      assert(dispatched { outcome(current, statement, sql, [2, 1], api) }.include?(kernel), 'unchanged aggregate rebuilt its kernel')
      current.engine.instance_variable_set(:@native_aggregate_disabled, true)
      assert(dispatched { outcome(current, statement, sql, [2, 1], api) }.empty?, 'disabled cached aggregate still dispatched')
      current.engine.instance_variable_set(:@native_aggregate_disabled, false)
      assert(dispatched { outcome(current, statement, sql, [2, 1], api) }.include?(kernel), 'reenabled aggregate failed to reuse code')
      [[], [1], [1, 2, 3], ['wrong', 1]].each do |params|
        assert(outcome(current, statement, sql, params, api).first == :error, 'warm aggregate admitted invalid parameters')
      end
      current.close
    end
  end

  def callbacks_and_fallback
    calls = []
    environment = JedNative::Environment.new.register_function(name: 'aggregate_probe', args: ['i32'], returns: 'i32', cost: 3) do |value|
      calls << value
      raise JedNative::Error.new('22023', 'aggregate callback failure') if value == -7
      value
    end
    JedNative.memory(environment: environment) do |database|
      setup(database)
      ['SELECT sum(aggregate_probe(v)) FROM aggregate_t',
       'SELECT sum(v) FILTER (WHERE aggregate_probe(id) > 0) FROM aggregate_t',
       'SELECT aggregate_probe(count(*)::i32) FROM aggregate_t'].each do |sql|
        %i[query_prepared execute_prepared].each { |api| compare(database, sql, api: api, calls: calls) }
      end
    end
  end

  def cache_bounds_and_pool_identity
    JedNative.memory do |database|
      setup(database)
      current = session(database)
      first_kernel = nil
      80.times do |index|
        sql = "SELECT sum(v + #{index}) FROM aggregate_t"
        statement = current.prepare(sql)
        2.times { current.execute_prepared(statement) }
        observed = dispatched { current.execute_prepared(statement) }
        assert(!observed.empty?, 'aggregate cache did not admit a repeated program')
        first_kernel ||= observed.first
        assert(observed.first.pool != first_kernel.pool || index.zero?, 'different literal pools aliased in aggregate cache')
      end
      cache = current.engine.instance_variable_get(:@native_compiled_aggregates)
      assert(cache && cache.length <= JedNative::CompiledPlans::MAX_AGGREGATE_CACHE_ENTRIES, 'aggregate cache exceeded its bound')
      assert(!cache.values.include?(first_kernel), 'aggregate cache did not evict its oldest program')
      assert(cache.values.compact.all? { |kernel| kernel.source_bytes <= JedNative::CompiledExpression::MAX_SOURCE_BYTES },
        'aggregate cache retained oversized code')
      sql = 'SELECT sum(v + 79) FROM aggregate_t'
      statement = current.prepare(sql)
      latest = dispatched { current.execute_prepared(statement) }.first
      assert(latest && cache.values.include?(latest), 'aggregate LRU failed to reuse its newest program')
      changed = 'SELECT sum(v * 79) FROM aggregate_t'
      changed_statement = current.prepare(changed)
      2.times { current.execute_prepared(changed_statement) }
      different = dispatched { current.execute_prepared(changed_statement) }.first
      assert(different && different.source != latest.source, 'aggregate cache ignored emitted source changes')
      current.close
    end
  end

  def detached_kernel
    instance = engine
    expression, registry = resolve('SELECT sum(v)')
    kernel = JedNative::CompiledAggregate.build(expression, types: registry)
    rows, params = VALUES.map(&:dup), []
    kernel.call(instance, rows, params)
    [kernel, WeakRef.new(instance), WeakRef.new(rows), WeakRef.new(params)]
  end

  def kernel_lifetime
    kernel, *references = detached_kernel
    3.times { GC.start }
    assert(references.none?(&:weakref_alive?), 'generated aggregate retained an engine, group or bindings')
    assert(kernel.pool.none? { |value| value.is_a?(JedNative::Expression) || value.is_a?(Array) || value.is_a?(Hash) },
      'generated aggregate retained mutable resolution data')
    assert(kernel.call(engine, VALUES, []) == -4, 'detached aggregate code did not remain usable')
  end

  def file_fallback
    Dir.mktmpdir('jed-compiled-aggregate-') do |directory|
      JedNative.create(File.join(directory, 'aggregate.jed'), locking: :exclusive, work_mem: 128) do |database|
        setup(database)
        sql = 'SELECT flag, count(*), sum(v + id) FROM aggregate_t GROUP BY flag ORDER BY flag'
        assert(dispatched { compare(database, sql) }.empty?, 'file spill fold entered the general-memory aggregate compiler')
        compare(database, sql, api: :execute_prepared, setting: :max_query_memory_bytes, limit: 512, collect: true)
      end
    end
  end

  def snapshots_and_lifetime(database)
    reader, writer = session(database), database.session
    sql = 'SELECT flag, sum(v + $1) FROM aggregate_t GROUP BY flag ORDER BY flag'
    statement = reader.prepare(sql)
    3.times { reader.query_prepared(statement, 1).rows }
    before = reader.query_prepared(statement, 1).rows
    before_ten = reader.query_prepared(statement, 10).rows
    old, pending = reader.query_prepared(statement, 1), reader.query_prepared(statement, 10)
    first = old.next.to_a
    writer.execute('UPDATE aggregate_t SET v=100 WHERE id=3')
    fresh = reader.query_prepared(statement, 1).rows
    assert(fresh != before && [first, *old.map(&:to_a)] == before, 'aggregate cursor lost its captured publication')
    assert(pending.rows == before_ten, 'pending aggregate cursor used a later publication or bindings')
    admitted = reader.query_prepared(statement, 1)
    reader.revoke(:select, 'aggregate_t')
    denied = outcome(reader, statement, sql, [1], :query_prepared)
    assert(denied[0] == :error && denied[2] == '42501', 'aggregate bypassed refreshed permissions')
    assert(admitted.rows == fresh, 'admitted aggregate cursor lost captured permissions')
    reader.reset_privileges
    complete = reader.query_prepared(statement, 1)
    complete.rows
    reader.lifetime_max_cost = reader.lifetime_cost + complete.cost + 1
    first, second = reader.query_prepared(statement, 1), reader.query_prepared(statement, 2)
    first.rows
    begin
      second.next
      raise 'aggregate cursor escaped shared lifetime budget'
    rescue JedNative::Error => error
      assert(error.sqlstate == '54P02', 'aggregate shared lifetime failed with wrong error')
    ensure
      first.close
      second.close
    end
    reader.lifetime_max_cost = 0
    captured = reader.query_prepared(statement, 1)
    writer.execute('DROP TABLE aggregate_t')
    writer.execute('CREATE TABLE aggregate_t(id i16 PRIMARY KEY, v i16, flag boolean)')
    writer.execute('INSERT INTO aggregate_t VALUES(1,7,TRUE)')
    assert(reader.query_prepared(statement, 1).rows == [[true, 8]], 'aggregate reused stale schema')
    assert(captured.rows == fresh, 'aggregate captured cursor lost old schema')
    reader.close
    writer.close
  end

  def run
    direct_kernels
    evaluation_precedes_reduction
    admission_and_source
    JedNative.memory do |database|
      setup(database)
      plan_reuse(database)
      api_equivalence(database)
      snapshots_and_lifetime(database)
    end
    callbacks_and_fallback
    cache_bounds_and_pool_identity
    kernel_lifetime
    file_fallback
    puts 'ruby-native compiled aggregate: admission, two-phase reduction, guard prefixes, warmed APIs, callbacks and cursor lifetimes passed'
  end
end

NativeCompiledAggregateTests.run
