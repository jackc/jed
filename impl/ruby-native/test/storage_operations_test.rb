# frozen_string_literal: true
require 'tmpdir'
require_relative '../lib/jed_native'

def assert(condition, message)
  raise message unless condition
end

def expect_state(state)
  yield
  raise "expected #{state}"
rescue JedNative::Error => error
  raise unless error.sqlstate == state
end

Dir.mktmpdir('jed-ruby-operations') do |directory|
  db = JedNative.memory
  db.execute('CREATE TABLE t (id i64 PRIMARY KEY, v text)')
  db.execute("INSERT INTO t SELECT g, repeat('x', 30) FROM generate_series(1, 100) g")
  db.execute('DELETE FROM t WHERE id > 10')
  version = db.version
  reader = db.read_session
  expect_state('55006') { db.compact }
  reader.close
  expect_state('42704') { db.compact('missing') }
  db.compact
  assert(db.version == version + 1, 'memory compaction did not advance version')
  assert(db.query('SELECT count(*), max(id) FROM t').rows == [[10, 10]], 'memory compaction changed rows')
  db.execute("INSERT INTO t VALUES (11, 'tail')")
  destination = File.join(directory, 'memory-copy')
  expect_state('0A000') { db.backup(destination) }
  db.backup(destination, form: :compacted)
  copy = JedNative.open(destination)
  assert(copy.query('SELECT count(*) FROM t').rows == [[11]], 'memory backup changed rows')
  copy.close
  db.close

  path = File.join(directory, 'file')
  db = JedNative.create(path, synchronous: :off)
  db.execute('CREATE TABLE t (id i64 PRIMARY KEY)')
  db.execute('INSERT INTO t VALUES (1)')
  storage = db.engine.instance_variable_get(:@storage)
  storage.define_singleton_method(:durable_sync) { |_file| raise IOError, 'injected barrier failure' }
  expect_state('58030') { db.close }
  assert(storage.instance_variable_get(:@closed), 'failed close retained handle')
  recovered = JedNative.open(path)
  recovered.close
end
puts 'ruby-native storage operations: memory compaction, backup, close failure passed'
