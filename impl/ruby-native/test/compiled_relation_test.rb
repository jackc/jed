# frozen_string_literal: true

require 'tmpdir'
require 'weakref'
require_relative '../lib/jed_native'

# SQL behavior belongs to the shared corpus. These probes observe the compiler
# seam: input/key order, ownership release, bounded code and captured host state.
module NativeCompiledRelationTests
  module_function

  COLUMNS = [{ name: 'id', type: 'i32' }, { name: 'v', type: 'i32' },
             { name: 'flag', type: 'boolean' }].freeze
  VALUES = [[1, 3, true], [2, nil, nil], [3, -7, false], [4, 0, true]].freeze

  class << self
    attr_accessor :source_failure, :nonarray_source, :nonarray_groups
  end

  class EnumerableRows
    include Enumerable
    def initialize(rows) = (@rows = rows)
    def each(&block) = block ? @rows.each(&block) : enum_for(:each)
    def length = @rows.length
  end

  module SourceFault
    def source_relation(source)
      result = super
      if NativeCompiledRelationTests.source_failure && source[:name] == 'relation_t'
        raise JedNative::Error.new('XX001', 'source materialization failed')
      end
      result.rows = EnumerableRows.new(result.rows) if NativeCompiledRelationTests.nonarray_source && source[:name] == 'relation_t'
      result
    end
    def build_aggregate_contexts(statement, relation, group_nodes)
      if NativeCompiledRelationTests.nonarray_groups
        relation = JedNative::Relation.new(columns: relation.columns, rows: EnumerableRows.new(relation.rows))
      end
      super(statement, relation, group_nodes)
    end
  end
  JedNative::Engine.prepend(SourceFault)

  def assert(value, message) = (raise(message) unless value)

  def engine(params = [], types = [])
    current = JedNative::Engine.new
    current.instance_variable_set(:@params, params)
    current.instance_variable_set(:@native_binding_types, types)
    current.instance_variable_set(:@native_relation_disabled, true)
    current
  end

  def resolve(sql, mode, types = [], columns = COLUMNS)
    current = engine(Array.new(types.length), types)
    ast = JedNative::Statement.new(sql).ast
    expressions = if mode == :filter
      current.compile_filter(ast[:where], columns)
    else
      ast[:columns].map { |column| current.compile(column[:expr], columns) }
    end
    [expressions, current.instance_variable_get(:@types)]
  end

  class ProbeRow
    attr_reader :values, :ordinal
    def initialize(values, ordinal, events, failure)
      @values, @ordinal, @events, @failure = values, ordinal, events, failure
    end
    def length = @values.length
    def [](index)
      @events << [:column, @ordinal, index]
      raise JedNative::Error.new('XX001', 'deferred relation column failed') if @failure == [@ordinal, index]
      @values[index]
    end
    def each_with_index
      index = 0
      while index < length
        yield self[index], index
        index += 1
      end
    end
  end

  class ProbeRows < Array
    def initialize(values, events, column_failure, input_failure)
      @events, @input_failure = events, input_failure
      super(values.each_with_index.map { |row, index| ProbeRow.new(row, index, events, column_failure) })
    end
    def [](index)
      @events << [:input, index]
      raise JedNative::Error.new('XX001', 'deferred relation input failed') if index == @input_failure
      super
    end
    def each
      return enum_for(:each) unless block_given?
      index = 0
      while index < length
        yield self[index]
        index += 1
      end
    end
  end

  module AccountProbe
    attr_accessor :relation_events
    def charge(amount = 1)
      relation_events << [:charge, amount, cost]
      super
    end
    def query_memory_row_bytes(row, columns = nil, mask = nil)
      relation_events << [:row_bytes, row.ordinal, columns, mask]
      super
    end
    def query_memory_release(owner, bytes = nil)
      relation_events << [:release, owner.nil?, bytes]
      super
    end
  end

  def direct_outcome(expressions, kernel, mode, values, params, types, memory, owner_present, column_failure, input_failure, max_cost, lifetime_max_cost, prior)
    current = engine(params, types)
    current.singleton_class.prepend(AccountProbe)
    events = current.relation_events = []
    current.max_cost, current.lifetime_max_cost = max_cost, lifetime_max_cost
    current.instance_variable_set(:@lifetime_cost, prior)
    account = JedNative::QueryMemoryAccount.new(8192)
    account.reserve(4096)
    current.instance_variable_set(:@query_memory, account) if memory
    owner = JedNative::QueryMemoryAccount::Owner.new(4096) if owner_present
    rows = ProbeRows.new(values, events, column_failure, input_failure)
    mask = [true, true, true]
    begin
      if kernel
        result = kernel.call(current, rows, params, owner, COLUMNS, mask)
      elsif mode == :filter
        result = []
        rows.each do |row|
          accepted = current.evaluate(expressions, row) == true
          current.query_memory_release(owner, current.query_memory_row_bytes(row, COLUMNS, mask)) if memory && !accepted
          result << row if accepted
        end
      else
        result = rows.each.map { |row| expressions.map { |expression| current.evaluate(expression, row) } }
      end
      result = result.map(&:values) if mode == :filter
      outcome = [:rows, result]
    rescue JedNative::Error => error
      outcome = [:error, error.sqlstate, error.message]
    end
    [*outcome, current.cost, current.lifetime_cost, events, owner&.bytes, account.used]
  end

  def direct_compare(sql, mode: :filter, values: VALUES, params: [], types: [], columns: COLUMNS,
                     memory: false, owner_present: memory, column_failure: nil, input_failure: nil, expected: nil)
    expressions, registry = resolve(sql, mode, types, columns)
    program = mode == :filter ? JedNative::CompiledRelation.prepare_filter(expressions, types: registry, memory: memory) :
      JedNative::CompiledRelation.prepare_keys(expressions, types: registry)
    assert(program, "supported relation expressions rejected: #{sql}")
    kernel = program.materialize
    arguments = [mode, values, params, types, memory, owner_present, column_failure, input_failure]
    reference = direct_outcome(expressions, nil, *arguments, 0, 0, 0)
    actual = direct_outcome(expressions, kernel, *arguments, 0, 0, 0)
    assert(actual == reference, "relation loop changed #{sql}: #{actual.inspect} != #{reference.inspect}")
    assert(reference[0, expected.length] == expected, "unexpected reference for #{sql}: #{reference.inspect}") if expected
    (1..reference[-5] + 1).each do |limit|
      [[limit, 0, 0], [0, 7 + limit, 7], [limit, 7 + limit, 7]].each do |statement, lifetime, prior|
        expected_prefix = direct_outcome(expressions, nil, *arguments, statement, lifetime, prior)
        actual_prefix = direct_outcome(expressions, kernel, *arguments, statement, lifetime, prior)
        assert(actual_prefix == expected_prefix, "relation guard changed #{sql} at #{[statement, lifetime, prior].inspect}: #{actual_prefix.inspect} != #{expected_prefix.inspect}")
      end
    end
    assert(program.frozen? && kernel.frozen? && kernel.pool.frozen?, 'relation program or pool is mutable')
    assert(kernel.node_count <= JedNative::CompiledExpression::MAX_NODES &&
      kernel.source_bytes <= JedNative::CompiledExpression::MAX_SOURCE_BYTES && kernel.pool.length <= JedNative::CompiledExpression::MAX_POOL,
      'relation program escaped compiler bounds')
    kernel
  end

  def direct_loops
    [false, true].each do |memory|
      ['flag', 'v >= $1', 'flag AND v < $1', '(flag AND v < $1) OR id > 3',
       'CASE WHEN flag THEN v < $1 ELSE id > 2 END', 'NULL::boolean', 'TRUE'].each do |predicate|
        params, types = predicate.include?('$1') ? [[2], ['i32']] : [[], []]
        direct_compare("SELECT 1 WHERE #{predicate}", params: params, types: types, memory: memory)
      end
      direct_compare('SELECT 1 WHERE v + 1 > $1', params: [nil], types: ['i32'], memory: memory)
      direct_compare('SELECT 1 WHERE 10 / v > 0', memory: memory, expected: [:error, '22012'])
      direct_compare('SELECT 1 WHERE flag', memory: memory, column_failure: [2, 2], expected: [:error, 'XX001'])
      direct_compare('SELECT 1 WHERE v + 1 > 0', memory: memory, input_failure: 1, expected: [:error, 'XX001'])
      direct_compare('SELECT 1 WHERE flag', values: [], memory: memory, expected: [:rows, []])
    end
    # Rejection measures the row before release even with no attached owner.
    direct_compare('SELECT 1 WHERE FALSE', memory: true, owner_present: false, column_failure: [0, 1], expected: [:error, 'XX001'])
    direct_compare('SELECT 1 WHERE flag', memory: true, owner_present: false)
    direct_compare('SELECT 1 WHERE NULL::boolean AND 10 / v > 0', values: [[0, 0, nil]])
    direct_compare('SELECT 1 WHERE (NULL::boolean AND 10 / v > 0) OR FALSE', values: [[0, 0, nil]], expected: [:error, '22012'])
    direct_compare('SELECT id, v, flag', mode: :keys)
    direct_compare('SELECT id % 2, coalesce(v, 99), flag', mode: :keys)
    direct_compare('SELECT id + $1, CASE WHEN flag THEN v ELSE -v END', mode: :keys, params: [5], types: ['i32'])
    direct_compare('SELECT id + $1, v', mode: :keys, params: [nil], types: ['i32'])
    direct_compare('SELECT id, 10 / v', mode: :keys, expected: [:error, '22012'])
    direct_compare('SELECT id, v', mode: :keys, column_failure: [1, 1], expected: [:error, 'XX001'])
    direct_compare('SELECT id, v', mode: :keys, input_failure: 1, expected: [:error, 'XX001'])
    direct_compare('SELECT 10 / (id - 2), 2147483647 + id', mode: :keys, expected: [:error, '22003'])
    direct_compare('SELECT id + 1', mode: :keys, values: [], expected: [:rows, []])
  end

  def admission_and_source
    compiler = JedNative::CompiledRelation
    assert(compiler.prepare_filter(nil, types: {}).nil? && compiler.prepare_keys([], types: {}).nil?, 'empty relation program compiled')
    [['SELECT 1 WHERE v::decimal > 0', :filter], ['SELECT upper(v::text)', :keys],
     ['SELECT row_number() OVER (ORDER BY id)', :keys], ['SELECT v::decimal', :keys]].each do |sql, mode|
      expressions, registry = resolve(sql, mode)
      program = mode == :filter ? compiler.prepare_filter(expressions, types: registry) : compiler.prepare_keys(expressions, types: registry)
      assert(program.nil?, "unsupported relation program compiled: #{sql}")
    end
    predicate, = resolve('SELECT 1 WHERE v > 0', :filter)
    assert(compiler.prepare_filter(predicate, types: { 'i32' => [] }).nil?, 'nominal WHERE type compiled as primitive')
    predicate.instance_variable_set(:@native_collation, Object.new)
    assert(compiler.prepare_filter(predicate, types: {}).nil?, 'collated WHERE entered compiler')
    keys, = resolve('SELECT id, v', :keys)
    keys.last.instance_variable_set(:@native_collation, Object.new)
    assert(compiler.prepare_keys(keys, types: {}).nil?, 'key vector partially compiled unsupported root')
    outer = JedNative::Expression.new(kind: :outer, type: 'i32', value: [0, 0])
    assert(compiler.prepare_keys([outer], types: {}).nil?, 'correlated outer expression entered compiler')
    marker = 'v"; raise \'relation source injection\'; #'
    quoted = '"' + marker.gsub('"', '""') + '"'
    keys, registry = resolve("SELECT #{quoted} + 1", :keys, [], [{ name: marker, type: 'i32' }])
    program = compiler.prepare_keys(keys, types: registry)
    assert(program && !program.source.include?(marker), 'SQL identifier became generated relation source')
    assert(program.pool.none? { |value| value.is_a?(JedNative::Expression) }, 'relation pool retained evaluator fallback')
    maximum = JedNative::CompiledExpression::MAX_NODES
    column = JedNative::Expression.new(kind: :column, type: 'i32', value: 0)
    at_cap = compiler.prepare_keys(Array.new(maximum, column), types: {})
    assert(at_cap && at_cap.node_count == maximum, 'valid key vector exactly at node cap rejected')
    assert(compiler.prepare_keys(Array.new(maximum + 1, column), types: {}).nil?, 'relation vector exceeded node cap')
    deep = column
    40.times { deep = JedNative::Expression.new(kind: :unary, type: 'i32', value: '+', args: [deep]) }
    assert(compiler.prepare_keys([deep], types: {}).nil?, 'relation expression exceeded depth cap')

    predicate, = resolve('SELECT 1 WHERE flag', :filter)
    retained = compiler.prepare_filter(predicate, types: {}).materialize.call(engine, VALUES, [])
    assert(retained.length == 2 && retained[0].equal?(VALUES[0]) && retained[1].equal?(VALUES[3]), 'compiled filter copied original row objects')

    current = engine
    current.instance_variable_set(:@native_relation_disabled, false)
    modes = [false, true].map do |memory|
      first, = resolve('SELECT 1 WHERE v > 0', :filter)
      assert(current.native_compiled_relation_filter(first, memory: memory).nil?, 'relation cache skipped first-use admission')
      repeated, = resolve('SELECT 1 WHERE v > 0', :filter)
      kernel = current.native_compiled_relation_filter(repeated, memory: memory)
      assert(kernel, 'relation cache lost repeated program')
      current.instance_variable_set(:@native_compilation_disabled, true)
      assert(current.native_compiled_relation_filter(repeated, memory: memory).nil?, 'global disable dispatched memoized relation program')
      current.instance_variable_set(:@native_compilation_disabled, false)
      assert(current.native_compiled_relation_filter(repeated, memory: memory).equal?(kernel), 'global reenable did not recover memoized program')
      kernel
    end
    assert(modes[0].source != modes[1].source && !modes[0].equal?(modes[1]), 'memory and ordinary filter cached as the same program')
  end

  def setup(database)
    database.execute('CREATE TABLE relation_t(id i32 PRIMARY KEY, v i32, flag boolean)')
    database.execute('INSERT INTO relation_t VALUES(1,3,TRUE),(2,NULL,NULL),(3,-7,FALSE),(4,0,TRUE),(5,3,FALSE),(6,-7,TRUE)')
    database.execute('CREATE TABLE relation_limits(v i32)')
    database.execute('INSERT INTO relation_limits VALUES(2)')
  end

  def session(database, disabled: false)
    current = database.session
    current.engine.instance_variable_set(:@native_relation_disabled, disabled)
    current
  end

  def invoke(current, statement, sql, params, api)
    case api
    when :execute then current.execute(sql, *params)
    when :query then current.query(sql, *params)
    when :execute_prepared then current.execute_prepared(statement, *params)
    when :query_prepared then current.query_prepared(statement, *params)
    end
  end

  def outcome(current, statement, sql, params, api, collect: false)
    prior = current.lifetime_cost
    phase, rows, checkpoints = :admission, [], []
    result = invoke(current, statement, sql, params, api)
    phase = :pull
    if result.is_a?(JedNative::Rows) && !collect
      result.each do |row|
        rows << row.to_a
        checkpoints << [result.cost, current.lifetime_cost - prior]
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
    assert(results[0] == results[1], "relation compilation changed #{api} #{sql} #{params.inspect} #{setting}=#{limit}: #{results.inspect}")
    results[1]
  end

  def dispatched
    calls = []
    trace = TracePoint.new(:call) do |event|
      calls << event.self if event.method_id == :call && event.self.is_a?(JedNative::CompiledRelation::Kernel)
    end
    trace.enable { yield }
    calls
  ensure
    trace&.disable
  end

  def api_equivalence(database)
    cases = [
      ['SELECT sum(v) FROM relation_t WHERE flag AND v < $1', [4]],
      ['SELECT id % $1, count(*), sum(v) FROM relation_t WHERE v > $2 GROUP BY id % $1 ORDER BY 1', [3, -8]],
      ['SELECT coalesce(v,99), count(*) FROM relation_t GROUP BY ROLLUP(coalesce(v,99)) HAVING coalesce(v,99) IS NULL OR count(*) > 0 ORDER BY 1', []],
      ['SELECT id % 2, flag, count(*) FROM relation_t GROUP BY GROUPING SETS ((id % 2), (flag), ()) ORDER BY 1,2,3', []],
      ['SELECT id, v, count(*) FROM relation_t GROUP BY id ORDER BY id', []],
      ['SELECT sum(v) FROM relation_t WHERE id > (SELECT v FROM relation_limits)', []],
      ['SELECT sum(v) FROM relation_t WHERE id > 100', []],
      ['SELECT sum(v) FROM relation_t WHERE id > 3 AND id < 2', []],
      ['SELECT 2147483647 + id, count(*) FROM relation_t WHERE 10 / (id - 3) > 0 OR id < 3 GROUP BY 2147483647 + id', []],
      ['SELECT 10 / (id - 2), 2147483647 + id, count(*) FROM relation_t GROUP BY 10 / (id - 2), 2147483647 + id', []],
      ['SELECT id, sum(v) OVER (ORDER BY id) FROM relation_t WHERE v + 1 > $1 ORDER BY id', [-9]],
      ['SELECT upper(v::text), count(*) FROM relation_t GROUP BY upper(v::text) ORDER BY 1', []],
      ['SELECT a.flag, count(*) FROM relation_t a JOIN relation_t b ON a.id = b.id WHERE a.v + b.v > $1 GROUP BY a.flag ORDER BY a.flag', [-20]]
    ]
    %i[execute query execute_prepared query_prepared].each do |api|
      cases.each { |sql, params| compare(database, sql, params, api: api) }
    end
    where_precedes_keys = compare(database, *cases[8])
    assert(where_precedes_keys[0] == :error && where_precedes_keys[2] == '22012', 'group key ran before all WHERE inputs')
    row_major_keys = compare(database, *cases[9])
    assert(row_major_keys[0] == :error && row_major_keys[2] == '22003', 'group keys were evaluated column-major')
    %i[execute_prepared query_prepared].each do |api|
      [cases[0], cases[1], cases[8]].each do |sql, params|
        complete = compare(database, sql, params, api: api)
        %i[max_cost lifetime_max_cost].each do |setting|
          (1..complete[-2] + 1).each { |limit| compare(database, sql, params, api: api, setting: setting, limit: limit) }
        end
        [1, 127, 256, 512, 2048, 1_048_576].each do |limit|
          compare(database, sql, params, api: api, setting: :max_query_memory_bytes, limit: limit, collect: true)
        end
      end
    end
    [true, false].each do |disabled|
      current = session(database, disabled: disabled)
      sql = 'SELECT sum(v) FROM relation_t WHERE 10 / v > 0'
      statement = current.prepare(sql)
      3.times { outcome(current, statement, sql, [], :query_prepared) }
      calls = dispatched { outcome(current, statement, sql, [], :query_prepared) }
      assert(disabled || !calls.empty?, 'source-fault probe did not warm a relation kernel')
      self.source_failure = true
      failure = outcome(current, statement, sql, [], :query_prepared)
      assert(failure[0] == :error && failure[2] == 'XX001', 'predicate ran before source materialization completed')
      self.source_failure = false
      current.close
    end
  ensure
    self.source_failure = false
  end

  def plan_reuse(database)
    %i[execute query execute_prepared query_prepared].each do |api|
      current = session(database)
      sql = 'SELECT id % $1, sum(v) FROM relation_t WHERE v + 1 > $2 GROUP BY id % $1 ORDER BY 1'
      statement = current.prepare(sql)
      assert(dispatched { outcome(current, statement, sql, [3, -8], api) }.empty?, 'first relation resolution compiled eagerly')
      kernels = dispatched { outcome(current, statement, sql, [3, -8], api) }.uniq
      assert(kernels.length == 2, "#{api} did not dispatch separate WHERE and key loops")
      [[2, 0], [4, -9], [nil, -9], [3, nil]].each do |params|
        assert(outcome(current, statement, sql, params, api) == compare(database, sql, params, api: api), 'relation program captured previous bindings')
      end
      assert(dispatched { outcome(current, statement, sql, [3, -8], api) }.uniq == kernels, 'unchanged relation programs rebuilt code')
      current.engine.instance_variable_set(:@native_relation_disabled, true)
      assert(dispatched { outcome(current, statement, sql, [3, -8], api) }.empty?, 'disabled cached relation program dispatched')
      current.engine.instance_variable_set(:@native_relation_disabled, false)
      assert(dispatched { outcome(current, statement, sql, [3, -8], api) }.uniq == kernels, 'reenabled relation program lost cache')
      [[], [1], [1, 2, 3], ['wrong', 1]].each do |params|
        assert(outcome(current, statement, sql, params, api).first == :error, 'warmed relation admitted invalid bindings')
      end
      current.close
    end
  end

  def host_fallback
    calls = []
    environment = JedNative::Environment.new.register_function(name: 'relation_probe', args: ['i32'], returns: 'i32', cost: 3) do |value|
      calls << value
      raise JedNative::Error.new('22023', 'relation callback failed') if value == -7
      value
    end
    JedNative.memory(environment: environment) do |database|
      setup(database)
      ['SELECT sum(v) FROM relation_t WHERE relation_probe(v) > 0',
       'SELECT relation_probe(v), count(*) FROM relation_t GROUP BY relation_probe(v) ORDER BY 1'].each do |sql|
        %i[execute_prepared query_prepared].each do |api|
          assert(dispatched { compare(database, sql, api: api, calls: calls) }.empty?, 'unsupported relation root partially compiled')
        end
      end
    end
  end

  def snapshots(database)
    reader, writer = session(database), database.session
    sql = 'SELECT id % $1, sum(v) FROM relation_t WHERE id > (SELECT v FROM relation_limits) GROUP BY id % $1 ORDER BY 1'
    statement = reader.prepare(sql)
    3.times { reader.query_prepared(statement, 3).rows }
    before, before_two = reader.query_prepared(statement, 3).rows, reader.query_prepared(statement, 2).rows
    captured, pending = reader.query_prepared(statement, 3), reader.query_prepared(statement, 2)
    first = captured.next.to_a
    writer.execute('UPDATE relation_limits SET v=4')
    writer.execute('UPDATE relation_t SET v=100 WHERE id=5')
    fresh = reader.query_prepared(statement, 3).rows
    assert(fresh != before && [first, *captured.map(&:to_a)] == before && pending.rows == before_two,
      'relation program retained a folded subquery or lost cursor snapshot/bindings')
    admitted = reader.query_prepared(statement, 3)
    reader.revoke(:select, 'relation_t')
    denied = outcome(reader, statement, sql, [3], :query_prepared)
    assert(denied[0] == :error && denied[2] == '42501' && admitted.rows == fresh, 'relation compilation bypassed captured/refreshed privileges')
    reader.reset_privileges
    captured = reader.query_prepared(statement, 3)
    writer.execute('DROP TABLE relation_t')
    writer.execute('CREATE TABLE relation_t(id i16 PRIMARY KEY, v i16, flag boolean)')
    writer.execute('INSERT INTO relation_t VALUES(5,7,TRUE)')
    assert(reader.query_prepared(statement, 3).rows == [[2, 7]] && captured.rows == fresh, 'relation program reused stale schema')
    reader.close
    writer.close
  end

  def cache_and_lifetime
    JedNative.memory do |database|
      setup(database)
      current = session(database)
      first = nil
      80.times do |index|
        statement = current.prepare("SELECT sum(v) FROM relation_t WHERE v + #{index} > 0")
        2.times { current.execute_prepared(statement) }
        kernel = dispatched { current.execute_prepared(statement) }.first
        assert(kernel, 'relation LRU did not admit repeated source')
        first ||= kernel
        assert(index.zero? || kernel.pool != first.pool, 'relation cache aliased different literal pools')
      end
      cache = current.engine.instance_variable_get(:@native_compiled_relations)
      assert(cache.length <= JedNative::CompiledPlans::MAX_RELATION_CACHE_ENTRIES && !cache.values.include?(first), 'relation cache exceeded bounds or failed eviction')
      current.close
    end
    # A finished thread releases its VM/native call stack. This avoids treating
    # conservative stale argument slots as retention by the generated kernel.
    kernel, *references = Thread.new { detached_kernel }.value
    3.times { GC.start }
    assert(references.none?(&:weakref_alive?), 'relation kernel captured engine, input or parameters')
    assert(kernel.pool.none? { |value| value.is_a?(Array) || value.is_a?(Hash) || value.is_a?(JedNative::Expression) }, 'relation kernel retained mutable resolution data')
    assert(kernel.call(engine, VALUES, []) == [[2], [3], [4], [5]], 'detached relation kernel lost usability')
  end

  def detached_kernel
    current = engine
    expressions, types = resolve('SELECT id + 1', :keys)
    kernel = JedNative::CompiledRelation.prepare_keys(expressions, types: types).materialize
    rows, params = VALUES.map(&:dup), []
    kernel.call(current, rows, params)
    [kernel, WeakRef.new(current), WeakRef.new(rows), WeakRef.new(params)]
  end

  def file_fallback
    Dir.mktmpdir('jed-compiled-relation-') do |directory|
      JedNative.create(File.join(directory, 'relation.jed'), locking: :exclusive, work_mem: 128) do |database|
        setup(database)
        sql = 'SELECT id % 2, count(*) FROM relation_t WHERE v + 1 > -9 GROUP BY id % 2 ORDER BY 1'
        assert(dispatched { compare(database, sql) }.empty?, 'file query entered memory relation compiler')
      end
    end
  end

  def nonarray_fallback
    JedNative.memory do |database|
      setup(database)
      self.nonarray_source = true
      sql = 'SELECT sum(v) FROM relation_t WHERE v + 1 > -9'
      assert(dispatched { compare(database, sql) }.empty?, 'non-Array source entered indexed relation filter')
      self.nonarray_source = false
      self.nonarray_groups = true
      sql = 'SELECT id % 2, count(*) FROM relation_t GROUP BY id % 2 ORDER BY 1'
      assert(dispatched { compare(database, sql) }.empty?, 'non-Array group input entered indexed key loop')
    end
  ensure
    self.nonarray_source = self.nonarray_groups = false
  end

  def run
    direct_loops
    admission_and_source
    JedNative.memory do |database|
      setup(database)
      plan_reuse(database)
      api_equivalence(database)
      snapshots(database)
    end
    host_fallback
    cache_and_lifetime
    file_fallback
    nonarray_fallback
    puts 'ruby-native compiled relation: row/key order, exact guards, ownership release, warmed APIs, cache bounds and cursor captures passed'
  end
end

NativeCompiledRelationTests.run
