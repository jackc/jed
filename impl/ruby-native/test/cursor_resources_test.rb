# frozen_string_literal: true

require_relative '../lib/jed_native'

# Collector ownership and captured budgets are host-API contracts; the shared
# corpus drains cursors externally and checks their operator thresholds.
module NativeCursorResourcesTests
  module_function

  def assert(value, message) = (raise(message) unless value)

  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => failure
    assert(failure.sqlstate == code, "expected #{code}, got #{failure.sqlstate}")
  end

  def collect(session, sql, bytes)
    session.max_query_memory_bytes = bytes
    session.query(sql).rows
  end

  def run
    JedNative.memory do |database|
      session = database.session
      session.execute('CREATE TABLE cursor_memory (id i32 PRIMARY KEY)')
      session.execute('INSERT INTO cursor_memory SELECT g FROM generate_series(1, 100) g')
      sql = 'SELECT id FROM cursor_memory LIMIT 1000'

      # A streaming scan retains no result rows. Host-owned external collection
      # is outside the account; Rows#rows owns and admits 100 * (ROW + VALUE).
      session.max_query_memory_bytes = 1
      assert(session.query(sql).each.to_a.length == 100, 'external collection retained an engine result buffer')
      assert(collect(session, sql, 6_400).length == 100, 'collector rejected its exact allowance')
      error('54P05') { collect(session, sql, 6_399) }
      assert(database.instance_variable_get(:@pins).empty?, 'failed collector retained a snapshot pin')
      assert((session.instance_variable_get(:@cursors) || {}).empty?, 'failed collector remained registered')

      # A materialized function relation stays live while the lazy drive emits
      # it: 192 bytes of input plus 192 bytes in the engine-owned collector.
      function_sql = 'SELECT g FROM generate_series(1, 3) g'
      session.max_query_memory_bytes = 192
      assert(session.query(function_sql).each.to_a == [[1], [2], [3]], 'external drain lost its relation allowance')
      assert(collect(session, function_sql, 384) == [[1], [2], [3]], 'collector miscounted retained relation rows')
      error('54P05') { collect(session, function_sql, 383) }

      session.max_query_memory_bytes = 6_400
      old = session.query(sql)
      session.max_query_memory_bytes = 1
      error('54P05') { session.query(sql).rows }
      assert(old.rows.length == 100, 'setting change replaced an existing cursor budget')

      # A collection failure belongs to the writer's statement and aborts its
      # block, even though execution occurs on the cursor's captured engine.
      session.max_query_memory_bytes = 6_399
      session.begin
      error('54P05') { session.query(sql).rows }
      error('25P02') { session.execute('SELECT 1') }
      session.rollback
      session.max_query_memory_bytes = 0
      assert(session.query(sql).rows.length == 100, 'rollback failed to restore usable cursor execution')
      session.close
    end
    calls = []
    caller_thread = Thread.current
    environment = JedNative::Environment.new.register_function(name: 'cursor_depth_probe', args: ['i32'], returns: 'i32', cost: 1) do |value|
      assert(Thread.current.equal?(caller_thread), 'deep cursor callback changed Ruby threads')
      calls << value
      value
    end
    JedNative.memory(environment: environment) do |database|
      session = database.session
      session.execute('CREATE TABLE deep_cursor (id i32 PRIMARY KEY)')
      session.execute('INSERT INTO deep_cursor VALUES (1), (2), (3)')
      source = ('(' * 48) + 'deep_cursor a JOIN deep_cursor b ON a.id = b.id' + (')' * 48)
      cursor = session.query("SELECT cursor_depth_probe(a.id) FROM #{source}")
      assert(calls.empty?, 'deep source evaluated a projection before its first pull')
      assert(cursor.next == [1] && calls == [1], 'deep source lost the root cursor suspension')
      cursor.close
      assert(calls == [1], 'closing a deep cursor evaluated unread projections')
      assert(database.instance_variable_get(:@pins).empty?, 'closing a deep cursor retained a snapshot pin')
      assert(session.query('SELECT 1').rows == [[1]], 'deep cursor close left traversal state behind')
      session.close
    end
    puts 'ruby-native cursor resources: external/owned collectors, exact thresholds, captured budgets and failed-block cleanup passed'
  end
end

NativeCursorResourcesTests.run
