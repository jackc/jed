# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# SQL semantics live in the shared corpus. These checks cover the scalar
# cursor's host lifecycle, ownership, and accounting against ordinary Rows.
module NativeScalarCursorTests
  module_function

  def assert(condition, message) = (raise(message) unless condition)

  def error(code)
    yield
    raise "expected SQLSTATE #{code}"
  rescue JedNative::Error => exception
    assert(exception.sqlstate == code, "expected #{code}, got #{exception.sqlstate}: #{exception.message}")
    exception
  end

  def normalized(value)
    case value
    when Array then value.map { |item| normalized(item) }
    when Float then value.nan? ? :nan : [:float, [value].pack('G')]
    when JedNative::Decimal then [:decimal, value.coefficient, value.scale]
    else value
    end
  end

  # A finite, ample query-memory allowance selects ordinary Rows without
  # changing SQL work or its cost. Restore the session after the cursor captures
  # its settings, so both lanes exercise the public cursor-construction API.
  def cursor_for(session, statement, params, ordinary: false)
    previous = session.max_query_memory_bytes
    session.max_query_memory_bytes = 1_048_576 if ordinary
    session.query_prepared(statement, *params)
  ensure
    session.max_query_memory_bytes = previous
  end

  def outcome(session, statement, params = [], ordinary: false)
    lifetime = session.lifetime_cost
    phase = :construction
    cursor = cursor_for(session, statement, params, ordinary: ordinary)
    assert(cursor.is_a?(JedNative::ScalarRows) != ordinary, 'comparison used the wrong scalar cursor lane')
    assert(cursor.cost.zero? && session.lifetime_cost == lifetime, 'construction evaluated scalar SQL')
    metadata = [cursor.columns.map(&:dup), cursor.column_types.map(&:dup)]
    phase = :pull
    rows = cursor.rows
    assert(cursor.closed?, 'drained scalar cursor stayed open')
    assert(session.lifetime_cost - lifetime == cursor.cost, 'scalar drain lost work from the lifetime account')
    [:rows, normalized(rows), cursor.columns, cursor.column_types, cursor.cost,
      session.lifetime_cost - lifetime, metadata]
  rescue JedNative::Error => exception
    assert(cursor.nil? || cursor.closed?, 'failed scalar cursor stayed open')
    assert(!cursor || session.lifetime_cost - lifetime == cursor.cost, 'scalar failure lost work from the lifetime account')
    [:error, phase, exception.sqlstate, exception.message, cursor&.cost || 0,
      session.lifetime_cost - lifetime]
  ensure
    cursor&.close
  end

  def compare(session, sql, params = [], state: nil)
    statement = session.prepare(sql)
    fast = outcome(session, statement, params)
    ordinary = outcome(session, statement, params, ordinary: true)
    assert(fast == ordinary, "scalar cursor differs for #{sql}: #{fast.inspect} != #{ordinary.inspect}")
    if state
      assert(fast[0, 3] == [:error, :pull, state], "#{sql} did not fail lazily with #{state}: #{fast.inspect}")
    else
      assert(fast.first == :rows, "#{sql} failed: #{fast.inspect}")
    end
    fast
  end

  def equivalence(session)
    [-999, 0, nil].each { |value| compare(session, 'SELECT abs($1::i64) + 7 AS value', [value]) }
    compare(session, 'SELECT $1::i16 + $2::i16 AS value', [-32768, 1])
    compare(session, 'SELECT +$1::i16, -$1::i16, abs($1::i16), $1::i16::i64', [nil])
    compare(session, 'SELECT +$1::i16, -$1::i16, abs($1::i16), $1::i16::i64', [-32767])
    compare(session, 'SELECT $1::i64::i16', [-32768])
    compare(session, 'SELECT $1::i64 / 2, $1::i64 % 2', [-7])
    compare(session, 'SELECT $1::decimal + 1.250::decimal', [JedNative::Decimal.parse('2.50')])
    [-0.0, Float::NAN, Float::INFINITY, nil].each do |value|
      compare(session, 'SELECT $1::f64 AS value, $1::f64 = $1::f64 AS same', [value])
    end
    [true, false, nil].each do |value|
      compare(session, 'SELECT $1::boolean AND NULL::boolean AS value', [value])
      compare(session, 'SELECT 1 + $1::i64 AS value WHERE $2::boolean', [3, value])
    end
    compare(session, 'SELECT coalesce($1::i64, 4), nullif($1::i64, 4), greatest($1::i64, 4), least($1::i64, 4)', [nil])
    compare(session, 'SELECT coalesce(7::i64, 1 / $1::i64)', [0])
    %w[nullif greatest least].each do |name|
      compare(session, "SELECT #{name}(NULL::i64, 1 / $1::i64)", [0], state: '22012')
    end
    compare(session, 'SELECT 1 / 0 WHERE FALSE')
    compare(session, 'SELECT 1 / 0 WHERE NULL::boolean')
    compare(session, 'SELECT 1 / 0 WHERE 1 = 2')
    compare(session, 'SELECT 1 / 0 WHERE $1::i64 = 2', [1])
    compare(session, 'SELECT 1 / $1::i64', [0], state: '22012')
    compare(session, 'SELECT abs($1::i64)', [-(2**63)], state: '22003')
    compare(session, 'SELECT abs($1::i16)', [-32768], state: '22003')
    compare(session, 'SELECT -$1::i64', [-(2**63)], state: '22003')
    compare(session, 'SELECT -$1::i16', [-32768], state: '22003')
    compare(session, 'SELECT $1::i64::i16', [32768], state: '22003')
    compare(session, 'SELECT $1::i64 + 1', [2**63 - 1], state: '22003')
    compare(session, "SELECT 'invalid'::i64", state: '22P02')
    compare(session, "SELECT 'invalid'::i64 WHERE FALSE", state: '22P02')

    statement = session.prepare('SELECT abs($1::i16)')
    [false, true].each do |ordinary|
      [[[], '42601'], [[1, 2], '42601'], [['1'], '42804'], [[32768], '22003']].each do |params, state|
        result = outcome(session, statement, params, ordinary: ordinary)
        assert(result[0, 3] == [:error, :construction, state] && result[4, 2] == [0, 0], 'binding error was deferred or accrued work')
      end
    end
  end

  def conditional_and_text(session)
    [true, false, nil].each do |condition|
      compare(session, 'SELECT CASE WHEN $1::boolean THEN $2::i64 + 1 ELSE 7 END', [condition, 4])
      compare(session, 'SELECT CASE $1::boolean WHEN TRUE THEN 1 WHEN FALSE THEN 2 ELSE 3 END', [condition])
    end
    compare(session, 'SELECT CASE WHEN FALSE THEN 1 / 0 ELSE 7 END')
    compare(session, 'SELECT CASE WHEN TRUE THEN 1 / $1::i64 ELSE 7 END', [0], state: '22012')
    compare(session, "SELECT CASE WHEN FALSE THEN 'invalid'::i64 ELSE 7 END", state: '22P02')
    compare(session, 'SELECT 1 BETWEEN 0 AND 1 / $1::i64', [0], state: '22012')
    [0, 2, nil].each do |value|
      compare(session, 'SELECT $1::i64 BETWEEN 1 AND 3, $1::i64 NOT BETWEEN 1 AND 3', [value])
    end
    statement = session.prepare('SELECT CASE WHEN $1::boolean THEN CASE WHEN $2::boolean THEN $3 ELSE 2::i16 END ELSE 3::i16 END')
    [[true, true, -32768], [true, false, 32767], [false, true, nil], [nil, nil, 7], [true, true, 12]].each do |params|
      fast = outcome(session, statement, params)
      assert(fast.first == :rows && fast == outcome(session, statement, params, ordinary: true), 'cached nested CASE lost contextual parameters')
    end
    [false, true].each do |ordinary|
      result = outcome(session, statement, [true, true, 32768], ordinary: ordinary)
      assert(result[0, 3] == [:error, :construction, '22003'], 'nested CASE deferred a contextual binding error')
    end

    ['Ruby', "Straße Ω", '', nil].each do |text|
      compare(session, 'SELECT $1::text, lower($1::text), upper($1::text), length($1::text)', [text])
      compare(session, 'SELECT $1::text || $2::text', [text, '!'])
    end
    compare(session, 'SELECT $1::i64 + 1, upper($2::text), $1::i64::text', [42, 'ruby'])
    compare(session, 'SELECT $1::text, 1 / $2::i64', ['first', 0], state: '22012')
    compare(session, 'SELECT repeat($1::text, 3), lpad($1::text, 5, \'x\'), replace($1::text, \'a\', \'z\')', ['ab'])
    statement = session.prepare('SELECT $1::text AS value, upper($1::text) AS uppercase')
    input = +'original'
    cursor = session.query_prepared(statement, input)
    input.replace('changed')
    row = cursor.next
    assert(row == ['original', 'ORIGINAL'] && row['value'] == 'original', 'scalar text cursor retained mutable host input')
    row[0].replace('mutated')
    row[1].replace('mutated')
    cursor.close
    assert(outcome(session, statement, ['original'])[1] == [['original', 'ORIGINAL']], 'returned text mutated the cached scalar plan')
  end

  def lifecycle(database, session)
    statement = session.prepare('SELECT $1::i64 + 7 AS value, $1::i64 AS value')
    lifetime = session.lifetime_cost
    cursor = session.query_prepared(statement, 1)
    assert(cursor.is_a?(JedNative::ScalarRows), 'eligible query missed scalar cursor')
    assert(cursor.cost.zero? && session.lifetime_cost == lifetime, 'unread scalar cursor performed work')
    assert(database.instance_variable_get(:@pins).key?(cursor.object_id), 'scalar cursor has no snapshot pin')
    cursor.close
    cursor.close
    assert(cursor.closed? && cursor.cost.zero? && session.lifetime_cost == lifetime, 'unread close performed work')
    assert(!database.instance_variable_get(:@pins).key?(cursor.object_id), 'unread close retained a snapshot pin')
    begin
      cursor.next
      raise 'closed scalar cursor did not signal EOF'
    rescue StopIteration
      # Public next signals exhaustion, including after an unread close.
    end

    expected = outcome(session, statement, [1])
    cursor = session.query_prepared(statement, 1)
    cursor.columns[0].replace('changed')
    cursor.column_types[0].replace('changed')
    row = cursor.next
    assert(row == [8, 1] && row['value'] == 8, 'public metadata changed scalar row lookup')
    row[0] = 123
    cursor.close
    assert(outcome(session, statement, [1]) == expected, 'returned row or metadata mutated a cached plan')

    first = session.query_prepared(statement, 1)
    second = session.query_prepared(statement, 5)
    version = database.version
    session.execute('CREATE TABLE scalar_snapshot_marker(id i32)')
    assert(database.oldest_live_txid == version, 'unread scalar cursor lost its captured generation')
    lifetime = session.lifetime_cost
    assert(second.rows == [[12, 5]] && first.rows == [[8, 1]], 'outstanding scalar cursors shared parameters')
    assert(session.lifetime_cost - lifetime == first.cost + second.cost, 'scalar cursors overwrote shared lifetime cost')
    assert(database.oldest_live_txid == database.version, 'scalar drain retained the old generation')

    cursor = session.query_prepared(statement, 1)
    session.max_cost = session.lifetime_max_cost = session.max_query_memory_bytes = session.max_scalar_bytes = 1
    assert(cursor.rows == [[8, 1]], 'later settings changed an open scalar cursor allowance')
    session.max_cost = session.lifetime_max_cost = session.max_query_memory_bytes = session.max_scalar_bytes = 0

    cursor = session.query('SELECT 1 / 0')
    session.begin
    error('22012') { cursor.next }
    assert(session.execute('SELECT 1').rows == [[1]], 'idle cursor failure aborted a later transaction')
    block_cursor = session.query_prepared(statement, 1)
    assert(!block_cursor.is_a?(JedNative::ScalarRows) && block_cursor.block_bound?, 'transaction used scalar cursor')
    session.rollback
    assert(block_cursor.closed?, 'rollback retained a transaction cursor')

    assert(database.query('SELECT 1 + 2').rows == [[3]], 'database convenience cursor lost its owning session')
    reader = database.read_session
    cursor = reader.query('SELECT 1 + 2')
    assert(cursor.is_a?(JedNative::ScalarRows) && cursor.rows == [[3]], 'read-only session missed scalar cursor')
    reader.close
    closing = database.session
    cursor = closing.query('SELECT 1 + 2')
    closing.close
    assert(cursor.closed? && cursor.cost.zero?, 'session close evaluated or retained an unread scalar cursor')
    error('55000') { closing.query('SELECT 1') }
  end

  def budgets(database)
    statement = database.prepare('SELECT abs($1::i64) + 7')
    session = database.session
    total = outcome(session, statement, [-9])[4]
    session.close
    %i[max_cost lifetime_max_cost].each do |setting|
      (1..total + 1).each do |limit|
        results = [false, true].map do |ordinary|
          session = database.session
          session.public_send("#{setting}=", limit)
          result = outcome(session, statement, [-9], ordinary: ordinary)
          session.close
          result
        end
        assert(results[0] == results[1], "#{setting}=#{limit} changed scalar failure prefix: #{results.inspect}")
      end
    end

    [false, true].each do |ordinary|
      session = database.session
      session.lifetime_max_cost = total + 1
      first = cursor_for(session, statement, [-9], ordinary: ordinary)
      second = cursor_for(session, statement, [-2], ordinary: ordinary)
      assert(first.rows == [[16]], 'first cursor did not fit its lifetime allowance')
      exception = error('54P02') { second.next }
      assert(second.closed? && second.cost == 1 && session.lifetime_cost == total + 1, 'suspended cursor did not observe shared lifetime work')
      admission = cursor_for(session, statement, [-3], ordinary: ordinary)
      error('54P02') { admission.next }
      assert(admission.closed? && admission.cost.zero? && session.lifetime_cost == total + 1, 'exhausted admission accrued work')
      assert(exception.message.include?('cost'), 'lifetime failure lost its diagnostic')
      session.close
    end

    [false, true].each do |ordinary|
      session = database.session
      session.lifetime_max_cost = 1
      cursors = ['SELECT 1 + 2', 'SELECT 1 / 0 WHERE FALSE'].map do |sql|
        cursor_for(session, session.prepare(sql), [], ordinary: ordinary)
      end
      error('54P02') { session.execute('SELECT 1 + 2') }
      spent = session.lifetime_cost
      session.lifetime_max_cost = 0
      cursors.each do |cursor|
        error('54P02') { cursor.next }
        assert(cursor.closed? && cursor.cost.zero? && session.lifetime_cost == spent,
          'first pull ignored captured ceiling or live exhaustion before a zero-work predicate')
      end
      session.close
    end
  end

  def fallbacks(session)
    session.max_query_memory_bytes = 1_048_576
    cursor = session.query('SELECT 1 + 2')
    assert(!cursor.is_a?(JedNative::ScalarRows) && cursor.rows == [[3]], 'finite query memory bypassed ordinary accounting')
    session.max_query_memory_bytes = 0
    [
      'SELECT 1 + 2 ORDER BY 1', 'SELECT 1 + 2 LIMIT 1', 'SELECT DISTINCT 1 + 2',
      'SELECT (SELECT 1)', 'SELECT count(*)', 'SELECT row_number() OVER ()',
      'SELECT CASE WHEN TRUE THEN 1 ELSE 2 END WHERE TRUE', 'SELECT 1 BETWEEN 0 AND 2 WHERE TRUE',
      "SELECT 'abc' ~ 'b'", 'SELECT ARRAY[1, 2]', "SELECT concat('a', 'b')",
      "SELECT '2001-02-03'::date", 'SELECT current_date'
    ].each do |sql|
      cursor = session.query(sql)
      assert(!cursor.is_a?(JedNative::ScalarRows), "unsupported shape used scalar cursor: #{sql}")
      cursor.close
    end
    [
      ["SELECT CASE WHEN TRUE THEN 'invalid'::i64 ELSE 1 END WHERE 1 / 0 = 1", '22012'],
      ["SELECT CASE WHEN TRUE THEN 'invalid'::i64 ELSE 1 END WHERE FALSE", '22P02']
    ].each do |sql, state|
      lifetime = session.lifetime_cost
      cursor = session.query(sql)
      assert(!cursor.is_a?(JedNative::ScalarRows) && cursor.cost.zero?, 'conditional WHERE skipped the general error order')
      error(state) { cursor.next }
      assert(cursor.closed? && cursor.cost == session.lifetime_cost - lifetime, 'conditional WHERE failure lost work or its cursor pin')
    end

    statement = session.prepare('SELECT abs($1::i64)')
    outcome(session, statement, [-1])
    session.revoke(:execute, 'abs')
    error('42804') { session.query_prepared(statement, 'bad') }
    error('42501') { session.query_prepared(statement, -1) }
    session.reset_privileges
    assert(outcome(session, statement, [-1]).first == :rows, 'privilege reset failed to restore scalar admission')

    session.timezone = 'UTC'
    cursor = session.query("SELECT '2001-02-03 00:00:00'::timestamptz")
    session.timezone = '+02:00'
    assert(!cursor.is_a?(JedNative::ScalarRows), 'temporal context used scalar cursor')
    frozen_value = cursor.rows[0][0]
    assert(frozen_value != session.query("SELECT '2001-02-03 00:00:00'::timestamptz").rows[0][0], 'fallback lost captured timezone')
    session.timezone = 'UTC'
  end

  def extension_budgets(database)
    [
      ['SELECT CASE WHEN $1::boolean THEN 1 + 2 ELSE 4 END', [true]],
      ['SELECT $1::i64 BETWEEN 1 AND 3', [2]],
      ['SELECT repeat($1::text, 3)', ['ab']],
      ['SELECT coalesce($1::i16, $2::i32, $3::i64)', [nil, nil, 2**63 - 1]],
      ['SELECT nullif($1::i16, $2::i32)', [-32768, 2**31 - 1]],
      ['SELECT greatest($1::i16, $2::i32, $3::i64)', [-32768, 2**31 - 1, 2**63 - 1]],
      ['SELECT least($1::i16, $2::i32, $3::i64)', [nil, 2**31 - 1, -(2**63)]],
      ['SELECT nullif(NULL::i64, 1 / $1::i64)', [0]],
      ['SELECT greatest(NULL::i64, 1 / $1::i64)', [0]],
      ['SELECT least(NULL::i64, 1 / $1::i64)', [0]]
    ].each do |sql, params|
      session = database.session
      statement = session.prepare(sql)
      total = outcome(session, statement, params)[4]
      session.close
      %i[max_cost lifetime_max_cost].each do |setting|
        (1..total + 1).each do |limit|
          results = [false, true].map do |ordinary|
            session = database.session
            session.public_send("#{setting}=", limit)
            result = outcome(session, statement, params, ordinary: ordinary)
            session.close
            result
          end
          assert(results[0] == results[1], "#{setting}=#{limit} changed #{sql} failure prefix: #{results.inspect}")
        end
      end
    end
    [1, 5, 6, 7, 11, 12, 13].each do |limit|
      ['SELECT repeat($1::text, 3)', 'SELECT repeat($1::text, 3), repeat($1::text, 3)'].each do |sql|
        results = [false, true].map do |ordinary|
          session = database.session(max_scalar_bytes: limit)
          result = outcome(session, session.prepare(sql), ['ab'], ordinary: ordinary)
          session.close
          result
        end
        assert(results[0] == results[1], "max_scalar_bytes=#{limit} changed #{sql} failure prefix: #{results.inspect}")
      end
    end
    [false, true].each do |ordinary|
      session = database.session(max_scalar_bytes: 6)
      statement = session.prepare('SELECT repeat($1::text, 3)')
      cursor = cursor_for(session, statement, ['ab'], ordinary: ordinary)
      session.max_scalar_bytes = 1
      assert(cursor.rows == [['ababab']], 'later scalar cap changed an admitted cursor allowance')
      cursor = cursor_for(session, statement, ['ab'], ordinary: ordinary)
      session.max_scalar_bytes = 6
      error('54P04') { cursor.next }
      assert(cursor.closed?, 'scalar cap failure retained a cursor')
      session.max_scalar_bytes = session.max_cost = 1
      result = outcome(session, statement, ['ab'], ordinary: ordinary)
      assert(result[0, 3] == [:error, :pull, '54P01'], 'scalar allowance changed cost-limit error precedence')
      session.close
    end
  end

  def storage_guards(database)
    session = database.session
    storage = session.engine.instance_variable_get(:@storage)
    [[:@service_epoch, '08006'], [:@poisoned, '58030'], [:@closed, '55000']].each do |field, state|
      [false, true].each do |ordinary|
        cursor = cursor_for(session, session.prepare('SELECT 1 + 2'), [], ordinary: ordinary)
        original = storage.instance_variable_get(field)
        begin
          storage.instance_variable_set(field, field == :@service_epoch ? original + 1 : true)
          error(state) { cursor.next }
          assert(cursor.closed? && cursor.cost.zero?, "#{state} guard performed work or retained the cursor")
          assert(!database.instance_variable_get(:@pins).key?(cursor.object_id), "#{state} guard retained a snapshot pin")
        ensure
          storage.instance_variable_set(field, original)
          cursor.close
        end
      end
    end
    session.close
  end

  def host_fallback
    calls = []
    environment = JedNative::Environment.new.register_function(name: 'scalar_probe', args: ['i64'], returns: 'i64', volatility: :immutable, cost: 3) do |value|
      calls << value
      value + 1
    end
    JedNative.memory(environment: environment) do |database|
      session = database.session
      cursor = session.query('SELECT scalar_probe($1::i64)', 8)
      assert(!cursor.is_a?(JedNative::ScalarRows) && calls.empty?, 'host function was evaluated or admitted during construction')
      cursor.close
      assert(calls.empty?, 'unread close invoked a host function')
      cursor = session.query('SELECT scalar_probe($1::i64)', 9)
      assert(cursor.rows == [[10]] && calls == [9], 'host fallback lost lazy evaluation')
      session.close
    end
  end

  def reentrant_construction
    pending, session = [], nil
    environment = JedNative::Environment.new.register_function(name: 'capture_scalar', args: ['i64'], returns: 'i64', volatility: :immutable, cost: 3) do |value|
      pending << session.query('SELECT abs($1::i64) + 7 AS value', value)
      value
    end
    JedNative.memory(environment: environment) do |database|
      session = database.session
      statement = session.prepare('SELECT capture_scalar($1::i64) + $2::i64')
      lifetime = session.lifetime_cost
      first = session.execute_prepared(statement, -9, 30)
      second = session.execute_prepared(statement, -2, 40)
      assert(first.rows == [[21]] && second.rows == [[38]], 'nested cursor construction changed an active host call')
      assert(session.lifetime_cost - lifetime == first.cost + second.cost, 'nested construction accrued work in its parent statement')
      assert(pending.all? { |cursor| cursor.is_a?(JedNative::ScalarRows) && cursor.cost.zero? }, 'nested cursor inherited active evaluation state')
      lifetime = session.lifetime_cost
      assert(pending[1].rows == [[9]] && pending[0].rows == [[16]], 'nested cursor inherited subsequent host parameters')
      assert(session.lifetime_cost - lifetime == pending.sum(&:cost), 'nested cursor lost lifetime accounting after its parent returned')
      session.close
    end
  end

  def integer_fold_callbacks
    calls = []
    environment = JedNative::Environment.new.register_function(name: 'fold_probe', args: ['i64'], returns: 'i64', volatility: :immutable, cost: 3) do |value|
      calls << value
      raise JedNative::Error.new('22023', 'fold probe failed') if value == 99
      value
    end
    JedNative.memory(environment: environment) do |database|
      session = database.session
      [
        ['coalesce(7::i64, fold_probe(99::i64))', 7, []],
        ['coalesce(NULL::i64, fold_probe(2::i64), fold_probe(99::i64))', 2, [2]],
        ['greatest(NULL::i64, fold_probe(1::i64), fold_probe(2::i64))', 2, [1, 2]],
        ['least(NULL::i64, fold_probe(1::i64), fold_probe(2::i64))', 1, [1, 2]],
        ['nullif(NULL::i64, fold_probe(3::i64))', nil, [3]],
        ['nullif(NULL::i64, fold_probe(99::i64))', :error, [99]],
        ['least(NULL::i64, fold_probe(99::i64))', :error, [99]],
        ['greatest(fold_probe(1::i64), NULL::i64, fold_probe(99::i64), fold_probe(2::i64))', :error, [1, 99]]
      ].each do |expression, expected, expected_calls|
        calls.clear
        lifetime = session.lifetime_cost
        cursor = session.query("SELECT #{expression}")
        assert(!cursor.is_a?(JedNative::ScalarRows) && calls.empty?, 'integer fold bypassed lazy host-function fallback')
        if expected == :error
          error('22023') { cursor.next }
        else
          assert(cursor.rows == [[expected]], 'integer fold changed a host-call result')
        end
        assert(calls == expected_calls, "integer fold changed argument evaluation order: #{expression}")
        assert(cursor.closed? && session.lifetime_cost - lifetime == cursor.cost, 'integer fold lost host-call work or retained its cursor')
      end
      session.close
    end
  end

  def check(database)
    session = database.session
    equivalence(session)
    conditional_and_text(session)
    lifecycle(database, session)
    fallbacks(session)
    session.close
    budgets(database)
    extension_budgets(database)
  end

  JedNative.memory { |database| check(database) }
  Dir.mktmpdir('jed-ruby-scalar-cursor') do |directory|
    JedNative.create(File.join(directory, 'db'), locking: :exclusive) do |database|
      check(database)
      storage_guards(database)
    end
  end
  host_fallback
  reentrant_construction
  integer_fold_callbacks
  puts 'ruby-native scalar cursor: ordinary parity, lazy failures, budgets, ownership and lifecycle passed'
end
