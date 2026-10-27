# frozen_string_literal: true
require 'tmpdir'
require_relative '../lib/jed_native'

def assert(condition, message)
  raise message unless condition
end
Dir.mktmpdir('jed-ruby-durability') do |directory|
  path = File.join(directory, 'database')
  database = JedNative.create(path, page_size: 512, synchronous: :off, flush_interval_ms: 0)
  storage = database.engine.instance_variable_get(:@storage)
  codec = JedNative::FileFormat.new
  floor_txid = storage.meta[:txid]
  barriers = 0
  storage.define_singleton_method(:durable_sync) { |file| barriers += 1; super(file) }
  database.execute('CREATE TABLE t (id i64 PRIMARY KEY, v text)')
  first_slot = storage.meta[:slot]
  database.execute("INSERT INTO t VALUES (1, 'a')")
  assert(storage.meta[:slot] == first_slot, 'relaxed batch overwrote durable floor')
  assert(barriers.zero?, 'small relaxed batch flushed early')
  pending = storage.instance_variable_get(:@durability_pending).dup
  bytes = File.binread(path)
  meta = codec.select_meta(bytes, 512)
  assert(meta[:txid] == storage.meta[:txid], 'cumulative manifest did not validate')
  assert(pending.keys.all? { |page| meta[:manifest].any? { |id, _| id == page } }, 'manifest omits pending pages')
  victim = pending.keys.first
  corrupt = bytes.dup
  corrupt.setbyte(victim * 512 + 20, corrupt.getbyte(victim * 512 + 20) ^ 1)
  fallback = codec.select_meta(corrupt, 512)
  assert(fallback[:txid] == floor_txid, 'damaged batch did not recover durable floor')
  35.times { |i| database.execute("UPDATE t SET v = '#{i}'") }
  assert(barriers.positive? && barriers < 35, 'cap did not batch durable barriers')
  before = barriers
  database.close
  assert(barriers >= before, 'close did not flush')
  reopened = JedNative.open(path)
  assert(reopened.query('SELECT v FROM t').rows == [['34']], 'relaxed close lost data')
  reopened.close
end
puts 'ruby-native durability: cumulative manifests, floor recovery, cap, close passed'

# The time-based flush (validated-cow.md §4.1): with a manual clock, a commit at or past the batch's
# deadline flushes it before planning and starts the next batch; one before it joins the batch.
Dir.mktmpdir('jed-ruby-flush-interval') do |directory|
  clock = JedNative::ManualClock.new(0)
  environment = JedNative::Environment.new(clock: clock)
  path = File.join(directory, 'database')
  database = JedNative.create(path, page_size: 4096, synchronous: :off, environment: environment)
  storage = database.engine.instance_variable_get(:@storage)
  barriers = 0
  storage.define_singleton_method(:durable_sync) { |file| barriers += 1; super(file) }
  database.execute('CREATE TABLE t (id i64 PRIMARY KEY, v text)')
  assert(barriers.zero?, 'first relaxed commit flushed')
  clock.set(500_000)
  database.execute("INSERT INTO t VALUES (1, 'a')")
  assert(barriers.zero?, 'commit before the deadline flushed')
  clock.set(1_000_000)
  database.execute("INSERT INTO t VALUES (2, 'b')")
  assert(barriers == 1, "commit at the deadline did not flush first (#{barriers})")
  assert(!storage.instance_variable_get(:@durability_pending).empty?, 'the deadline commit did not start the next batch')
  assert(storage.instance_variable_get(:@flush_deadline) == 2_000_000, 'the next batch deadline is not reading + interval')
  database.close

  # flush_interval_ms = 0 turns the time-based flush off.
  off = JedNative.create(File.join(directory, 'off'), page_size: 4096, synchronous: :off, flush_interval_ms: 0, environment: environment)
  off_storage = off.engine.instance_variable_get(:@storage)
  off_barriers = 0
  off_storage.define_singleton_method(:durable_sync) { |file| off_barriers += 1; super(file) }
  off.execute('CREATE TABLE t (id i64 PRIMARY KEY)')
  clock.advance(10_000_000)
  off.execute('INSERT INTO t VALUES (1)')
  assert(off_barriers.zero?, 'flush_interval_ms = 0 flushed on time')
  off.close

  # The idle flusher flushes a batch no commit follows, about one interval after it began.
  idle = JedNative.create(File.join(directory, 'idle'), page_size: 4096, synchronous: :off, flush_interval_ms: 50)
  idle_storage = idle.engine.instance_variable_get(:@storage)
  idle.execute('CREATE TABLE t (id i64 PRIMARY KEY)')
  idle.execute('INSERT INTO t VALUES (1)')
  limit = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 5
  sleep 0.02 until idle_storage.instance_variable_get(:@durability_pending).empty? || Process.clock_gettime(Process::CLOCK_MONOTONIC) > limit
  assert(idle_storage.instance_variable_get(:@durability_pending).empty?, 'idle flusher did not flush the batch')
  idle.execute('INSERT INTO t VALUES (2)')
  idle.close
  count = JedNative.open(File.join(directory, 'idle')) { |db| db.query('SELECT count(*) FROM t').rows }
  assert(count == [[2]], "idle reopen lost rows: #{count}")
end
puts 'ruby-native durability: time-based flush, interval off, idle flusher passed'
