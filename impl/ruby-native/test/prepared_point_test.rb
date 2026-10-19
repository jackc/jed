# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

# The corpus pins SQL answers and costs. These checks cover reuse, a single
# physical descent, lazy snapshot ownership, and guards on the prepared lane.
module NativePreparedPointTests
  module_function

  def assert(value, message)
    raise message unless value
  end

  def calls
    counts = Hash.new(0)
    trace = TracePoint.new(:call) do |event|
      counts[:point] += 1 if event.defined_class == JedNative::PagedRows && event.method_id == :point_slot
      counts[:plan] += 1 if event.defined_class == JedNative::AccessPlanning && event.method_id == :scan_access_plan
    end
    value = nil
    trace.enable { value = yield }
    [value, counts]
  end

  def outcome(session, statement, parameter)
    rows = session.query_prepared(statement, parameter)
    [rows.rows, rows.cost]
  rescue JedNative::Error => error
    [error.sqlstate]
  end

  def check(database, paged:)
    session = database.session
    session.execute('CREATE TABLE t(id i32 PRIMARY KEY, x i32, note text)')
    session.execute("INSERT INTO t VALUES #{(1..80).map { |id| "(#{id}, #{id % 9}, 'note #{id}')" }.join(',')}")
    session.execute("INSERT INTO t VALUES (81, 0, repeat('payload', 2000))")
    session.execute('CREATE INDEX t_x ON t(x)')
    statement = session.prepare('SELECT id, x, note FROM t WHERE id = $1')
    outcome(session, statement, 1)

    rows, counts = calls { session.query_prepared(statement, 42) }
    assert(counts.values.sum.zero? && rows.cost.zero?, 'prepared point executed before its first pull')
    values, counts = calls { rows.rows }
    assert(values == [[42, 6, 'note 42']], 'prepared point returned wrong row')
    assert(counts[:plan].zero?, 'prepared point repeated access-path planning')
    assert(counts[:point] == (paged ? 1 : 0), 'prepared point did not use one physical descent')

    [1, 42, 80, 81, 82, -1, nil, 2**31].each do |parameter|
      fast = outcome(session, statement, parameter)
      session.max_cost = 1_000_000
      ordinary = outcome(session, statement, parameter)
      session.max_cost = 0
      assert(fast == ordinary, "prepared point changed values/cost/error for #{parameter.inspect}: #{fast.inspect} != #{ordinary.inspect}")
    end
    key_only = session.prepare('SELECT id FROM t WHERE id = $1')
    small = outcome(session, key_only, 81)
    session.max_cost = 1_000_000
    assert(small == outcome(session, key_only, 81), 'point inventory charged an unread large value')
    session.max_cost = 0

    old = session.query_prepared(statement, 42)
    session.execute("UPDATE t SET note = 'changed' WHERE id = 42")
    assert(old.rows == [[42, 6, 'note 42']], 'unpulled point cursor lost its captured snapshot')
    assert(outcome(session, statement, 42).first == [[42, 6, 'changed']], 'prepared point retained an old table after publication')

    session.execute('CREATE INDEX t_same_key ON t(id)')
    _, counts = calls { outcome(session, statement, 42) }
    assert(counts[:plan].positive?, 'competing secondary access bypassed the planner')
    session.execute('DROP INDEX t_same_key')
    [
      'SELECT id FROM t WHERE id = $1 ORDER BY x',
      'SELECT id FROM t WHERE id = $1 LIMIT 0',
      'SELECT id FROM t WHERE id = $1 AND x = 6'
    ].each do |sql|
      _, counts = calls { outcome(session, session.prepare(sql), 42) }
      assert(counts[:plan].positive?, "general access shape bypassed the planner: #{sql}")
    end
    session.close
  end

  def run
    JedNative.memory { |database| check(database, paged: false) }
    Dir.mktmpdir('jed-ruby-prepared-point') do |directory|
      JedNative.create(File.join(directory, 'db'), page_size: 512, locking: :exclusive) do |database|
        check(database, paged: true)
      end
    end
    puts 'ruby-native prepared points: one descent, plan reuse, lazy snapshots, invalidation, costs and guarded fallback passed'
  end
end

NativePreparedPointTests.run
