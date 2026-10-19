# frozen_string_literal: true

require 'tmpdir'
require_relative '../lib/jed_native'

module NativeStorageSnapshotApiTests
  module_function

  def assert(value, message) = (raise(message) unless value)

  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => caught
    assert(caught.sqlstate == code, "expected #{code}, got #{caught.sqlstate}: #{caught.message}")
  end

  def image_rows(bytes, page_size: 1024, txid: 0)
    assert(bytes.encoding == Encoding::BINARY, 'image is not a binary string')
    2.times do |slot|
      assert(bytes.byteslice(slot * page_size + 12, 8).unpack1('Q>') == txid, 'requested transaction id missing from a meta slot')
    end
    JedNative.from_image(bytes) do |restored|
      assert(restored.page_size == page_size, 'requested image page size was lost')
      error('42P01') { restored.execute('SELECT * FROM temporary_snapshot_test') }
      error('42P01') { restored.execute('SELECT * FROM ref.attached_snapshot_test') }
      restored.execute('SELECT id, value FROM snapshot_test ORDER BY id').rows
    end
  end

  def exercise(database, file: false)
    expected_empty = file ? database.page_size : 3 * database.page_size
    assert(database.storage_bytes == expected_empty, 'empty committed storage measure is wrong')
    database.attach('ref')
    assert(database.storage_bytes('ref') == 3 * 8192, 'empty attachment high-water is wrong')
    error('42704') { database.storage_bytes('missing') }
    error('42704') { database.storage_bytes('temp') }
    database.execute('CREATE TABLE snapshot_test(id i32 PRIMARY KEY, value text)')
    database.execute("INSERT INTO snapshot_test VALUES(1, 'committed')")
    database.execute('CREATE TABLE ref.attached_snapshot_test(id i32)')
    database.execute('INSERT INTO ref.attached_snapshot_test VALUES(7)')
    attachment_bytes = database.storage_bytes('ref')
    before = database.storage_bytes
    session = database.session
    session.execute('CREATE TEMP TABLE temporary_snapshot_test(id i32)')
    session.execute('INSERT INTO temporary_snapshot_test VALUES(9)')
    assert(database.storage_bytes == before, 'temporary changes entered the main committed gauge')
    error('42704') { session.storage_bytes('temp') }

    session.begin
    session.execute("UPDATE snapshot_test SET value = 'pending' WHERE id = 1")
    session.execute('CREATE TABLE pending_snapshot_test(id i32)')
    session.execute('INSERT INTO snapshot_test VALUES($1, $2)', 2, Random.new(17).bytes(6000).unpack1('H*'))
    assert(database.storage_bytes == before && session.storage_bytes == before, 'pending main writes entered the committed gauge')
    assert(database.storage_bytes('ref') == attachment_bytes, 'main pending writes changed attachment storage')
    assert(image_rows(database.to_image) == [[1, 'committed']], 'database image exposed another session pending writes')
    assert(image_rows(session.to_image) == [[1, 'committed']], 'session image exposed its own pending writes')
    assert(session.execute('SELECT value FROM snapshot_test WHERE id=1').rows == [['pending']], 'snapshot serialization changed the working set')
    JedNative.from_image(session.to_image) do |restored|
      error('42P01') { restored.execute('SELECT * FROM pending_snapshot_test') }
    end
    session.rollback
    assert(database.storage_bytes == before, 'rollback changed committed storage')

    pinned = database.read_session
    database.execute("INSERT INTO snapshot_test VALUES(2, 'new commit')")
    assert(image_rows(pinned.to_image) == [[1, 'committed']], 'pinned session image advanced to a later commit')
    current = [[1, 'committed'], [2, 'new commit']]
    assert(image_rows(database.to_image) == current, 'database image did not capture latest committed rows')
    assert(image_rows(session.to_image(page_size: 2048, txid: 123), page_size: 2048, txid: 123) == current,
           'idle session image or explicit image options were incorrect')
    pinned.close
    assert(database.storage_bytes('ref') == attachment_bytes, 'main commit changed attachment measure')
    assert(database.storage_bytes.positive? && (database.storage_bytes % database.page_size).zero?, 'storage gauge is not page based')
    assert(database.storage_bytes < File.size(database.path), 'file gauge includes meta/dead/manifest pages') if file
    error('0A000') { database.to_image(page_size: 0) }
    error('0A000') { database.to_image(page_size: 1000) }
    error('54000') { database.to_image(txid: -1) }
    error('54000') { database.to_image(txid: 1 << 64) }
    # Failed serialization must release its temporary pin.
    database.detach('ref')
    error('42704') { database.storage_bytes('ref') }
    session.close
    error('55000') { session.storage_bytes }
    error('55000') { session.to_image }
    database.close
    error('55000') { database.storage_bytes }
    error('55000') { database.to_image }
  ensure
    pinned&.close
    session&.close
  end

  JedNative.memory(page_size: 1024) { |database| exercise(database) }
  Dir.mktmpdir('jed-ruby-snapshot-api') do |directory|
    path = File.join(directory, 'db')
    JedNative.create(path, page_size: 1024, locking: :exclusive) { |database| exercise(database, file: true) }
    attached_path = File.join(directory, 'attached')
    JedNative.create(attached_path, page_size: 2048, locking: :exclusive) do |attached|
      attached.execute('CREATE TABLE attached_snapshot_test(id i32)')
      attached.execute('INSERT INTO attached_snapshot_test VALUES(1)')
      @attached_bytes = attached.storage_bytes
    end
    JedNative.memory do |database|
      database.attach('disk', attached_path, locking: :exclusive)
      assert(database.storage_bytes('disk') == @attached_bytes, 'file attachment gauge used the main page size or memory measure')
    end
  end
  puts 'ruby-native snapshot API: committed storage gauges, pinned/working image isolation, attachments, metadata and closed handles passed'
end
