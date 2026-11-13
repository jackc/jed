# frozen_string_literal: true
require 'tmpdir'
require_relative '../lib/jed_native'

module RubySerializationTest
  module_function
  def assert(value, message)
    raise message unless value
  end
  def error(code)
    yield
    raise "expected #{code}"
  rescue JedNative::Error => exception
    raise "expected #{code}, got #{exception.sqlstate}" unless exception.sqlstate == code
  end
  def rows(db) = db.query('SELECT * FROM t ORDER BY id').rows
  def verify(db)
    before = rows(db)
    version = db.current_logical_version
    image = db.serialize
    codec = JedNative::FileFormat.new
    meta = codec.select_meta(image, db.page_size)
    assert(meta[:txid] == version, 'export version')
    assert(meta[:free_head].zero?, 'export free list')
    assert(meta[:page_count] * db.page_size == image.bytesize, 'export extent')
    copy = JedNative.deserialize(image)
    image.replace('caller owns the buffer')
    assert(rows(copy) == before, 'import owns its buffer')
    assert(copy.page_size == db.page_size, 'import page size')
    assert(copy.current_logical_version == version, 'import version')
    copy.execute("INSERT INTO t VALUES (99, 'independent', 1.25)")
    assert(rows(db) == before, 'import writes are independent')
    JedNative::Database.deserialize(copy.serialize) { |roundtrip| assert(rows(roundtrip) == rows(copy), 'second roundtrip') }
  ensure
    copy&.close
  end
  def seed(db)
    db.execute('CREATE TABLE t (id i32 PRIMARY KEY, v text, n numeric(8,2))')
    db.execute("INSERT INTO t VALUES (1, repeat('abc', 4000), 123.45), (2, 'small', NULL)")
    db.execute('CREATE INDEX by_n ON t(n)')
  end
  def run
    JedNative.memory(page_size: 1024) do |db|
      seed(db)
      verify(db)
      db.update do |session|
        session.execute('DELETE FROM t')
        JedNative.deserialize(db.serialize) { |copy| assert(rows(copy).length == 2, 'export ignores pending writes') }
      end
      db.attach('aux')
      db.execute('CREATE TABLE aux.a (id i32)')
      db.execute('INSERT INTO aux.a VALUES (7)')
      JedNative.deserialize(db.serialize('aux')) do |copy|
        assert(copy.query('SELECT * FROM a').rows == [[7]], 'attachment export')
        assert(copy.version == 2, 'attachment preserves its own committed version')
      end
      db.detach('aux')
      db.attach('aux')
      JedNative.deserialize(db.serialize('aux')) { |copy| assert(copy.version.zero?, 'reattached memory database starts at version zero') }
      error('42704') { db.serialize('temp') }
      error('42704') { db.serialize('missing') }
    end
    Dir.mktmpdir('ruby-serialization') do |dir|
      path = File.join(dir, 'db')
      db = JedNative.create(path, page_size: 1024, locking: :exclusive)
      seed(db)
      db.close
      db = JedNative.open(path, locking: :exclusive)
      verify(db)
      JedNative.deserialize(File.binread(path)) { |copy| assert(rows(copy) == rows(db), 'raw COW image import') }
      db.close
      db = JedNative.create(File.join(dir, 'sealed'), credential: JedNative::Credential.key('k' * 32), locking: :exclusive)
      error('0A000') { db.serialize }
      error('55000') { JedNative.deserialize(File.binread(File.join(dir, 'sealed'))) }
      db.close
    end
    error('XX001') { JedNative.deserialize('broken') }
    puts 'ruby-native serialization: owned images, versions, pending writes, attachments, disk COW and encryption boundaries passed'
  end
end
RubySerializationTest.run
