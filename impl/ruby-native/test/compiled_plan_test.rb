# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# Generated code is an internal execution choice. Exercise its reuse through
# host APIs, where bindings, snapshots, permissions and cursor accounts change.
module NativeCompiledPlanTests
  module_function

  def assert(condition, message) = (raise(message) unless condition)

  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => exception
    assert(exception.sqlstate == code, "expected #{code}, got #{exception.sqlstate}: #{exception.message}")
    exception
  end

  def setup(database)
    database.execute('CREATE TABLE compiled_t(id i64 PRIMARY KEY, v i64, label text, flag boolean)')
    database.execute("INSERT INTO compiled_t VALUES (1,3,'one',TRUE),(2,NULL,'two',NULL),(3,-7,'three',FALSE),(4,0,'four',TRUE)")
  end

  def session(database, disabled: false, immediate: false, **options)
    result = database.session(**options)
    result.engine.instance_variable_set(:@native_compilation_disabled, disabled)
    result.engine.instance_variable_set(:@native_compilation_immediate, immediate)
    result
  end

  def invoke(session, statement, sql, params, api)
    case api
    when :query then session.query(sql, *params)
    when :execute then session.execute(sql, *params)
    when :query_prepared then session.query_prepared(statement, *params)
    when :execute_prepared then session.execute_prepared(statement, *params)
    else raise "unknown API #{api}"
    end
  end

  def outcome(session, statement, sql, params, api, collect: true)
    lifetime = session.lifetime_cost
    phase, rows = :admission, []
    result = invoke(session, statement, sql, params, api)
    phase = :pull
    if collect || !result.is_a?(JedNative::Rows)
      rows = result.rows
    else
      result.each { |row| rows << row.to_a }
    end
    [:rows, rows, result.columns, result.column_types, result.cost, session.lifetime_cost - lifetime]
  rescue JedNative::Error => exception
    [:error, phase, exception.sqlstate, exception.message, rows,
      result&.cost || session.engine.cost, session.lifetime_cost - lifetime]
  ensure
    result.close if result.is_a?(JedNative::Rows)
  end

  def states(session)
    %i[@native_select_plans @native_read_plans].flat_map do |name|
      (session.engine.instance_variable_get(name) || {}).values.filter_map do |plan|
        plan.instance_variable_get(:@native_compiled_state)
      end
    end
  end

  def kernels(session)
    states(session).flat_map { |state| state.kernels.to_a.compact }
  end

  def paired(database, sql, params, api, warmed: true, setting: nil, limit: 0, collect: true, calls: nil)
    [true, false].map do |disabled|
      current = session(database, disabled: disabled, immediate: !warmed)
      statement = current.prepare(sql)
      2.times { outcome(current, statement, sql, params, api, collect: collect) } if warmed
      if setting == :lifetime_max_cost
        current.lifetime_max_cost = current.lifetime_cost + limit
      elsif setting
        current.public_send("#{setting}=", limit)
      end
      calls&.clear
      result = outcome(current, statement, sql, params, api, collect: collect)
      result << calls.dup if calls
      result
    ensure
      current&.close
    end
  end

  def compare(database, sql, params = [], api: :query_prepared, **options)
    reference, compiled = paired(database, sql, params, api, **options)
    assert(reference == compiled, "compiled plan changed #{api} #{sql} #{params.inspect} #{options.inspect}: #{compiled.inspect} != #{reference.inspect}")
    compiled
  end

  def admission_and_reuse
    JedNative.memory do |database|
      setup(database)
      current = session(database)
      sql = 'SELECT abs($1::i64) + 7'
      scalar = current.prepare(sql)
      first = outcome(current, scalar, sql, [-9], :query_prepared)
      assert(first.first == :rows && kernels(current).empty?, 'first-use scalar generated code eagerly')
      assert(states(current).any? { |state| state.uses == 1 }, 'first use did not retain the resolved plan')
      second = outcome(current, scalar, sql, [-9], :query_prepared)
      assert(second == first && !kernels(current).empty?, 'reused scalar plan was not compiled')
      generated = kernels(current).map(&:object_id)
      [-7, nil, 0, 999].each do |value|
        result = outcome(current, scalar, sql, [value], :query_prepared)
        expected = value.nil? ? nil : value.abs + 7
        assert(result[1] == [[expected]], 'compiled scalar retained an earlier binding')
      end
      assert(kernels(current).map(&:object_id) == generated, 'reused scalar rebuilt identical generated code')

      scan = 'SELECT v + $1::i64, upper(label) FROM compiled_t WHERE id >= $2 ORDER BY id'
      prepared = current.prepare(scan)
      3.times { outcome(current, prepared, scan, [1, 1], :query_prepared) }
      assert(states(current).any? { |state| state.kernels.predicate && state.kernels.projection },
        'reused scan did not compile its predicate and mixed projection')
      result = outcome(current, prepared, scan, [10, 3], :query_prepared)
      assert(result[1] == [[3, 'THREE'], [10, 'FOUR']], 'compiled scan retained bindings or failed mixed fallback')
      current.close
    end
  end

  def api_equivalence
    JedNative.memory do |database|
      setup(database)
      cases = [
        ['SELECT abs($1::i64) + 7, upper($2::text)', [-9, 'ruby']],
        ['SELECT abs($1::i64) + 7, upper($2::text)', [nil, nil]],
        ['SELECT $1::boolean AND $2::boolean', [nil, false]],
        ['SELECT $1::boolean OR $2::boolean', [nil, true]],
        ['SELECT 1 / $1::i64', [0]],
        ['SELECT $1::i64 + 1', [(1 << 63) - 1]],
        ['SELECT id + $1, v * 2, label FROM compiled_t WHERE id >= $2 ORDER BY id', [3, 2]],
        ['SELECT v / $1 FROM compiled_t WHERE flag AND id > 0 ORDER BY id', [0]],
        ['SELECT v + $1 FROM compiled_t WHERE id >= $2 ORDER BY v + $3, id LIMIT 2', [1, 1, 7]],
        ['SELECT v + 1 FROM compiled_t ORDER BY upper(label), id + 1 LIMIT 2', []],
        ['SELECT CASE WHEN flag THEN v + 1 ELSE v - 1 END FROM compiled_t ORDER BY id', []],
        ['SELECT id + 1, ARRAY[id] FROM compiled_t ORDER BY id', []],
        ['SELECT count(*), sum(v) FROM compiled_t WHERE id > $1', [1]],
        ['SELECT id + 1, row_number() OVER (ORDER BY id) FROM compiled_t ORDER BY id', []]
      ]
      %i[query_prepared execute_prepared query execute].each do |api|
        cases.each { |sql, params| compare(database, sql, params, api: api) }
      end
      # Malformed bindings remain admission failures on already-warmed trees.
      %i[query_prepared execute_prepared query execute].each do |api|
        current = session(database)
        sql = 'SELECT $1::i16 + 1'
        statement = current.prepare(sql)
        3.times { outcome(current, statement, sql, [7], api) }
        [[[], '42601'], [[1, 2], '42601'], [['1'], '42804'], [[32_768], '22003']].each do |params, code|
          result = outcome(current, statement, sql, params, api)
          assert(result[0] == :error && result[2] == code && result[-1].zero?, 'compiled plan changed a binding failure')
        end
        current.close
      end
    end
  end

  def budgets_and_memory
    JedNative.memory do |database|
      setup(database)
      queries = [
        ['SELECT abs($1::i64) + 7', [-9]],
        ['SELECT id + 1, v / $1 FROM compiled_t WHERE id > 0 ORDER BY id', [2]],
        ['SELECT id + 1, v / $1 FROM compiled_t WHERE id > 0 ORDER BY id', [0]],
        ['SELECT id + 1, label FROM compiled_t ORDER BY abs(v) + 1, id LIMIT 2', []],
        ['SELECT id + 1, upper(label) FROM compiled_t ORDER BY id', []]
      ]
      %i[query_prepared execute_prepared].each do |api|
        queries.each do |sql, params|
          result = compare(database, sql, params, api: api)
          total = result[-2]
          %i[max_cost lifetime_max_cost].each do |setting|
            (1..total + 1).each do |limit|
              compare(database, sql, params, api: api, setting: setting, limit: limit)
            end
          end
          [1, 63, 64, 127, 128, 255, 256, 512, 1_048_576].each do |limit|
            compare(database, sql, params, api: api, setting: :max_query_memory_bytes, limit: limit)
          end
          compare(database, sql, params, api: api, collect: false)
        end
      end
      [1, 7, 8, 16].each do |limit|
        compare(database, 'SELECT id + 1, repeat(label, 2) FROM compiled_t ORDER BY id',
          setting: :max_scalar_bytes, limit: limit)
      end
    end
  end

  def captured_context
    JedNative.memory do |database|
      setup(database)
      reader, writer = session(database), database.session
      sql = 'SELECT id + $1, v, upper(label) FROM compiled_t ORDER BY id'
      statement = reader.prepare(sql)
      3.times { reader.query_prepared(statement, 1).rows }
      assert(!kernels(reader).empty?, 'snapshot probe never warmed compiled code')
      old = reader.query_prepared(statement, 1)
      other = reader.query_prepared(statement, 10)
      reader.revoke(:select, 'compiled_t')
      error('42501') { reader.query_prepared(statement, 1) }
      reader.reset_privileges
      writer.execute("UPDATE compiled_t SET v=30,label='changed' WHERE id=1")
      fresh = reader.query_prepared(statement, 3).rows
      assert(fresh.first == [4, 30, 'CHANGED'], 'compiled plan retained an old publication')
      assert(other.rows.first == [11, 3, 'ONE'] && old.rows.first == [2, 3, 'ONE'],
        'compiled cursors lost captured parameters, permissions, or publication')

      old = reader.query_prepared(statement, 1)
      writer.execute('DROP TABLE compiled_t')
      writer.execute('CREATE TABLE compiled_t(id i16 PRIMARY KEY, v text, label text)')
      writer.execute("INSERT INTO compiled_t VALUES(1,'replacement','new')")
      replacement = reader.query_prepared(statement, 1)
      assert(replacement.rows == [[2, 'replacement', 'NEW']] && replacement.column_types[1] == 'text',
        'compiled plan retained dropped table metadata')
      assert(old.rows.first == [2, 30, 'CHANGED'] && old.column_types[1] == 'i64',
        'captured compiled cursor lost its old schema')
      scalar = reader.prepare('SELECT $1::i64 + 1, upper($2::text)')
      3.times { reader.query_prepared(scalar, 1, 'old').rows }
      input = +'captured'
      pending = reader.query_prepared(scalar, 7, input)
      input.replace('mutated')
      reader.max_cost = 1
      assert(pending.rows == [[8, 'CAPTURED']], 'compiled cursor retained host input or observed a later budget')
      reader.max_cost = 0
      row = reader.query_prepared(scalar, 7, 'captured').rows.first
      row[1].replace('mutated')
      assert(reader.query_prepared(scalar, 7, 'captured').rows == [[8, 'CAPTURED']], 'compiled result aliases reused data')
      reader.close
      writer.close
    end
  end

  def conditional_plan_reuse
    JedNative.memory do |database|
      setup(database)
      current = session(database)
      sql = 'SELECT CASE WHEN flag THEN v + $1 ELSE v - 1 END FROM compiled_t WHERE id BETWEEN 1 AND $2 ORDER BY id'
      statement = current.prepare(sql)
      3.times { current.query_prepared(statement, 2, 4).rows }
      cache = current.engine.instance_variable_get(:@native_read_plans)
      plan = cache.values.last
      kernels = plan&.instance_variable_get(:@native_compiled_state)&.kernels
      assert(kernels && (kernels.projection || kernels.pipeline),
        'reused table CASE did not retain compiled expressions')
      assert(current.query_prepared(statement, 10, 3).rows == [[13], [nil], [-8]], 'conditional plan retained old bindings')
      assert(cache.values.last.equal?(plan), 'safe CASE/BETWEEN rebuilt the same read plan')

      # Context checks still happen before scanning, even for an unselected arm.
      current.execute('CREATE TABLE narrow_case(id i16 PRIMARY KEY, v i16)')
      current.execute('INSERT INTO narrow_case VALUES(1,7)')
      narrow = current.prepare('SELECT CASE WHEN FALSE THEN v + $1 ELSE v END FROM narrow_case ORDER BY id')
      3.times { assert(current.query_prepared(narrow, 1).rows == [[7]], 'narrow CASE warmup failed') }
      error('22003') { current.query_prepared(narrow, 32_768).rows }
      assert(current.query_prepared(narrow, nil).rows == [[7]], 'failed contextual binding poisoned compiled CASE')

      current.set_var('app.compiled', 'changed')
      ["SELECT id + 1, CASE WHEN TRUE THEN label ELSE current_setting('app.compiled') END FROM compiled_t ORDER BY id",
       'SELECT id + 1, CASE WHEN TRUE THEN v ELSE (SELECT 7::i64) END FROM compiled_t ORDER BY id'].each do |unsafe|
        prepared = current.prepare(unsafe)
        before = (current.engine.instance_variable_get(:@native_read_plans) || {}).values.map(&:object_id)
        3.times { current.query_prepared(prepared).rows }
        after = (current.engine.instance_variable_get(:@native_read_plans) || {}).values.map(&:object_id)
        assert(after == before, 'dead stateful or subquery branch became a reusable read plan')
      end
      current.close
    end
  end

  def lifetime_interleaving
    JedNative.memory do |database|
      current = session(database)
      statement = current.prepare('SELECT abs($1::i64) + 7')
      3.times { current.query_prepared(statement, 1).rows }
      cost = current.query_prepared(statement, 1).tap(&:rows).cost
      start = current.lifetime_cost
      current.lifetime_max_cost = start + cost + 1
      first = current.query_prepared(statement, 1)
      second = current.query_prepared(statement, 2)
      assert(first.rows == [[8]], 'first compiled cursor did not fit lifetime allowance')
      error('54P02') { second.next }
      assert(second.cost == 1 && current.lifetime_cost == start + cost + 1,
        'compiled cursor overwrote the live lifetime account')
      current.close
    end
  end

  def host_and_error_order
    calls = []
    environment = JedNative::Environment.new.register_function(name: 'compiled_probe', args: ['i64'], returns: 'i64', cost: 3) do |value|
      calls << value
      raise JedNative::Error.new('22023', 'compiled callback failure') if value == 99
      value + 10
    end
    JedNative.memory(environment: environment) do |database|
      setup(database)
      sql = 'SELECT id + 1, compiled_probe(id + $1), v + 2 FROM compiled_t WHERE id >= $2 ORDER BY id'
      [0, 96, 98].each do |offset|
        compare(database, sql, [offset, 1], calls: calls)
        (1..25).each { |limit| compare(database, sql, [offset, 1], calls: calls, setting: :max_cost, limit: limit) }
      end
      current = session(database)
      statement = current.prepare(sql)
      3.times { current.query_prepared(statement, 0, 1).rows }
      current.revoke(:execute, 'compiled_probe')
      calls.clear
      error('42501') { current.query_prepared(statement, 0, 1) }
      assert(calls.empty?, 'compiled fallback invoked a revoked host function')
      current.close

      # Sort expressions run before projections, including mixed compiled and
      # interpreted roots. A failed key must not execute a projection callback.
      sort = 'SELECT id + 1, compiled_probe(99) FROM compiled_t ORDER BY id + 1, 10 / v LIMIT 1'
      result = compare(database, sort, calls: calls)
      assert(result[0] == :error && result[2] == '22012' && result.last.empty?, 'sort-key failure reached a projection callback')
      sort = 'SELECT id + 1, compiled_probe(id) FROM compiled_t ORDER BY id + 1, upper(label) LIMIT 2'
      compare(database, sort, calls: calls)
      compare(database, 'SELECT id + 1, compiled_probe(id) FROM compiled_t WHERE id > 1 ORDER BY id LIMIT 1', calls: calls)
    end
  end

  def cache_bounds
    JedNative.memory do |database|
      current = session(database)
      80.times do |index|
        statement = current.prepare("SELECT $1::i64 + #{index}")
        3.times { current.query_prepared(statement, 7).rows }
      end
      cache = current.engine.instance_variable_get(:@native_select_plans)
      assert(cache.length <= JedNative::PreparedSelect::CACHE_ENTRIES, 'compiled plan cache exceeded existing entry bound')
      states(current).each do |state|
        source_bytes = state.kernels.to_a.compact.sum(&:source_bytes)
        assert(source_bytes == state.source_bytes && source_bytes <= JedNative::CompiledPlans::MAX_PLAN_SOURCE_BYTES,
          'compiled source accounting exceeded per-plan bound')
      end
      assert(states(current).all? { |state| state.uses <= 2 }, 'reuse counters grow without bound')
      current.close
    end
  end

  def aggregate_arguments(database)
    setup(database)
    queries = [
      ['SELECT sum(v + $1), count(v / $2) FROM compiled_t', [2, 2]],
      ['SELECT sum(v + $1), count(v / $2) FROM compiled_t', [nil, 0]],
      ['SELECT sum(10 / v) FILTER (WHERE v <> 0), count(id + 1) FROM compiled_t', []],
      ['SELECT flag, sum(v + 1), array_agg(id + 1 ORDER BY id DESC) FROM compiled_t GROUP BY flag ORDER BY flag', []],
      ['SELECT flag, sum(v + 1) FROM compiled_t GROUP BY ROLLUP(flag) ORDER BY flag, sum(v + 1)', []],
      ['SELECT sum(v + 1), count(id + 1) FROM compiled_t WHERE id > 20', []]
    ]
    queries.each do |sql, params|
      %i[query_prepared execute_prepared].each do |api|
        result = compare(database, sql, params, api: api)
        total = result[-2]
        %i[max_cost lifetime_max_cost].each do |setting|
          (1..total + 1).each { |limit| compare(database, sql, params, api: api, setting: setting, limit: limit) }
        end
        [128, 512, 1_048_576].each do |limit|
          compare(database, sql, params, api: api, setting: :max_query_memory_bytes, limit: limit)
        end
      end
    end
    current = session(database)
    sql = 'SELECT sum(v + $1) FROM compiled_t'
    statement = current.prepare(sql)
    3.times { current.query_prepared(statement, 1).rows }
    cache = current.engine.instance_variable_get(:@native_compiled_arguments)
    assert(cache && cache.values.any?, 'aggregate arguments did not reuse generated code')
    assert(!cache.compare_by_identity? && cache.length == 1,
      'fresh lazy aggregate calls did not share one value-keyed argument program')
    assert(current.query_prepared(statement, 10).rows.first.first == JedNative::Decimal.parse('26'),
      'aggregate argument code retained old parameters')
    assert(current.query('SELECT sum(id + 1) FROM compiled_t').rows.first.first == JedNative::Decimal.parse('14'),
      'aggregate code cache reused another column ordinal')

    current.execute('CREATE TABLE compiled_offset(delta i64)')
    current.execute('INSERT INTO compiled_offset VALUES(1)')
    folded = current.prepare('SELECT sum(v + (SELECT delta FROM compiled_offset)) FROM compiled_t')
    3.times { assert(current.query_prepared(folded).rows.first.first == JedNative::Decimal.parse('-1'), 'folded aggregate warmup failed') }
    current.execute('UPDATE compiled_offset SET delta=10')
    assert(current.query_prepared(folded).rows.first.first == JedNative::Decimal.parse('26'),
      'aggregate code cache reused a stale folded literal pool')
    70.times do |index|
      changing = current.prepare("SELECT sum(v + #{index}) FROM compiled_t")
      2.times { current.query_prepared(changing).rows }
    end
    cache = current.engine.instance_variable_get(:@native_compiled_arguments)
    assert(cache.length <= JedNative::CompiledPlans::MAX_ARGUMENT_CACHE_ENTRIES, 'aggregate argument cache exceeded its entry bound')
    assert(cache.keys.all? { |source, pool| source.frozen? && pool.frozen? }, 'aggregate code cache has mutable keys')
    current.close
  end

  def aggregate_hosts
    JedNative.memory { |database| aggregate_arguments(database) }
    Dir.mktmpdir('jed-compiled-plan-') do |directory|
      JedNative.create(File.join(directory, 'aggregate.jed'), locking: :exclusive, work_mem: 128) do |database|
        aggregate_arguments(database)
      end
      assert(Dir.children(directory).none? { |name| name.include?('spill') }, 'compiled aggregate retained a spill artifact')
    end
  end

  def run
    admission_and_reuse
    api_equivalence
    budgets_and_memory
    captured_context
    conditional_plan_reuse
    lifetime_interleaving
    host_and_error_order
    cache_bounds
    aggregate_hosts
    puts 'ruby-native compiled plans: warm APIs, bindings, budgets, memory, snapshots, privileges, callbacks, sort ordering and cache bounds passed'
  end
end

NativeCompiledPlanTests.run
