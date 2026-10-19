# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# The shared corpus owns SQL behavior. These tests cover the generated loop's
# internal schedule, lazy host APIs, retained plans and conservative admission.
module NativeCompiledPipelineTests
  module_function

  COLUMNS = [{ name: 'id', type: 'i64' }, { name: 'v', type: 'i64' },
             { name: 'flag', type: 'boolean' }].freeze
  VALUES = [[1, 3, true], [2, nil, nil], [3, -7, false], [4, 0, true]].freeze

  def assert(condition, message) = (raise(message) unless condition)

  def engine(params = [], types = [])
    instance = JedNative::Engine.new
    instance.instance_variable_set(:@params, params)
    instance.instance_variable_set(:@native_binding_types, types)
    instance
  end

  def resolve(sql, types = [], columns = COLUMNS)
    instance = engine(Array.new(types.length), types)
    ast = JedNative::Statement.new(sql).ast
    predicate = instance.compile_filter(ast[:where], columns)
    projections = ast[:columns].map { |column| instance.compile(column[:expr], columns) }
    [predicate, projections, instance.instance_variable_get(:@types)]
  end

  class ProbeRow
    def initialize(values, index, trace, failure)
      @values, @index, @trace, @failure = values, index, trace, failure
    end

    def [](column)
      @trace << [:column, @index, column]
      raise JedNative::Error.new('XX001', 'deferred column failure') if @failure == [@index, column]
      @values[column]
    end
  end

  class ProbeInput
    def initialize(values, trace, failure, input_failure)
      @trace, @input_failure = trace, input_failure
      @rows = values.each_with_index.map { |row, index| ProbeRow.new(row, index, trace, failure) }
    end

    def length = @rows.length

    def [](index)
      @trace << [:input, index]
      raise JedNative::Error.new('XX001', 'deferred scan failure') if @input_failure == index
      @rows[index]
    end

    def each
      index = 0
      while index < length
        yield self[index]
        index += 1
      end
    end
  end

  class ProbeOutput < Array
    def initialize(trace, failure)
      @trace, @failure = trace, failure
      super()
    end

    def <<(row)
      @trace << [:output, length, row]
      raise JedNative::Error.new('XX001', 'deferred output failure') if @failure == length
      super
    end
  end

  # This intentionally follows each existing executor's accounting boundary:
  # array scans charge their input and output outside the loop; stream scans
  # charge both at the row's original checkpoints, around the expression work.
  def interpreted_loop(instance, input, output, predicate, projections, mode, offset, limit)
    if mode == :array
      index = 0
      while index < input.length
        row = input[index]
        index += 1
        next if predicate && instance.evaluate(predicate, row) != true
        output << projections.map { |expression| instance.evaluate(expression, row) }
      end
    elsif limit != 0
      input.each do |row|
        instance.charge
        next if predicate && instance.evaluate(predicate, row) != true
        if offset.positive?
          offset -= 1
          next
        end
        values = projections.map { |expression| instance.evaluate(expression, row) }
        instance.charge
        output << values
        break if limit && output.length >= limit
      end
    end
    output
  end

  def loop_outcome(predicate, projections, kernel, mode, params, types, values, offset, limit,
                   failure, input_failure, output_failure, max_cost, lifetime_max_cost, prior)
    instance = engine(params, types)
    instance.max_cost, instance.lifetime_max_cost = max_cost, lifetime_max_cost
    instance.instance_variable_set(:@lifetime_cost, prior)
    trace = []
    input = ProbeInput.new(values, trace, failure, input_failure)
    output = ProbeOutput.new(trace, output_failure)
    begin
      if kernel
        kernel.call(instance, input, params, output, offset, limit)
      else
        interpreted_loop(instance, input, output, predicate, projections, mode, offset, limit)
      end
      result = [:rows, output.to_a]
    rescue JedNative::Error => error
      result = [:error, error.sqlstate, error.message, output.to_a]
    end
    [*result, instance.cost, instance.lifetime_cost, trace]
  end

  def compare_loop(sql, mode:, params: [], types: [], values: VALUES, offset: 0, limit: nil,
                   failure: nil, input_failure: nil, output_failure: nil)
    predicate, projections, registry = resolve(sql, types)
    kernel = JedNative::CompiledPipeline.build(predicate, projections, types: registry,
      mode: mode, offset: offset.positive?, limit: !limit.nil?)
    assert(kernel, "supported pipeline was not compiled: #{sql}")
    args = [mode, params, types, values, offset, limit, failure, input_failure, output_failure]
    reference = loop_outcome(predicate, projections, nil, *args, 0, 0, 0)
    actual = loop_outcome(predicate, projections, kernel, *args, 0, 0, 0)
    assert(actual == reference, "pipeline changed #{sql} #{mode}: #{actual.inspect} != #{reference.inspect}")
    (1..reference[-3] + 1).each do |boundary|
      [[boundary, 0, 0], [0, boundary + 7, 7], [boundary, boundary + 7, 7]].each do |limits|
        expected = loop_outcome(predicate, projections, nil, *args, *limits)
        observed = loop_outcome(predicate, projections, kernel, *args, *limits)
        assert(observed == expected,
          "pipeline changed cost prefix #{sql} #{mode} #{limits.inspect}: #{observed.inspect} != #{expected.inspect}")
      end
    end
    kernel
  end

  def direct_schedules
    %i[array stream].each do |mode|
      compare_loop('SELECT id + $1, v * 2 FROM pipeline_t WHERE flag AND id > 0',
        mode: mode, params: [10], types: ['i64'])
      compare_loop('SELECT id + $1, v * 2 FROM pipeline_t', mode: mode, params: [nil], types: ['i64'])
      compare_loop('SELECT v + 1 FROM pipeline_t WHERE flag', mode: mode, failure: [1, 1])
      compare_loop('SELECT v + (10 / $1) FROM pipeline_t WHERE id = 2',
        mode: mode, params: [0], types: ['i64'])
      compare_loop('SELECT v + 1 FROM pipeline_t', mode: mode, values: [[1, 3, true], [2, (1 << 63) - 1, true]])
      compare_loop('SELECT id + 1, v + 2 FROM pipeline_t', mode: mode, failure: [1, 1])
      compare_loop('SELECT CASE WHEN flag THEN v + 1 ELSE id - 1 END FROM pipeline_t', mode: mode, failure: [1, 1])
      compare_loop('SELECT id + 1 FROM pipeline_t', mode: mode, input_failure: 2)
      compare_loop('SELECT id + 1 FROM pipeline_t', mode: mode, output_failure: 1)
      compare_loop('SELECT id + 1 FROM pipeline_t', mode: mode, values: [])
    end
    compare_loop('SELECT 10 / v FROM pipeline_t', mode: :stream, limit: 0, input_failure: 0)
    compare_loop('SELECT v + 1 FROM pipeline_t WHERE id > 1', mode: :stream, offset: 1, limit: 1, failure: [1, 1])
    compare_loop('SELECT v + 1 FROM pipeline_t', mode: :stream, limit: 1, input_failure: 1)
    compare_loop('SELECT v + 1 FROM pipeline_t WHERE flag', mode: :stream, offset: 20, failure: [0, 1])
    compare_loop('SELECT v + 1 FROM pipeline_t', mode: :stream, offset: 1)

    predicate, projections, registry = resolve('SELECT id + 1, upper($1) FROM pipeline_t', ['text'])
    assert(JedNative::CompiledPipeline.build(predicate, projections, types: registry,
      mode: :stream, offset: false, limit: false).nil?, 'unsupported complete projection entered a fused loop')
  end

  def setup(database)
    database.execute('CREATE TABLE pipeline_t(id i64 PRIMARY KEY, v i64, flag boolean)')
    database.execute('INSERT INTO pipeline_t VALUES(1,3,TRUE),(2,NULL,NULL),(3,-7,FALSE),(4,0,TRUE)')
  end

  def session(database, disabled: false, immediate: false)
    current = database.session
    current.engine.instance_variable_set(:@native_pipeline_disabled, disabled)
    current.engine.instance_variable_set(:@native_compilation_immediate, immediate)
    current
  end

  def pipeline_kernels(current)
    %i[@native_select_plans @native_read_plans].flat_map do |name|
      (current.engine.instance_variable_get(name) || {}).values.filter_map do |plan|
        plan.instance_variable_get(:@native_compiled_state)&.kernels&.pipeline
      end
    end
  end

  def invoke(current, statement, sql, params, api)
    case api
    when :query then current.query(sql, *params)
    when :execute then current.execute(sql, *params)
    when :query_prepared then current.query_prepared(statement, *params)
    when :execute_prepared then current.execute_prepared(statement, *params)
    else raise "unknown API #{api}"
    end
  end

  def outcome(current, statement, sql, params, api, collect: false)
    prior = current.lifetime_cost
    rows, checkpoints, phase = [], [], :admission
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
    [:rows, rows, result.columns, result.column_types, checkpoints,
      result.cost, current.lifetime_cost - prior]
  rescue JedNative::Error => error
    [:error, phase, error.sqlstate, error.message, rows, checkpoints,
      result&.cost || current.engine.cost, current.lifetime_cost - prior]
  ensure
    result.close if result.is_a?(JedNative::Rows)
  end

  def compare(database, sql, params = [], api: :query_prepared, setting: nil, limit: 0, collect: false, calls: nil)
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
      observed = outcome(current, statement, sql, params, api, collect: collect)
      observed << calls.dup if calls
      observed
    ensure
      current&.close
    end
    assert(outcomes[0] == outcomes[1],
      "pipeline changed #{api} #{sql} #{params.inspect} #{setting}=#{limit}: #{outcomes.inspect}")
    outcomes[1]
  end

  def api_equivalence(database)
    queries = [
      ['SELECT id + $1, v * 2 FROM pipeline_t WHERE id + 1 > $2', [10, 1]],
      ['SELECT id + $1, v * 2 FROM pipeline_t WHERE id + 1 > $2', [nil, 1]],
      ['SELECT id + 1, v / $1 FROM pipeline_t WHERE flag ORDER BY id', [0]],
      ['SELECT id + 1, v / $1 FROM pipeline_t WHERE flag ORDER BY id', [2]],
      ['SELECT id + 1, v + $1 FROM pipeline_t ORDER BY id DESC LIMIT $2 OFFSET $3', [10, 2, 1]],
      ['SELECT id + 1, v + $1 FROM pipeline_t ORDER BY id DESC LIMIT $2 OFFSET $3', [10, nil, nil]],
      ['SELECT v / 0 FROM pipeline_t LIMIT 0', []],
      ['SELECT v / 0 FROM pipeline_t LIMIT 1 OFFSET 20', []],
      ['SELECT 10 / (id - 2) FROM pipeline_t ORDER BY id LIMIT 1', []],
      ['SELECT 10 / (id - 2) FROM pipeline_t ORDER BY id LIMIT 2', []],
      ['SELECT id + $1 FROM pipeline_t WHERE flag AND 10 / v > 0 ORDER BY id', [1]],
      ['SELECT CASE WHEN flag THEN v + 1 ELSE id - 1 END FROM pipeline_t ORDER BY id', []],
      ['SELECT id + $1 FROM pipeline_t ORDER BY id', [(1 << 63) - 1]],
      ['SELECT id + 1 FROM pipeline_t LIMIT $1 OFFSET $2', [-1, 0]],
      ['SELECT id + 1 FROM pipeline_t LIMIT $1 OFFSET $2', [1, -1]],
      ['SELECT id + 1 FROM pipeline_t WHERE id > 20', []]
    ]
    %i[query_prepared execute_prepared query execute].each do |api|
      queries.each { |sql, params| compare(database, sql, params, api: api) }
    end

    %i[query_prepared execute_prepared].each do |api|
      [queries[0], queries[2], queries[4], queries[8], queries[9]].each do |sql, params|
        full = compare(database, sql, params, api: api)
        %i[max_cost lifetime_max_cost].each do |setting|
          (1..full[-2] + 1).each do |limit|
            compare(database, sql, params, api: api, setting: setting, limit: limit)
          end
        end
        [1, 127, 128, 255, 512, 1_048_576].each do |limit|
          compare(database, sql, params, api: api, setting: :max_query_memory_bytes, limit: limit, collect: true)
        end
      end
    end
  end

  def admission_and_reuse(database)
    %i[query_prepared execute_prepared].each do |api|
      current = session(database)
      sql = 'SELECT id + $1, v * 2 FROM pipeline_t WHERE id + 1 > $2'
      statement = current.prepare(sql)
      first = outcome(current, statement, sql, [10, 1], api)
      assert(pipeline_kernels(current).empty?, 'first-use plan generated a fused loop')
      second = outcome(current, statement, sql, [10, 1], api)
      assert(second == first && !pipeline_kernels(current).empty?, "#{api} did not reuse a fused loop")
      kernel_ids = pipeline_kernels(current).map(&:object_id)
      assert(outcome(current, statement, sql, [20, 3], api)[1] == [[23, -14], [24, 0]], 'fused loop retained old parameters')
      assert(pipeline_kernels(current).map(&:object_id) == kernel_ids, 'fused loop recompiled unchanged plan')
      [[[], '42601'], [[1], '42601'], [[1, 2, 3], '42601'], [['wrong', 1], '42804']].each do |params, code|
        result = outcome(current, statement, sql, params, api)
        assert(result[0] == :error && result[2] == code && result[-1].zero?, 'fused loop changed admission binding failure')
      end
      current.close
    end
  end

  def captured_context(database)
    reader, writer = session(database), database.session
    sql = 'SELECT id + $1, v + 1 FROM pipeline_t ORDER BY id'
    statement = reader.prepare(sql)
    3.times { reader.query_prepared(statement, 1).rows }
    assert(!pipeline_kernels(reader).empty?, 'snapshot probe never admitted a fused loop')
    old = reader.query_prepared(statement, 1)
    assert(old.next.to_a == [2, 4], 'first fused cursor row changed')
    pending = reader.query_prepared(statement, 10)
    writer.execute('UPDATE pipeline_t SET v=30 WHERE id=1')
    assert(reader.query_prepared(statement, 3).rows.first == [4, 31], 'fused plan retained an old publication')
    assert(pending.rows.first == [11, 4] && old.next.to_a == [3, nil], 'fused cursor lost its captured binding or publication')
    old.close

    captured = reader.query_prepared(statement, 1)
    reader.revoke(:select, 'pipeline_t')
    denied = outcome(reader, statement, sql, [1], :query_prepared)
    assert(denied[0] == :error && denied[2] == '42501', 'fused loop bypassed refreshed permissions')
    assert(captured.rows.first == [2, 31], 'captured fused cursor lost its admitted permissions')
    reader.reset_privileges

    old = reader.query_prepared(statement, 1)
    writer.execute('DROP TABLE pipeline_t')
    writer.execute('CREATE TABLE pipeline_t(id i16 PRIMARY KEY, v i16, flag boolean)')
    writer.execute('INSERT INTO pipeline_t VALUES(1,7,TRUE)')
    fresh = reader.query_prepared(statement, 1)
    assert(fresh.rows == [[2, 8]] && fresh.column_types == %w[i16 i16], 'fused plan retained replaced schema')
    assert(old.rows.first == [2, 31] && old.column_types == %w[i64 i64], 'fused cursor lost old schema')
    reader.close
    writer.close
  end

  def lifetime_interleaving(database)
    current = session(database)
    sql = 'SELECT id + 1, v * 2 FROM pipeline_t ORDER BY id'
    statement = current.prepare(sql)
    3.times { current.query_prepared(statement).rows }
    full = current.query_prepared(statement)
    full.rows
    prior = current.lifetime_cost
    current.lifetime_max_cost = prior + full.cost + 1
    first, second = current.query_prepared(statement), current.query_prepared(statement)
    first.rows
    begin
      second.next
      raise 'second cursor should exhaust the shared lifetime allowance'
    rescue JedNative::Error => error
      assert(error.sqlstate == '54P02' && current.lifetime_cost >= current.lifetime_max_cost,
        'fused cursor stopped using the live shared lifetime account')
    end
    first.close
    second.close
    current.close
  end

  def fallback_and_bounds
    calls = []
    environment = JedNative::Environment.new.register_function(name: 'pipeline_probe', args: ['i64'], returns: 'i64', cost: 3) do |value|
      calls << value
      raise JedNative::Error.new('22023', 'pipeline host failure') if value == 3
      value + 10
    end
    JedNative.memory(environment: environment) do |database|
      setup(database)
      queries = [
        'SELECT id + 1, pipeline_probe(id) FROM pipeline_t ORDER BY id',
        'SELECT id + 1, ARRAY[id] FROM pipeline_t ORDER BY id',
        'SELECT DISTINCT id + 1 FROM pipeline_t ORDER BY id + 1',
        'SELECT id + 1 FROM pipeline_t ORDER BY v + 1',
        'SELECT id + 1 FROM pipeline_t WHERE id=$1',
        'SELECT id + 1 FROM pipeline_t LIMIT 2'
      ]
      queries.each_with_index do |sql, index|
        params = sql.include?('$1') ? [1] : []
        %i[query_prepared execute_prepared].each do |api|
          compare(database, sql, params, api: api, calls: calls)
          current = session(database)
          statement = current.prepare(sql)
          3.times { outcome(current, statement, sql, params, api) }
          # LIMIT uses the same incremental executor for eager and lazy APIs.
          unless index == queries.length - 1
            assert(pipeline_kernels(current).empty?, "guarded shape acquired a fused loop: #{api} #{sql}")
          end
          current.close
        end
      end

      current = session(database)
      80.times do |index|
        sql = "SELECT id + #{index} FROM pipeline_t WHERE v + 1 > 0"
        statement = current.prepare(sql)
        2.times { current.query_prepared(statement).rows }
      end
      cache = current.engine.instance_variable_get(:@native_read_plans)
      assert(cache.length <= JedNative::PreparedSelect::CACHE_ENTRIES, 'fused plans exceeded cache entry bound')
      assert(cache.values.any? { |plan| plan.instance_variable_get(:@native_compiled_state)&.kernels&.pipeline }, 'cache probe did not compile loops')
      cache.each_value do |plan|
        state = plan.instance_variable_get(:@native_compiled_state)
        next unless state
        source_bytes = state.kernels.to_a.compact.sum(&:source_bytes)
        assert(source_bytes == state.source_bytes && source_bytes <= JedNative::CompiledPlans::MAX_PLAN_SOURCE_BYTES,
          'fused source accounting exceeded the existing per-plan bound')
      end
      current.close
    end
  end

  def run
    direct_schedules
    JedNative.memory do |database|
      setup(database)
      api_equivalence(database)
      admission_and_reuse(database)
      lifetime_interleaving(database)
      captured_context(database)
    end
    Dir.mktmpdir('jed-compiled-pipeline-') do |directory|
      JedNative.create(File.join(directory, 'scan.jed'), locking: :exclusive) do |database|
        setup(database)
        api_equivalence(database)
        admission_and_reuse(database)
        captured_context(database)
      end
    end
    fallback_and_bounds
    puts 'ruby-native compiled pipelines: row/failure checkpoints, warm APIs, bindings, limits, budgets, file scans, snapshots, fallback and cache bounds passed'
  end
end

NativeCompiledPipelineTests.run
