# frozen_string_literal: true

require_relative '../lib/jed_native'

# Eager Result ownership and publication/lifecycle effects are host contracts.
# Compare the optimized entry directly with the original eager executor.
module NativeEagerCursorTests
  module_function

  def assert(condition, message) = (raise(message) unless condition)

  def normalize(value)
    case value
    when Array then value.map { |item| normalize(item) }
    when Float then value.nan? ? :nan : [:float, [value].pack('G')]
    when JedNative::Decimal then [:decimal, value.coefficient, value.scale]
    else value
    end
  end

  def execute(session, statement, params, original:)
    if original
      JedNative::SessionCursors::EAGER_QUERY.bind_call(session, statement, *params)
    else
      session.execute_prepared(statement, *params)
    end
  end

  def outcome(database, sql, params, calls, original:, warmed: false, setting: nil, limit: 0, revoke: nil)
    session = database.session
    statement = session.prepare(sql)
    if warmed
      # Mark first admission without running SQL or invoking a host callback.
      # The measured invocation then exercises the repeat-use cursor route.
      assert(session.send(:eager_cursor, statement, params).nil?, 'first admission built an eager cursor')
    end
    session.execute('SELECT 1 + 2')
    lifetime = session.lifetime_cost
    session.public_send("#{setting}=", limit) if setting
    session.revoke(:execute, revoke) if revoke
    calls.clear
    result = execute(session, statement, params, original: original)
    assert(result.instance_of?(JedNative::Result), 'eager execution returned a cursor')
    assert(result.rows.all? { |row| row.instance_of?(Array) }, 'eager rows changed host class')
    assert(result.cost == session.engine.cost, 'eager result and session meter disagree')
    [:rows, normalize(result.rows), result.columns, result.column_types, result.rows_affected,
      result.cost, session.lifetime_cost - lifetime, calls.dup]
  rescue JedNative::Error => exception
    [:error, exception.sqlstate, exception.message, session.engine.cost,
      session.lifetime_cost - lifetime, calls.dup]
  ensure
    if session
      assert((session.instance_variable_get(:@cursors) || {}).empty?, 'eager execution leaked a cursor')
      assert(!database.instance_variable_get(:@pins).key?(session.object_id), 'eager execution leaked its temporary session pin')
      session.close
      assert(database.instance_variable_get(:@pins).empty?, 'eager execution leaked a cursor pin')
    end
  end

  def compare(database, sql, params, calls, **options)
    [false, true].each do |warmed|
      optimized = outcome(database, sql, params, calls, original: false, warmed: warmed, **options)
      original = outcome(database, sql, params, calls, original: true, warmed: warmed, **options)
      assert(optimized == original, "eager cursor changed warmed=#{warmed} #{options.inspect} #{sql}: #{optimized.inspect} != #{original.inspect}")
    end
  end

  def repeat_admission(database)
    session = database.session
    [
      ['SELECT abs($1::i64)', [-7], JedNative::ScalarRows],
      ['SELECT value FROM eager_points WHERE id=$1', [1], JedNative::PointRows]
    ].each do |sql, params, kind|
      statement = session.prepare(sql)
      lifetime = session.lifetime_cost
      assert(session.send(:eager_cursor, statement, params).nil?, 'fresh eager statement built cursor metadata')
      assert(session.lifetime_cost == lifetime, 'first admission performed SQL work')
      cursor = session.send(:eager_cursor, statement, params)
      assert(cursor.is_a?(kind), 'repeat admission failed to use the intended fast cursor')
      assert(cursor.cost.zero?, 'repeat admission evaluated SQL before drain')
      cursor.close
    end
    80.times do |index|
      assert(session.send(:eager_cursor, session.prepare("SELECT #{index}"), []).nil?, 'unique SQL missed the first-use eager route')
    end
    seen = session.instance_variable_get(:@native_eager_cursor_seen)
    assert(seen.length <= 64, 'eager admission history grew without a bound')
    session.close
    assert(seen.empty?, 'closed session retained its eager admission history')
  end

  def equivalence(database, calls)
    queries = [
      ['SELECT 1', []],
      ['SELECT abs($1::i64) + 7 AS value', [-9]],
      ['SELECT abs($1::i64) + 7 AS value', [nil]],
      ['SELECT $1::decimal + 1.25::decimal', [JedNative::Decimal.parse('2.50')]],
      ['SELECT $1::f64 + 0.0::f64', [Float::NAN]],
      ['SELECT upper($1::text), $1::text AS value, $1::text AS value', ['Ruby']],
      ['SELECT 1 / $1::i64', [0]],
      ["SELECT 'invalid'::i64", []],
      ['SELECT 1 / $1::i64 WHERE FALSE', [0]],
      ['SELECT 1 / $1::i64 WHERE $2::boolean', [0, true]],
      ['SELECT CASE WHEN $1::boolean THEN 1 / 0 ELSE 7 END', [false]],
      ['SELECT CASE WHEN $1::boolean THEN 1 / 0 ELSE 7 END', [true]],
      ['SELECT $1::i16', [32_768]],
      ['SELECT $1::i16', ['1']],
      ['SELECT $1::i16', []],
      ['SELECT $1::i16', [1, 2]],
      ['SELECT repeat($1::text, 4)', ['ab']],
      ['SELECT eager_probe($1)', [7]],
      ['SELECT eager_probe($1)', [nil]],
      ['SELECT eager_probe($1)', [99]],
      ['SELECT eager_probe(1), eager_probe($1), eager_probe(2)', [99]],
      ['SELECT coalesce(1::i64, eager_probe($1))', [99]],
      ['SELECT id, value FROM eager_points WHERE id=$1', [1]],
      ['SELECT id, value FROM eager_points WHERE id=$1', [9]],
      ['SELECT id, value FROM eager_points WHERE id=$1', [nil]],
      ['SELECT id, value FROM eager_points WHERE id=$1', ['1']],
      ['SELECT id, value FROM eager_points WHERE id=$1', []],
      # Ordinary eager fallback remains responsible for these query shapes.
      ['SELECT count(*) FROM eager_points', []],
      ['SELECT value FROM eager_points ORDER BY id', []],
      ['SELECT CASE WHEN TRUE THEN 1 ELSE 2 END WHERE TRUE', []],
      ['SELECT (SELECT 1)', []],
      ['SELECT ARRAY[1,2]', []],
      ['SELECT 1 LIMIT 1', []]
    ]
    queries.each do |sql, params|
      compare(database, sql, params, calls)
      %i[max_cost lifetime_max_cost].each do |setting|
        (1..12).each { |limit| compare(database, sql, params, calls, setting: setting, limit: limit) }
      end
      compare(database, sql, params, calls, setting: :max_query_memory_bytes, limit: 1_048_576)
    end
    [1, 7, 8, 100].each do |limit|
      compare(database, 'SELECT repeat($1::text, 4)', ['ab'], calls, setting: :max_scalar_bytes, limit: limit)
    end
    compare(database, 'SELECT eager_probe($1)', [1], calls, revoke: 'eager_probe')
    compare(database, 'SELECT eager_probe($1)', ['wrong'], calls, revoke: 'eager_probe')
  end

  def ownership_and_lifecycle(database)
    session = database.session
    cursor = session.query('SELECT value FROM eager_points WHERE id=$1', 1)
    before = database.instance_variable_get(:@pins).keys.sort
    result = session.execute('SELECT id, value FROM eager_points WHERE id=$1', 1)
    assert(database.instance_variable_get(:@pins).keys.sort == before, 'eager execution changed an outstanding cursor pin')
    result.columns.first.replace('changed')
    result.column_types.first.replace('changed')
    result.rows.first[1].replace('changed')
    result.rows.first[0] = 99
    assert(cursor.rows == [['original']], 'eager result mutated an older cursor or stored row')
    fresh = session.execute('SELECT id, value FROM eager_points WHERE id=$1', 1)
    assert(fresh.columns == %w[id value] && fresh.column_types == %w[i64 text] && fresh.rows == [[1, 'original']], 'eager result changed cached metadata or values')
    assert(fresh.first['value'] == 'original', 'eager row lookup changed')

    session.begin
    block_cursor = session.query('SELECT id FROM eager_points WHERE id=$1', 1)
    session.execute('UPDATE eager_points SET value=\'pending\' WHERE id=1')
    assert(session.execute('SELECT value FROM eager_points WHERE id=$1', 1).rows == [['pending']], 'eager fast path bypassed transaction writes')
    session.rollback
    assert(block_cursor.closed?, 'eager rollback failed to close a transaction cursor')
    assert(session.execute('SELECT value FROM eager_points WHERE id=$1', 1).rows == [['original']], 'eager rollback left stale results')
    session.close

    reader = database.read_session
    2.times do
      assert(reader.execute('SELECT value FROM eager_points WHERE id=$1', 1).rows == [['original']], 'pinned read-only eager query failed')
    end
    assert(database.instance_variable_get(:@pins).key?(reader.object_id), 'eager query released its persistent read-session pin')
    reader.close
    assert(database.instance_variable_get(:@pins).empty?, 'closed read session retained eager pins')
    assert(database.execute('SELECT eager_probe(2)').rows == [[3]], 'database eager convenience helper failed')
  end

  def live_callback_state
    session, calls, setting = nil, [], nil
    environment = JedNative::Environment.new.register_function(name: 'change_eager_budget', args: ['i64'], returns: 'i64', cost: 3) do |value|
      calls << [session.engine.cost, session.lifetime_cost]
      session.public_send("#{setting}=", value) if setting
      value
    end
    JedNative.memory(environment: environment) do |database|
      [nil, :max_cost, :lifetime_max_cost, :max_scalar_bytes].each do |configured|
        results = [false, true].map do |original|
          session = database.session
          session.execute('SELECT 1 + 2')
          lifetime = session.lifetime_cost
          calls.clear
          setting = configured
          statement = session.prepare("SELECT change_eager_budget(1), repeat('abc', 4)")
          assert(session.send(:eager_cursor, statement, []).nil?, 'fresh host statement entered eager cursor execution')
          begin
            result = execute(session, statement, [], original: original)
            [:rows, result.rows, result.cost, session.lifetime_cost - lifetime, calls.dup]
          rescue JedNative::Error => exception
            [:error, exception.sqlstate, session.engine.cost, session.lifetime_cost - lifetime, calls.dup]
          ensure
            session.close
          end
        end
        assert(results[0] == results[1], "eager callback changed live #{configured || :meter} semantics: #{results.inspect}")
        assert(results.first.last.length == 1, 'eager callback was skipped or repeated')
      end
      # The guard must find callbacks inside otherwise eligible scalar syntax.
      setting = :max_cost
      expressions = [
        'change_eager_budget(1)::i64', '-change_eager_budget(1)',
        'change_eager_budget(1) + 1', 'coalesce(NULL::i64, change_eager_budget(1))',
        'CASE WHEN TRUE THEN change_eager_budget(1) ELSE 2 END',
        '1 BETWEEN 0 AND change_eager_budget(1)'
      ]
      expressions.each do |expression|
        session = database.session
        statement = session.prepare("SELECT #{expression}")
        2.times do
          assert(session.send(:eager_cursor, statement, []).nil?, 'nested host call entered eager cursor execution')
        end
        session.close
      end
    end
  end

  def run
    calls = []
    environment = JedNative::Environment.new.register_function(name: 'eager_probe', args: ['i64'], returns: 'i64', cost: 3) do |value|
      calls << value
      raise JedNative::Error.new('22023', 'eager probe failed') if value == 99
      value + 1
    end
    JedNative.memory(environment: environment) do |database|
      database.execute_script("CREATE TABLE eager_points(id i64 PRIMARY KEY,value text); INSERT INTO eager_points VALUES(1,'original'),(2,'second')")
      equivalence(database, calls)
      repeat_admission(database)
      ownership_and_lifecycle(database)
    end
    live_callback_state
    puts 'ruby-native eager cursors: original-executor values, metadata, costs, errors, budgets, callbacks, ownership and pins passed'
  end
end

NativeEagerCursorTests.run
