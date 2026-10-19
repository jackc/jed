# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

module NativeHostSettingsTests
  module_function

  def assert(value, message) = (raise(message) unless value)

  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => caught
    assert(caught.sqlstate == code, "expected #{code}, got #{caught.sqlstate}: #{caught.message}")
  end

  def defaults
    JedNative.memory do |database|
      database.execute('CREATE TABLE settings_test(x i32)')
      database.execute('INSERT INTO settings_test VALUES(1)')
      existing = database.session
      database.max_cost = 1
      error('54P01') { database.execute('SELECT 1 + 2') }
      assert(existing.execute('SELECT 1 + 2').rows == [[3]], 'database default changed an existing session')
      override = database.session(max_cost: 0)
      assert(override.execute('SELECT 1 + 2').rows == [[3]], 'explicit session option did not override the default')
      override.close
      database.max_cost = 0

      database.allow_ddl = false
      error('42501') { database.execute('CREATE TABLE denied(x i32)') }
      database.allow_temp_ddl = false
      error('42501') { database.execute('CREATE TEMP TABLE denied(x i32)') }
      database.allow_ddl = true
      database.allow_temp_ddl = nil

      database.default_privileges = []
      error('42501') { database.execute('SELECT x FROM settings_test') }
      database.grant(:select, 'settings_test')
      assert(database.execute('SELECT x FROM settings_test').rows == [[1]], 'database grant was not inherited')
      granted = database.session
      database.revoke(:select, 'settings_test')
      error('42501') { database.execute('SELECT x FROM settings_test') }
      assert(granted.execute('SELECT x FROM settings_test').rows == [[1]], 'revocation mutated an existing session')
      granted.revoke(:select, 'settings_test')
      error('42501') { granted.execute('SELECT x FROM settings_test') }
      granted.close
      database.reset_privileges
      error('42501') { database.execute('SELECT x FROM settings_test') }
      database.default_privileges = :all
      assert(existing.execute('SELECT x FROM settings_test').rows == [[1]], 'database privileges leaked into an existing session')

      database.max_query_memory_bytes = 1
      error('54P05') { database.execute('SELECT x FROM settings_test') }
      database.max_query_memory_bytes = 0
      database.max_scalar_bytes = 8
      error('54P04') { database.execute("SELECT repeat('a', 128)") }
      database.max_scalar_bytes = 0
      assert(database.max_scalar_bytes == 64 * 1024 * 1024, 'scalar setter did not restore its default')
      database.max_sql_length = 1
      error('54000') { database.execute('SELECT 1') }
      database.max_sql_length = 0

      database.work_mem = 0
      configured = database.session
      assert(configured.work_mem.zero?, 'setter-only unlimited work_mem became the constructor default')
      configured.close
      configured = database.session(work_mem: 0)
      assert(configured.work_mem == JedNative::WorkMemory::DEFAULT_WORK_MEM, 'explicit constructor zero did not restore default work_mem')
      configured.close
      database.max_temp_storage_bytes = 12345
      configured = database.session
      assert(configured.max_temp_storage_bytes == 12345, 'temporary-storage default was lost')
      configured.close

      utc = existing.execute("SELECT '2000-01-01 00:00:00'::timestamptz").rows[0][0].micros
      database.timezone = '+02:00'
      shifted = database.execute("SELECT '2000-01-01 00:00:00'::timestamptz").rows[0][0].micros
      assert(shifted != utc && existing.timezone == 'UTC', 'timezone default was ignored or changed an existing session')
      database.lifetime_max_cost = 1
      error('54P02') { database.execute('SELECT 1') }
      database.lifetime_max_cost = 0
      existing.close
    end
  end

  def lifetime
    JedNative.memory do |database|
      session = database.session
      session.begin
      session.lifetime_max_cost = 5
      session.execute('SELECT 1 + 2')
      session.rollback
      assert(session.lifetime_cost == 2, 'rollback reset cumulative work')
      session.execute('SELECT 1 + 2')
      error('54P02') { session.execute('SELECT 1 + 2') }
      assert(session.lifetime_cost == 5, 'partial failed work did not accrue')
      error('54P02') { session.execute('SELECT 1') }
      assert(session.lifetime_cost == 5, 'exhausted admission accrued additional work')
      session.lifetime_max_cost = 0
      before = session.lifetime_cost
      rows = session.query('SELECT 1 + 2')
      assert(rows.rows == [[3]] && session.lifetime_cost == before + rows.cost, 'lazy cursor work did not accrue to its session')
      session.max_scalar_bytes = 8
      error('54P04') { session.execute("SELECT repeat('a', 128)") }
      session.close
      error('55000') { session.max_cost = 1 }

      database.execute('CREATE TABLE spent_budget(x i32)')
      session = database.write_session
      session.execute('INSERT INTO spent_budget VALUES(1)')
      session.lifetime_max_cost = session.lifetime_cost + 1
      error('54P02') { session.execute('SELECT 1 + 2') }
      spent = session.lifetime_cost
      session.close
      assert(session.lifetime_cost == spent, 'close refunded cumulative work')
      writer = database.session(lock_timeout_ms: 1)
      writer.execute('INSERT INTO spent_budget VALUES(2)')
      assert(writer.execute('SELECT x FROM spent_budget').rows == [[2]], 'exhausted close failed to roll back and release the writer')
      writer.close
    end
  end

  def storage_gauges
    JedNative.memory(page_size: 1024) do |database|
      assert(database.path.nil? && database.page_size == 1024 && !database.read_only?, 'memory storage identity')
      assert(database.resident_leaves.zero?, 'empty memory database has resident data leaves')
      database.execute('CREATE TABLE gauge_test(id i32 PRIMARY KEY)')
      database.execute('INSERT INTO gauge_test VALUES(1)')
      assert(database.resident_leaves.positive?, 'memory leaf gauge remained empty after a stored row')
    end
    Dir.mktmpdir('jed-ruby-host-settings') do |directory|
      path = File.join(directory, 'db')
      JedNative.create(path, page_size: 1024, locking: :exclusive, cache_bytes: 1024, max_storage_bytes: 65536) do |database|
        assert(database.path == path && database.page_size == 1024 && !database.read_only?, 'file storage identity')
        assert(database.max_storage_bytes == 65536, 'file storage limit getter lost the create option')
        database.path.clear
        assert(database.path == path, 'path getter exposed mutable internal storage identity')
        database.execute('CREATE TABLE gauge_test(id i32 PRIMARY KEY)')
        database.execute('INSERT INTO gauge_test VALUES(1)')
        database.execute('SELECT id FROM gauge_test')
        assert(database.resident_leaves.between?(1, 1), 'file leaf gauge exceeds the one-leaf cache')
      end
      JedNative.open(path, read_only: true, locking: :exclusive) do |database|
        assert(database.read_only?, 'physical read-only gauge')
        session = database.session
        assert(session.read_only?, 'session lost physical read-only state')
        error('25006') { session.begin(true) }
        session.begin
        error('25001') { session.begin(true) }
        assert(session.execute('SELECT id FROM gauge_test').rows == [[1]], 'read-only transaction cannot read')
        session.rollback
        error('25006') { session.execute('INSERT INTO gauge_test VALUES(2)') }
        session.close
      end
    end
  end

  defaults
  lifetime
  storage_gauges
  puts 'ruby-native host settings: effective database defaults, independent sessions, lifetime/scalar controls and storage gauges passed'
end
