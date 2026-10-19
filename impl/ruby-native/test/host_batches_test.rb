# frozen_string_literal: true

require_relative '../lib/jed_native'
require 'tmpdir'
module NativeHostBatchTests
  module_function
  def outcome(session, sql, budget = 0)
    session.max_cost = budget
    result = session.execute(sql)
    [result.rows, result.cost]
  rescue JedNative::Error => error
    [error.sqlstate, session.engine.cost]
  ensure
    session.max_cost = 0
  end
  def cursor_outcome(session, sql, budget = 0)
    session.max_cost = budget
    rows = []
    cursor = session.query(sql)
    raise 'batch cursor executed before its first pull' unless cursor.cost.zero?
    cursor.each { |row| rows << row }
    [rows, nil, cursor.cost]
  rescue JedNative::Error => error
    [rows, error.sqlstate, cursor&.cost || session.engine.cost]
  ensure
    cursor&.close
    session.max_cost = 0
  end

  def expression(function)
    argument = JedNative::Expression.new(kind: :column, type: 'i64', value: 0)
    JedNative::Expression.new(kind: :host_call, type: function.result, value: function, args: [argument])
  end

  def assert_clean_replay(engine)
    raise 'source charge capture leaked out of batch replay' if engine.instance_variable_get(:@host_batch_source_charges)
    raise 'batch frame leaked out of replay' unless (engine.instance_variable_get(:@host_batch_stack) || []).empty?
  end

  # Source faults, exact charge boundaries and unfetched storage values cannot
  # be supplied by a SQL corpus entry. Exercise those internal replay seams.
  def source_replay
    function = JedNative::HostFunction.new('host_map', args: ['i64'], returns: 'i64', cost: 3,
      volatility: :stable, batch_kernel: ->(columns, output) { columns[0].each { |value| output << value + 1 } })
    call = expression(function)
    [:drain, :close, :replay_error].each do |mode|
      engine = JedNative::Engine.new
      source = Enumerator.new do |output|
        engine.charge(2)
        output << [1]
        engine.charge(5)
        output << [2]
        engine.charge(7)
        raise JedNative::Error.new('58030', 'deferred source fault')
      end
      rows, state = [], nil
      begin
        engine.host_batch_rows(source, [call], pulled: true).each do |row|
          engine.charge
          raise JedNative::Error.new('22012', 'earlier replay error') if mode == :replay_error
          rows << [engine.evaluate(call, row), engine.cost]
          break if mode == :close
        end
      rescue JedNative::Error => error
        state = error.sqlstate
      end
      expected = case mode
                 when :drain then [[[2, 6], [3, 15]], '58030', 22]
                 when :close then [[[2, 6]], nil, 6]
                 when :replay_error then [[], '22012', 3]
                 end
      raise "source replay changed #{mode}: #{[rows, state, engine.cost].inspect}" unless [rows, state, engine.cost] == expected
      assert_clean_replay(engine)
    end

    engine = JedNative::Engine.new
    nested = []
    engine.host_batch_rows([[1], [2]], [call]).each do |outer|
      nested << engine.evaluate(call, outer)
      engine.host_batch_rows([[10]], [call]).each { |inner| nested << engine.evaluate(call, inner) }
      nested << engine.evaluate(call, outer)
    end
    raise 'nested batch replay lost its outer outcomes' unless nested == [2, 11, 2, 3, 11, 3]
    assert_clean_replay(engine)

    engine = JedNative::Engine.new(max_cost: 3)
    source = Enumerator.new do |output|
      [1, 0, 2, 7].each { |units| engine.charge(units) }
      output << [1]
    end
    begin
      engine.host_batch_rows(source, [call], pulled: true).each { raise 'source cost abort reached row replay' }
      raise 'source cost abort was lost'
    rescue JedNative::Error => error
      raise unless error.sqlstate == '54P01' && engine.cost == 3
    end
    assert_clean_replay(engine)

    engine = JedNative::Engine.new
    source = Enumerator.new do |output|
      engine.host_batch_rows([[1], [2]], [call], pulled: true).each do |row|
        engine.charge
        output << [engine.evaluate(call, row)]
      end
    end
    nested = []
    engine.host_batch_rows(source, [call], pulled: true).each do |row|
      nested << [engine.evaluate(call, row), engine.cost]
    end
    raise 'nested source capture lost an individual charge' unless nested == [[3, 7], [4, 14]]
    assert_clean_replay(engine)
  end

  class DeferredLeaf
    attr_reader :reads, :columns
    def initialize
      @reads = 0
      @columns = [{ type: 'i64' }]
    end
    def units(*) = [1]
    def value(*)
      @reads += 1
      raise JedNative::Error.new('58030', 'unfetched value fault')
    end
  end

  def lazy_values
    called = []
    function = JedNative::HostFunction.new('host_map', args: ['i64'], returns: 'i64', cost: 3,
      volatility: :stable, batch_kernel: lambda do |columns, output|
        called.concat(columns[0])
        columns[0].each { |value| output << value + 1 }
      end)
    call, engine, leaf = expression(function), JedNative::Engine.new, DeferredLeaf.new
    rows = [[1], JedNative::PagedRow.new(leaf, 0)]
    values = []
    begin
      engine.host_batch_rows(rows, [call]).each do |row|
        raise 'batch prefetch touched an unfetched value' unless leaf.reads.zero?
        values << engine.evaluate(call, row)
      end
      raise 'unfetched value did not fail at replay'
    rescue JedNative::Error => error
      raise unless error.sqlstate == '58030'
    end
    raise 'lazy value changed the successful prefix or host call set' unless values == [2] && called == [1]
    raise 'lazy fault moved across its host call charge' unless leaf.reads == 1 && engine.cost == 6
    assert_clean_replay(engine)
  end

  def kernel_shapes
    kernels = {
      short: [->(_columns, output) { output << 11 }, [[11], '22000']],
      excess: [->(_columns, output) { output.concat([11, 12, 13, 14]) }, [[], '22000']],
      failed_after_all: [lambda { |_columns, output| output.concat([11, 12, 13]); raise 'late failure' }, [[], '22000']],
      failed_prefix: [lambda { |_columns, output| output << 11; raise JedNative::Error.new('22012', 'row failure') }, [[11], '22012']],
      wrong_kind: [->(_columns, output) { output.concat([11, 'wrong', 13]) }, [[11], '22000']],
      wrong_width: [->(_columns, output) { output.concat([11, 1 << 20, 13]) }, [[11], '22003']]
    }
    kernels.each do |name, (kernel, expected)|
      function = JedNative::HostFunction.new('host_map', args: ['i64'], returns: 'i16', cost: 3,
        volatility: :stable, batch_kernel: kernel)
      call, engine, values, state = expression(function), JedNative::Engine.new, [], nil
      begin
        engine.host_batch_rows([[1], [2], [3]], [call]).each { |row| values << engine.evaluate(call, row) }
      rescue JedNative::Error => error
        state = error.sqlstate
      end
      raise "kernel shape #{name} changed its prefix: #{[values, state].inspect}" unless [values, state] == expected
      assert_clean_replay(engine)
    end
  end

  def nullable_results_and_zero_arguments
    batches = []
    function = JedNative::HostFunction.new('host_boolean', args: ['i64'], returns: 'boolean', cost: 3,
      volatility: :stable, batch_kernel: lambda do |columns, output|
        batches << columns[0].dup
        columns[0].each { |value| output << { 1 => false, 2 => nil, 3 => true }.fetch(value) }
      end)
    call, engine = expression(function), JedNative::Engine.new
    values = engine.host_batch_rows([[1], [nil], [2], [3]], [call]).map { |row| engine.evaluate(call, row) }
    raise 'false or NULL batch result was mistaken for a missing outcome' unless values == [false, nil, nil, true]
    raise 'cached false or NULL reentered the kernel, or strict NULL reached it' unless batches == [[1, 2, 3]]
    raise 'nullable batch results changed declared call cost' unless engine.cost == 12
    assert_clean_replay(engine)

    calls = 0
    function = JedNative::HostFunction.new('host_zero', args: [], returns: 'boolean', cost: 3,
      volatility: :stable, batch_kernel: lambda do |columns, output|
        raise 'zero-argument kernel received argument columns' unless columns.empty?
        calls += 1
        output << false
      end)
    call = JedNative::Expression.new(kind: :host_call, type: 'boolean', value: function, args: [])
    engine = JedNative::Engine.new
    values = engine.host_batch_rows([[1], [2]], [call]).map { |row| engine.evaluate(call, row) }
    raise 'zero-argument calls were speculatively grouped' unless values == [false, false] && calls == 2 && engine.cost == 6
    assert_clean_replay(engine)
  end

  def run
    batches = []
    scalar = JedNative::Environment.new.register_function(name: 'host_map', args: ['i64'], returns: 'i64', cost: 3, volatility: :stable) do |value|
      raise JedNative::Error.new('22012', 'host row failure') if value == 77
      value + 1
    end
    batch = JedNative::Environment.new.register_function(name: 'host_map', args: ['i64'], returns: 'i64', cost: 3, volatility: :stable,
      batch_kernel: lambda do |columns, output|
        batches << columns[0].length
        columns[0].each do |value|
          raise JedNative::Error.new('22012', 'host row failure') if value == 77
          output << value + 1
        end
      end)
    Dir.mktmpdir('ruby-host-batches') do |directory|
      databases = [scalar, batch].each_with_index.map do |environment, index|
        database = JedNative.create(File.join(directory, "#{index}.jed"), environment: environment, page_size: 256)
        database.execute('create table t(id i64 primary key,v i64)')
        database.execute('insert into t values ' + (1..160).map { |value| "(#{value},#{value == 9 ? 'NULL' : value})" }.join(','))
        database
      end
      sessions = databases.map(&:session)
      queries = [
        'select host_map(v) from t where id<70',
        'select host_map(v) from t order by id limit 70',
        'select host_map(v) from t where host_map(v)>20 order by id limit 30 offset 4',
        'select host_map(v) from t order by v desc limit 90',
        'select 1/(v-76),host_map(v) from t order by id',
        'select case when id<5 then host_map(v) else 0 end from t order by id',
        'select host_map(v) from t order by id limit 1'
      ]
      queries.each do |sql|
        [0, 1, 10, 70, 300].each do |budget|
          expected, actual = sessions.map { |session| outcome(session, sql, budget) }
          raise "batch replay differs #{budget} #{sql}: #{expected.inspect} vs #{actual.inspect}" unless expected == actual
          expected, actual = sessions.map { |session| cursor_outcome(session, sql, budget) }
          raise "lazy batch replay differs #{budget} #{sql}: #{expected.inspect} vs #{actual.inspect}" unless expected == actual
        end
      end
      raise 'eligible host call was never batched' unless batches.any? { |size| size > 1 }
      raise 'streaming host batch exceeded its bound' unless batches.max <= 1024
      sessions.each(&:close)
      databases.each(&:close)
    end
    source_replay
    lazy_values
    kernel_shapes
    nullable_results_and_zero_arguments
    puts 'ruby-native host batches: eager/lazy scalar equivalence, source replay, deferred values, kernel errors and nested frames passed'
  end
end
NativeHostBatchTests.run
