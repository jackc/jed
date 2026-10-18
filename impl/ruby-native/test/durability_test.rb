# frozen_string_literal: true
require 'tmpdir'
require_relative '../lib/jed_native'

def assert(condition, message)
  raise message unless condition
end
Dir.mktmpdir('jed-ruby-durability') do |directory|
  path = File.join(directory, 'database')
  database = JedNative.create(path, page_size: 512, synchronous: :off)
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
