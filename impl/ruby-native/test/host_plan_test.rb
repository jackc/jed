# frozen_string_literal: true

require_relative '../lib/jed_native'

# Callback schedules, cursor ownership, and resolved-plan lifetime are host
# contracts that cannot be expressed by the language-neutral SQL corpus.
module NativeHostPlanTests
  module_function

  def assert(condition, message) = (raise(message) unless condition)

  def error(code)
    yield
    raise "expected SQLSTATE #{code}"
  rescue JedNative::Error => exception
    assert(exception.sqlstate == code, "expected #{code}, got #{exception.sqlstate}: #{exception.message}")
  end

  def outcome(database, sql, params, calls, ordinary:, setting: nil, limit: 0)
    session = database.session
    session.max_query_memory_bytes = 1_048_576 if ordinary
    session.public_send("#{setting}=", limit) if setting
    statement = session.prepare(sql)
    calls.clear
    phase = :construction
    cursor = session.query_prepared(statement, *params)
    assert(cursor.is_a?(JedNative::ScalarRows) != ordinary, 'host plan comparison selected the wrong cursor')
    assert(calls.empty? && cursor.cost.zero?, 'host compilation called a kernel or charged SQL work')
    phase = :pull
    rows = cursor.rows
    [:rows, rows, cursor.columns, cursor.column_types, cursor.cost, session.lifetime_cost, calls.dup]
  rescue JedNative::Error => exception
    [:error, phase, exception.sqlstate, cursor&.cost || 0, session.lifetime_cost, calls.dup]
  ensure
    cursor&.close
    session&.close
  end

  def scalar_lifecycle
    calls, offset = [], 0
    environment = JedNative::Environment.new
    %i[immutable stable volatile].each do |volatility|
      environment = environment.register_function(name: "probe_#{volatility}", args: ['i64'], returns: 'i64',
        volatility: volatility, cost: 3) do |value|
        calls << [volatility, value]
        value + offset
      end
    end
    JedNative.memory(environment: environment) do |database|
      session = database.session
      %i[immutable stable volatile].each do |volatility|
        statement = session.prepare("SELECT probe_#{volatility}($1) AS answer")
        calls.clear
        unread = session.query_prepared(statement, 2)
        assert(unread.is_a?(JedNative::ScalarRows), 'scalar host call missed lightweight cursor')
        unread.close
        assert(calls.empty? && unread.cost.zero?, 'closing an unread host cursor invoked its callback')
        first = session.query_prepared(statement, 2)
        second = session.query_prepared(statement, 4)
        offset = 10
        assert(second.rows == [[14]] && first.rows == [[12]], 'host plans shared parameters or folded callback results')
        offset = 20
        assert(session.query_prepared(statement, 2).rows == [[22]], 'cached host plan reused an earlier result')
        assert(session.query_prepared(statement, nil).rows == [[nil]], 'strict host NULL changed')
        assert(calls == [[volatility, 4], [volatility, 2], [volatility, 2]], 'cached host plan changed callback count/order')
        session.revoke(:execute, "probe_#{volatility}")
        error('42501') { session.query_prepared(statement, 3) }
        session.reset_privileges
      end
      session.close
    end
  end

  def failures_and_budgets
    calls = []
    environment = JedNative::Environment.new
      .register_function(name: 'probe', args: ['i64'], returns: 'i64', cost: 3) do |value|
        calls << value
        raise JedNative::Error.new('22023', 'host probe failed') if value == 99
        value
      end
    environment = environment.register_function(name: 'width_probe', args: ['i16'], returns: 'i16', cost: 2) do |value|
      calls << value
      value == 1 ? 'wrong kind' : value == 2 ? 32_768 : value
    end
    queries = [
      ['SELECT probe($1)', [7]],
      ['SELECT probe($1)', [nil]],
      ['SELECT probe($1)', [99]],
      ['SELECT probe(1), probe($1), probe(2)', [99]],
      ['SELECT probe(1) + probe($1)', [2]],
      ['SELECT coalesce(1::i64, probe($1))', [99]],
      ['SELECT CASE WHEN $1::boolean THEN probe(99) ELSE probe(3) END', [false]],
      ['SELECT width_probe($1)', [1]],
      ['SELECT width_probe($1)', [2]],
      ['SELECT width_probe($1)', [32_768]]
    ]
    JedNative.memory(environment: environment) do |database|
      queries.each do |sql, params|
        [nil, :max_cost, :lifetime_max_cost].each do |setting|
          (setting ? (1..11) : [0]).each do |limit|
            fast = outcome(database, sql, params, calls, ordinary: false, setting: setting, limit: limit)
            ordinary = outcome(database, sql, params, calls, ordinary: true, setting: setting, limit: limit)
            assert(fast == ordinary, "host plan changed #{setting}=#{limit} #{sql}: #{fast.inspect} != #{ordinary.inspect}")
          end
        end
      end
    end
  end

  def ownership_and_reentry
    returned, calls, session = +'shared', [], nil
    environment = JedNative::Environment.new
      .register_function(name: 'text_probe', args: ['text'], returns: 'text') do |value|
        calls << value.dup
        value.replace('host changed input')
        returned
      end
    environment = environment.register_function(name: 'nested_probe', args: ['i64'], returns: 'i64', cost: 3) do |value|
      session.query('SELECT $1::i64 + 1', value).rows.first.first
    end
    JedNative.memory(environment: environment) do |database|
      session = database.session
      input = +'original'
      statement = session.prepare('SELECT text_probe($1)')
      cursor = session.query_prepared(statement, input)
      input.replace('caller changed input')
      row = cursor.next
      returned.replace('later')
      assert(calls == ['original'] && row == ['shared'], 'host call lost input or output isolation')
      row.first.replace('caller changed output')
      cursor.close
      assert(session.query_prepared(statement, 'another').rows == [['later']], 'returned row mutated the callback or cached plan')
      lifetime = session.lifetime_cost
      cursor = session.query('SELECT nested_probe($1) + $2::i64', 7, 20)
      result = cursor.rows
      assert(result == [[28]] && cursor.cost == 6, 'reentrant query changed parent host bindings or cost')
      assert(session.lifetime_cost - lifetime == 9, 'reentrant host query lost nested lifetime work')
      session.close
    end
  end

  def read_plan_lifetime
    calls = []
    environment = JedNative::Environment.new
      .register_function(name: 'read_probe', args: ['i64'], returns: 'i64') { |value| calls << value; value }
      .register_function(name: 'time_probe', args: ['timestamptz'], returns: 'timestamptz') { |value| value }
    JedNative.memory(environment: environment) do |database|
      session = database.session
      session.execute_script('CREATE TABLE host_plan_rows(id i64 PRIMARY KEY); INSERT INTO host_plan_rows VALUES(1),(2)')
      statement = session.prepare('SELECT read_probe(id) FROM host_plan_rows WHERE id > $1::i64')
      assert(session.query_prepared(statement, 0).rows == [[1], [2]], 'initial host read plan failed')
      cache = session.engine.instance_variable_get(:@native_read_plans)
      first = cache.values.find { |plan| plan.expressions.any? { |expression| expression.kind == :host_call } }
      assert(first, 'resolved host read plan was not retained')
      calls.clear
      assert(session.query_prepared(statement, 1).rows == [[2]] && calls == [2], 'reused host read plan lost the parameter or callback')
      assert(cache.values.any? { |plan| plan.equal?(first) }, 'unchanged host read plan was rebuilt')
      session.execute('INSERT INTO host_plan_rows VALUES(3)')
      assert(session.query_prepared(statement, 1).rows == [[2], [3]], 'host read plan retained an older table snapshot')
      assert(cache.values.none? { |plan| plan.equal?(first) }, 'host read plan survived a table replacement')

      statement = session.prepare("SELECT time_probe('2001-02-03 00:00:00'::timestamptz) FROM host_plan_rows WHERE id = 1")
      session.timezone = 'UTC'
      original = session.query_prepared(statement).rows.first.first
      pending = session.query_prepared(statement)
      session.timezone = '+02:00'
      changed = session.query_prepared(statement).rows.first.first
      assert(original != changed && pending.rows == [[original]], 'cached host arguments lost live or captured timezone context')
      session.close
    end

    # A statement can be executed on another database; the registry and return
    # metadata belong to that handle, never to its parsed SQL object.
    text_environment = JedNative::Environment.new.register_function(name: 'read_probe', args: ['i64'], returns: 'text') { |value| value.to_s }
    databases = [environment, text_environment].map { |env| JedNative.memory(environment: env) }
    statement = databases.first.prepare('SELECT read_probe($1::i64)')
    assert(databases.first.query_prepared(statement, 4).rows == [[4]], 'first handle host signature changed')
    cursor = databases.last.query_prepared(statement, 4)
    assert(cursor.column_types == ['text'] && cursor.rows == [['4']], 'prepared host plan crossed environment identities')
  ensure
    databases&.each(&:close)
  end

  def builtin_overload_boundaries
    calls = []
    environment = JedNative::Environment.new
    %w[now regexp_replace concat abs].each do |name|
      environment = environment.register_function(name: name, args: ['i64'], returns: 'i64') do |value|
        calls << name
        value
      end
    end
    JedNative.memory(environment: environment) do |database|
      session = database.session
      ["SELECT now()", "SELECT regexp_replace('abc', 'b', 'B')", "SELECT concat('a', 'b')"].each do |sql|
        cursor = session.query(sql)
        assert(!cursor.is_a?(JedNative::ScalarRows), 'host overload admitted an unsupported built-in scalar family')
        cursor.rows
      end
      cursor = session.query('SELECT abs(-4::i64)')
      assert(cursor.is_a?(JedNative::ScalarRows) && cursor.rows == [[4]], 'supported built-in lost precedence over a host signature')
      assert(calls.empty?, 'built-in signature called its host overload')
      session.close
    end
  end

  def run
    scalar_lifecycle
    failures_and_budgets
    ownership_and_reentry
    read_plan_lifetime
    builtin_overload_boundaries
    puts 'ruby-native host plans: lazy callbacks, cached resolution, lifecycle, context, ownership and cost prefixes passed'
  end
end

NativeHostPlanTests.run
