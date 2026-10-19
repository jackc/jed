# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# SQL ordering is specified by the shared corpus. These checks exercise the
# generated comparator's admission, plan reuse, and host execution boundaries.
module NativeCompiledOrderingTests
  module_function

  class << self
    attr_accessor :sorter_probe
  end

  module SorterProbe
    def initialize(...)
      super
      NativeCompiledOrderingTests.sorter_probe&.fetch(:sorters)&.push(self)
    end

    def flush
      had_rows = !@rows.empty?
      super
      probe = NativeCompiledOrderingTests.sorter_probe
      probe[:flushes] += 1 if probe && had_rows && @runs.any?
    end
  end

  JedNative::ExternalSorter.prepend(SorterProbe)

  def assert(value, message) = (raise(message) unless value)
  def key(type, kind: :column, value: 0)
    JedNative::Expression.new(kind: kind, type: type, value: value)
  end

  def reference(engine, orders, left, right)
    orders.each_with_index do |order, index|
      comparison = engine.order_compare(left[1][index], right[1][index], order)
      return comparison unless comparison.zero?
    end
    left[2] <=> right[2]
  end

  def check_comparator(types, orders, values)
    keys = types.map { |type| key(type, kind: %w[null unknown].include?(type) ? :literal : :column, value: %w[null unknown].include?(type) ? nil : 0) }
    program = JedNative::CompiledOrdering.prepare(keys, orders)
    assert(program, "supported comparator rejected: #{types.inspect} #{orders.inspect}")
    assert(program.frozen? && program.source.frozen? && program.pool.frozen? && program.pool.empty?, 'ordering program retained mutable data')
    kernel = program.materialize
    assert(kernel.frozen? && kernel.to_proc.equal?(kernel.to_proc), 'ordering block was not retained by its immutable kernel')
    assert(kernel.source_bytes == kernel.source.bytesize && kernel.source_bytes <= JedNative::CompiledExpression::MAX_SOURCE_BYTES,
      'ordering source exceeded its method cap')
    instance = JedNative::Engine.new
    entries = values.each_with_index.map { |row, index| [Object.new, row, index] }
    # Include duplicate values with distinct ordinals: their SQL sort keys tie.
    entries.concat(values.each_with_index.map { |row, index| [Object.new, row, index + values.length] })
    entries.each do |left|
      entries.each do |right|
        expected = reference(instance, orders, left, right)
        assert(kernel.call(left, right) == expected, "comparator changed #{types.inspect}: #{left[1].inspect} vs #{right[1].inspect}")
      end
    end
    expected = entries.reverse.sort { |left, right| reference(instance, orders, left, right) }
    assert(entries.reverse.sort(&kernel) == expected, 'sort block changed ordinal tie order')
    assert(instance.cost.zero? && instance.lifetime_cost.zero?, 'ordering comparisons accrued SQL work')
    kernel
  end

  def direct_comparators
    directions = [nil, :asc, :desc, 'asc', 'desc']
    nulls = [nil, :first, :last, 'first', 'last']
    directions.product(nulls).each do |direction, placement|
      orders = [{ direction: direction, nulls: placement }]
      check_comparator(['i64'], orders, [nil, -(1 << 63), -1, 0, 1, (1 << 63) - 1].map { |value| [value] })
      check_comparator(['boolean'], orders, [[nil], [false], [true]])
    end
    [{ desc: true }, { desc: false }, { direction: :asc, desc: true }, { direction: :desc, desc: false }].each do |order|
      check_comparator(['i16'], [order], [[nil], [-32_768], [0], [32_767]])
    end
    vectors = [nil, -2, 2].product([nil, false, true], [nil, -1, 1])
    check_comparator(%w[i64 boolean i32], [{ direction: :desc, nulls: :last }, { nulls: :first }, { direction: :desc }], vectors)
    check_comparator(%w[unknown null i32], [{}, { direction: :desc }, {}], [[nil, nil, -1], [nil, nil, 0], [nil, nil, nil]])
  end

  def admission_and_source
    compiler = JedNative::CompiledOrdering
    assert(compiler.build([], []).nil?, 'empty key list compiled')
    assert(compiler.build([key('i64')], []).nil?, 'mismatched order list compiled')
    assert(compiler.build([Object.new], [{}]).nil?, 'unresolved key compiled')
    assert(compiler.build([key('i64')], [Object.new]).nil?, 'invalid ordering template compiled')
    ['text', 'decimal', 'f64', 'date', 'i64[]', 'record', 'custom'].each do |type|
      assert(compiler.build([key(type)], [{}]).nil?, "unsupported key type compiled: #{type}")
    end
    assert(compiler.build([key('i64')], [{}], types: { 'i64' => [] }).nil?, 'registered type shadow compiled as primitive')
    collated = key('i64')
    collated.instance_variable_set(:@native_collation, Object.new)
    assert(compiler.build([collated], [{}]).nil?, 'collated key entered primitive comparator')
    assert(compiler.build([key('unknown', kind: :literal, value: 'unresolved')], [{}]).nil?, 'unresolved string entered null comparator')
    assert(compiler.build([key('null')], [{}]).nil?, 'nonliteral null pseudo-type compiled')
    [{ direction: 'DESC' }, { direction: :sideways }, { nulls: 'FIRST' }, { nulls: :middle }, { desc: 'true' }].each do |order|
      assert(compiler.build([key('i64')], [order]).nil?, "unvalidated order template compiled: #{order.inspect}")
    end
    payload = "0]; raise 'ordering source injection'; #"
    safe = compiler.build([key('i64', kind: :call, value: payload)], [{ expr: { kind: :literal, value: payload } }])
    assert(safe && !safe.source.include?(payload) && safe.pool.empty?, 'sort-key syntax became generated Ruby source')
    assert(safe.call([nil, [1], 0], [nil, [2], 1]) == -1, 'precomputed key comparator evaluated its expression')
    [{ direction: payload }, { nulls: payload }, { desc: payload }].each do |order|
      assert(compiler.build([key('i64')], [order]).nil?, 'source-bearing ordering template was admitted')
    end
    assert(compiler.build([key(payload)], [{}]).nil?, 'source-bearing type was admitted')
    maximum = JedNative::CompiledExpression::MAX_NODES
    assert(compiler.build(Array.new(maximum + 1) { key('i64') }, Array.new(maximum + 1) { {} }).nil?, 'ordering key count exceeded its cap')
    [16, 32, 64, maximum].each do |width|
      kernel = compiler.build(Array.new(width) { key('i64') }, Array.new(width) { {} })
      assert(!kernel || kernel.source_bytes <= JedNative::CompiledExpression::MAX_SOURCE_BYTES, 'wide comparator exceeded source cap')
    end
  end

  def setup(database)
    database.execute('CREATE TABLE ordering_t(id i64 PRIMARY KEY, v i64, flag boolean, label text)')
    database.execute("INSERT INTO ordering_t VALUES(1,3,TRUE,'one'),(2,NULL,NULL,'two'),(3,-7,FALSE,'three'),(4,0,TRUE,'four'),(5,3,FALSE,'five'),(6,-7,TRUE,'six'),(7,NULL,FALSE,'seven'),(8,0,NULL,'eight')")
  end

  def session(database, disabled: false)
    current = database.session
    current.engine.instance_variable_set(:@native_ordering_disabled, disabled)
    current
  end

  def states(current)
    %i[@native_select_plans @native_read_plans].flat_map do |name|
      (current.engine.instance_variable_get(name) || {}).values.filter_map { |plan| plan.instance_variable_get(:@native_compiled_state) }
    end
  end

  def kernels(current) = states(current).filter_map { |state| state.kernels.ordering }

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

  def compare(database, sql, params = [], api: :query_prepared, setting: nil, limit: 0, collect: false, take: nil, calls: nil)
    outcomes = [true, false].map do |disabled|
      current = session(database, disabled: disabled)
      statement = current.prepare(sql)
      3.times { outcome(current, statement, sql, params, api) }
      if setting == :lifetime_max_cost
        current.lifetime_max_cost = current.lifetime_cost + limit
      elsif setting
        current.public_send("#{setting}=", limit)
      end
      calls&.clear
      result = outcome(current, statement, sql, params, api, collect: collect, take: take)
      result << calls.dup if calls
      result
    ensure
      current&.close
    end
    assert(outcomes[0] == outcomes[1], "compiled ordering changed #{api} #{sql} #{params.inspect} #{setting}=#{limit}: #{outcomes.inspect}")
    outcomes[1]
  end

  def dispatched
    count = 0
    trace = TracePoint.new(:call) do |event|
      count += 1 if event.method_id == :call && event.self.is_a?(JedNative::CompiledOrdering::Kernel)
    end
    trace.enable { yield }
    count
  ensure
    trace&.disable
  end

  def plan_reuse(database)
    %i[execute query execute_prepared query_prepared].each do |api|
      current = session(database)
      sql = 'SELECT id + $1, v FROM ordering_t ORDER BY v DESC NULLS LAST, flag, id LIMIT $2 OFFSET $3'
      statement = current.prepare(sql)
      first = outcome(current, statement, sql, [1, 3, 1], api)
      assert(kernels(current).empty?, 'first-use ordering plan compiled eagerly')
      calls = dispatched { assert(outcome(current, statement, sql, [1, 3, 1], api) == first, 'warm ordering changed its result') }
      assert(calls.positive? && !kernels(current).empty?, "#{api} never dispatched a generated comparator")
      identities = kernels(current).map(&:object_id)
      [[10, 2, 0], [nil, nil, nil], [1, 0, 0], [1, 2, 50]].each do |params|
        expected = compare(database, sql, params, api: api)
        assert(outcome(current, statement, sql, params, api) == expected, 'ordering plan retained earlier parameter values')
      end
      assert(kernels(current).map(&:object_id) == identities, 'unchanged ordering plan rebuilt generated methods')
      current.engine.instance_variable_set(:@native_ordering_disabled, true)
      assert(dispatched { outcome(current, statement, sql, [1, 3, 1], api) }.zero?, 'disable switch dispatched an already-cached comparator')
      current.engine.instance_variable_set(:@native_ordering_disabled, false)
      assert(dispatched { outcome(current, statement, sql, [1, 3, 1], api) }.positive?, 'reenabled ordering failed to reuse its comparator')
      [[], [1, 2], [1, 2, 3, 4], ['wrong', 2, 0]].each do |params|
        assert(outcome(current, statement, sql, params, api)[0] == :error, 'warm ordering admitted malformed bindings')
      end
      current.close
    end
  end

  def api_equivalence(database)
    cases = [
      ['SELECT id, v, flag FROM ordering_t ORDER BY v DESC NULLS LAST, flag, id DESC', []],
      ['SELECT id + $1 FROM ordering_t ORDER BY v + $2, flag DESC NULLS LAST LIMIT $3 OFFSET $4', [10, -1, 3, 2]],
      ['SELECT id FROM ordering_t ORDER BY v + $1 NULLS FIRST LIMIT 3', [nil]],
      ['SELECT id FROM ordering_t ORDER BY NULL, id + 0 DESC', []],
      ['SELECT id FROM ordering_t ORDER BY v LIMIT 0', []],
      ['SELECT id FROM ordering_t ORDER BY 10 / v LIMIT 0', []],
      ['SELECT 10 / (id - 2) FROM ordering_t ORDER BY v NULLS LAST LIMIT 2', []],
      ['SELECT 10 / (id - 2) FROM ordering_t ORDER BY v NULLS LAST', []],
      ['SELECT id FROM ordering_t ORDER BY v + $1', [(1 << 63) - 1]],
      ['SELECT id FROM ordering_t WHERE id > 20 ORDER BY v', []],
      ['SELECT id FROM ordering_t ORDER BY upper(label), id', []],
      ['SELECT id FROM ordering_t ORDER BY v::decimal, id', []]
    ]
    %i[execute query execute_prepared query_prepared].each do |api|
      cases.each { |sql, params| compare(database, sql, params, api: api) }
    end
    %i[execute_prepared query_prepared].each do |api|
      [cases[0], cases[1], cases[7]].each do |sql, params|
        complete = compare(database, sql, params, api: api)
        %i[max_cost lifetime_max_cost].each do |setting|
          (1..complete[-2] + 1).each { |limit| compare(database, sql, params, api: api, setting: setting, limit: limit) }
        end
        [1, 127, 256, 512, 2048, 1_048_576].each do |limit|
          compare(database, sql, params, api: api, setting: :max_query_memory_bytes, limit: limit, collect: true)
        end
      end
    end
    compare(database, cases[0][0], take: 1)
  end

  def snapshots_and_lifetime(database)
    reader, writer = session(database), database.session
    sql = 'SELECT id + $1, v FROM ordering_t ORDER BY v DESC NULLS LAST, id'
    statement = reader.prepare(sql)
    3.times { reader.query_prepared(statement, 1).rows }
    assert(!kernels(reader).empty?, 'snapshot probe never generated an ordering comparator')
    before = reader.query_prepared(statement, 1).rows
    old = reader.query_prepared(statement, 1)
    pending = reader.query_prepared(statement, 10)
    assert(old.next.to_a == before.first, 'first sorted cursor row changed')
    cost = old.cost
    writer.execute('UPDATE ordering_t SET v=100 WHERE id=3')
    fresh = reader.query_prepared(statement, 1).rows
    assert(fresh.first == [4, 100], 'ordering plan retained a prior publication')
    assert(pending.rows == before.map { |id, value| [id + 9, value] }, 'pending sorted cursor lost captured parameters or publication')
    old.close
    assert(old.cost == cost && old.closed?, 'closing a sorted cursor did extra work')

    admitted = reader.query_prepared(statement, 1)
    reader.revoke(:select, 'ordering_t')
    denied = outcome(reader, statement, sql, [1], :query_prepared)
    assert(denied[0] == :error && denied[2] == '42501', 'ordering bypassed refreshed permissions')
    assert(admitted.rows == fresh, 'admitted ordering cursor lost captured permissions')
    reader.reset_privileges
    complete = reader.query_prepared(statement, 1)
    complete.rows
    prior = reader.lifetime_cost
    reader.lifetime_max_cost = prior + complete.cost + 1
    first, second = reader.query_prepared(statement, 1), reader.query_prepared(statement, 2)
    first.rows
    begin
      second.next
      raise 'second sorted cursor escaped shared lifetime ceiling'
    rescue JedNative::Error => error
      assert(error.sqlstate == '54P02' && reader.lifetime_cost >= reader.lifetime_max_cost, 'sort cursor lost shared lifetime accounting')
    ensure
      first.close
      second.close
    end
    reader.lifetime_max_cost = 0

    captured = reader.query_prepared(statement, 1)
    writer.execute('DROP TABLE ordering_t')
    writer.execute('CREATE TABLE ordering_t(id i16 PRIMARY KEY, v text)')
    writer.execute("INSERT INTO ordering_t VALUES(1,'new')")
    replacement = reader.query_prepared(statement, 1)
    assert(replacement.rows == [[2, 'new']] && replacement.column_types == %w[i16 text], 'sort plan retained replaced schema')
    assert(captured.rows == fresh && captured.column_types == %w[i64 i64], 'captured sort lost its old schema')
    reader.close
    writer.close
  end

  def callbacks_and_fallback
    calls = []
    environment = JedNative::Environment.new.register_function(name: 'ordering_probe', args: ['i64'], returns: 'i64', cost: 3) do |value|
      calls << value
      raise JedNative::Error.new('22023', 'ordering callback failure') if value == 99
      value + 10
    end
    JedNative.memory(environment: environment) do |database|
      setup(database)
      [
        'SELECT ordering_probe(id) FROM ordering_t ORDER BY v, id LIMIT 2',
        'SELECT id FROM ordering_t ORDER BY ordering_probe(id), v LIMIT 2',
        'SELECT ordering_probe(99) FROM ordering_t ORDER BY 10 / v LIMIT 2',
        'SELECT id FROM ordering_t ORDER BY ordering_probe(id + 96), v LIMIT 2'
      ].each do |sql|
        %i[query_prepared execute_prepared].each { |api| compare(database, sql, api: api, calls: calls) }
      end
      ['SELECT id FROM ordering_t ORDER BY label', 'SELECT id FROM ordering_t ORDER BY v::decimal'].each do |sql|
        current = session(database)
        statement = current.prepare(sql)
        3.times { outcome(current, statement, sql, [], :query_prepared) }
        assert(kernels(current).empty?, 'unsupported ordering type generated a comparator')
        current.close
      end
    end
  end

  def cache_bounds
    JedNative.memory do |database|
      setup(database)
      current = session(database)
      80.times do |index|
        sql = "SELECT id + #{index} FROM ordering_t ORDER BY v, flag DESC"
        statement = current.prepare(sql)
        2.times { current.query_prepared(statement).rows }
      end
      cache = current.engine.instance_variable_get(:@native_read_plans)
      assert(cache.length <= JedNative::PreparedSelect::CACHE_ENTRIES && !kernels(current).empty?, 'ordering cache admission or bound failed')
      states(current).each do |state|
        bytes = state.kernels.to_a.compact.sum(&:source_bytes)
        assert(bytes == state.source_bytes && bytes <= JedNative::CompiledPlans::MAX_PLAN_SOURCE_BYTES,
          'ordering code escaped the shared plan source budget')
        assert(state.uses <= 2, 'ordering reuse counter grew without bound')
      end
      current.close
    end
  end

  def file_sorting
    Dir.mktmpdir('jed-compiled-ordering-') do |directory|
      JedNative.create(File.join(directory, 'sort.jed'), locking: :exclusive, work_mem: 128) do |database|
        setup(database)
        spill_cleanup(database)
        plan_reuse(database)
        api_equivalence(database)
        snapshots_and_lifetime(database)
      end
    end
  end

  def spill_cleanup(database)
    self.sorter_probe = { sorters: [], flushes: 0 }
    current = session(database)
    sql = 'SELECT id, v FROM ordering_t ORDER BY v, id'
    statement = current.prepare(sql)
    count = dispatched { 3.times { outcome(current, statement, sql, [], :query_prepared) } }
    assert(count.positive? && sorter_probe[:flushes].positive?, 'file sort did not spill with its generated comparator')
    outcome(current, statement, sql, [], :query_prepared, take: 1)
    failure = 'SELECT 10 / (id - 3) FROM ordering_t ORDER BY v, id'
    failed = outcome(current, current.prepare(failure), failure, [], :query_prepared)
    assert(failed[0] == :error && failed[2] == '22012', 'spilled projection failed at the wrong checkpoint')
    assert(!sorter_probe[:sorters].empty? && sorter_probe[:sorters].all? { |sorter| sorter.instance_variable_get(:@runs).nil? && sorter.resident_bytes.zero? },
      'drain, partial close, or failed projection retained external sort runs')
  ensure
    current&.close
    self.sorter_probe = nil
  end

  def run
    direct_comparators
    admission_and_source
    JedNative.memory do |database|
      setup(database)
      plan_reuse(database)
      api_equivalence(database)
      snapshots_and_lifetime(database)
    end
    callbacks_and_fallback
    cache_bounds
    file_sorting
    puts 'ruby-native compiled ordering: comparator parity, admission, bounded source, warmed APIs, exact budgets, callbacks, snapshots and file sorting passed'
  end
end

NativeCompiledOrderingTests.run
